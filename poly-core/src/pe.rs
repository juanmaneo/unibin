//! Just enough PE/COFF to identify images and relocate the launcher stub.
//!
//! The Windows loader reads only `e_magic` (`MZ`) at 0 and `e_lfanew` at 0x3C
//! from the DOS header. The packer therefore replaces the launcher's DOS
//! header and stub with the shell prologue and moves the launcher's NT
//! headers, sections and overlay `shift` bytes further into the file. Every
//! field holding a *file offset* is adjusted; RVAs are untouched because the
//! in-memory layout does not change.

use crate::{Error, Result, le16, le32, put32};

pub const MACHINE_AMD64: u16 = 0x8664;
pub const MACHINE_ARM64: u16 = 0xAA64;
const PE32_PLUS: u16 = 0x20B;
const E_LFANEW: usize = 0x3C;
const COFF_LEN: usize = 24; // "PE\0\0" + IMAGE_FILE_HEADER
const SECTION_LEN: usize = 40;
const DEBUG_ENTRY_LEN: usize = 28;
const DIR_SECURITY: usize = 4;
const DIR_DEBUG: usize = 6;

/// Offsets of the interesting header structures inside a PE32+ image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Headers {
    lfanew: usize,
    machine: u16,
    opt: usize,
    sections: usize,
    nsections: usize,
}

/// Locate the NT headers via `e_lfanew` and return `(e_lfanew, machine)`.
pub fn nt_headers(data: &[u8]) -> Option<(usize, u16)> {
    if !data.starts_with(b"MZ") {
        return None;
    }
    let lfanew = le32(data, E_LFANEW)? as usize;
    let sig = data.get(lfanew..lfanew.checked_add(4)?)?;
    (sig == b"PE\0\0").then_some(())?;
    Some((lfanew, le16(data, lfanew + 4)?))
}

fn headers(data: &[u8]) -> Result<Headers> {
    let (lfanew, machine) =
        nt_headers(data).ok_or_else(|| Error::new("not a PE image (no MZ/PE signature)"))?;
    let opt = lfanew + COFF_LEN;
    let opt_len = usize::from(le16(data, lfanew + 20).unwrap_or(0));
    let nsections = usize::from(le16(data, lfanew + 6).unwrap_or(0));
    let sections = opt + opt_len;
    if le16(data, opt) != Some(PE32_PLUS) || data.len() < sections + nsections * SECTION_LEN {
        return Err(Error::new(
            "not a PE32+ image, or its section table is truncated",
        ));
    }
    Ok(Headers {
        lfanew,
        machine,
        opt,
        sections,
        nsections,
    })
}

/// Machine field of a PE image.
pub fn machine(data: &[u8]) -> Result<u16> {
    headers(data).map(|h| h.machine)
}

/// `(VirtualAddress, Size)` of data directory `idx`, if present.
fn data_dir(data: &[u8], h: &Headers, idx: usize) -> Option<(u32, u32)> {
    let count = le32(data, h.opt + 108)? as usize;
    let at = h.opt + 112 + idx * 8;
    (idx < count).then_some(())?;
    Some((le32(data, at)?, le32(data, at + 4)?))
}

/// End of the polyglot's logical content: the start of an appended
/// Authenticode certificate table if the file has been signed, else EOF.
/// Signing tools zero-pad to 8 bytes before the table; the padding is dropped.
pub fn logical_end(data: &[u8]) -> usize {
    let cert = headers(data)
        .ok()
        .and_then(|h| data_dir(data, &h, DIR_SECURITY));
    match cert {
        Some((off, size)) if size > 0 && off as usize + size as usize == data.len() => {
            let mut end = off as usize;
            while end > 0 && off as usize - end < 7 && data[end - 1] == 0 {
                end -= 1;
            }
            end
        }
        _ => data.len(),
    }
}

/// Map an RVA to a file offset using the section table.
fn rva_to_offset(data: &[u8], h: &Headers, rva: u32) -> Option<usize> {
    (0..h.nsections).find_map(|i| {
        let s = h.sections + i * SECTION_LEN;
        let va = le32(data, s + 12)?;
        let raw_size = le32(data, s + 16)?;
        let raw_ptr = le32(data, s + 20)?;
        (rva >= va && rva - va < raw_size).then(|| (raw_ptr + (rva - va)) as usize)
    })
}

fn bump(b: &mut [u8], at: usize, shift: u32) {
    let v = le32(b, at).unwrap_or(0);
    if v != 0 {
        put32(b, at, v + shift);
    }
}

/// A launcher image prepared for placement at file offset `shift`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relocated {
    /// Patched launcher bytes (same length as the input).
    pub image: Vec<u8>,
    /// Where the launcher must be placed; a multiple of its FileAlignment.
    pub shift: usize,
    /// Value for the polyglot's `e_lfanew`.
    pub lfanew: u32,
}

/// Relocate an x86_64 PE32+ launcher so that at least `min_prefix` bytes of
/// shell prologue fit in front of it.
pub fn relocate(launcher: &[u8], min_prefix: usize) -> Result<Relocated> {
    let h = headers(launcher)?;
    if h.machine != MACHINE_AMD64 {
        return Err(Error::new(format!(
            "launcher machine is 0x{:04x}; it must be x86_64 (0x8664), which also runs on Windows on Arm",
            h.machine
        )));
    }
    let section_align = le32(launcher, h.opt + 32).unwrap_or(0);
    let file_align = le32(launcher, h.opt + 36).unwrap_or(0);
    let size_of_headers = le32(launcher, h.opt + 60).unwrap_or(0);
    if section_align < 0x1000
        || !file_align.is_power_of_two()
        || h.lfanew >= size_of_headers as usize
    {
        return Err(Error::new(
            "launcher has low or invalid alignment, or NT headers outside SizeOfHeaders",
        ));
    }
    if data_dir(launcher, &h, DIR_SECURITY).is_some_and(|(_, size)| size != 0) {
        return Err(Error::new(
            "launcher is already Authenticode-signed; sign the packed output instead",
        ));
    }
    let shift32 = min_prefix
        .checked_next_multiple_of(file_align as usize)
        .and_then(|s| u32::try_from(s).ok())
        .ok_or_else(|| Error::new("prologue too large"))?;
    let shift = shift32 as usize;
    let first_va = (0..h.nsections)
        .filter_map(|i| le32(launcher, h.sections + i * SECTION_LEN + 12))
        .min()
        .unwrap_or(u32::MAX);
    if u64::from(size_of_headers) + u64::from(shift32) > u64::from(first_va) {
        return Err(Error::new(format!(
            "prologue ({shift} bytes) does not fit before the launcher's first section (RVA 0x{first_va:x})"
        )));
    }

    let mut img = launcher.to_vec();
    put32(&mut img, h.opt + 60, size_of_headers + shift32);
    put32(&mut img, h.opt + 64, 0); // CheckSum: stale after patching, recomputed by signing tools
    bump(&mut img, h.lfanew + 12, shift32); // PointerToSymbolTable
    for i in 0..h.nsections {
        let s = h.sections + i * SECTION_LEN;
        for field in [20, 24, 28] {
            // PointerToRawData, PointerToRelocations, PointerToLinenumbers
            bump(&mut img, s + field, shift32);
        }
    }
    if let Some((rva, size)) = data_dir(launcher, &h, DIR_DEBUG).filter(|&(rva, _)| rva != 0) {
        let at = rva_to_offset(launcher, &h, rva)
            .filter(|&at| at + size as usize <= launcher.len())
            .ok_or_else(|| Error::new("launcher debug directory lies outside its sections"))?;
        for entry in 0..size as usize / DEBUG_ENTRY_LEN {
            bump(&mut img, at + entry * DEBUG_ENTRY_LEN + 24, shift32); // PointerToRawData
        }
    }
    // h.lfanew < SizeOfHeaders, and SizeOfHeaders + shift <= first_va fits in u32.
    let lfanew = shift32 + h.lfanew as u32;
    Ok(Relocated {
        image: img,
        shift,
        lfanew,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::put16;

    /// Minimal PE32+ image: two sections (.text at RVA 0x1000, .rdata at
    /// 0x2000), FileAlignment 0x200, a one-entry debug directory.
    pub(crate) fn fake_pe(machine: u16) -> Vec<u8> {
        let mut b = vec![0u8; 0x600];
        b[..2].copy_from_slice(b"MZ");
        put32(&mut b, E_LFANEW, 0x80);
        b[0x80..0x84].copy_from_slice(b"PE\0\0");
        put16(&mut b, 0x84, machine);
        put16(&mut b, 0x86, 2); // sections
        put16(&mut b, 0x94, 0xF0); // SizeOfOptionalHeader
        put16(&mut b, 0x96, 0x22);
        let opt = 0x98;
        put16(&mut b, opt, PE32_PLUS);
        put32(&mut b, opt + 32, 0x1000);
        put32(&mut b, opt + 36, 0x200);
        put32(&mut b, opt + 56, 0x3000);
        put32(&mut b, opt + 60, 0x200);
        put32(&mut b, opt + 64, 0x1234);
        put16(&mut b, opt + 68, 3);
        put32(&mut b, opt + 108, 16);
        put32(&mut b, opt + 112 + DIR_DEBUG * 8, 0x2000);
        put32(
            &mut b,
            opt + 112 + DIR_DEBUG * 8 + 4,
            DEBUG_ENTRY_LEN as u32,
        );
        let sec = opt + 0xF0;
        for (i, (name, va, raw)) in [
            (b".text\0\0\0", 0x1000, 0x200),
            (b".rdata\0\0", 0x2000, 0x400),
        ]
        .into_iter()
        .enumerate()
        {
            let s = sec + i * SECTION_LEN;
            b[s..s + 8].copy_from_slice(name);
            put32(&mut b, s + 8, 0x40);
            put32(&mut b, s + 12, va);
            put32(&mut b, s + 16, 0x200);
            put32(&mut b, s + 20, raw);
        }
        b[0x200] = 0xC3; // ret
        put32(&mut b, 0x400 + 16, 4);
        put32(&mut b, 0x400 + 20, 0x201C);
        put32(&mut b, 0x400 + 24, 0x41C);
        b
    }

    fn opt_field(b: &[u8], off: usize) -> u32 {
        le32(b, 0x98 + off).unwrap()
    }

    #[test]
    fn identifies_images() {
        let pe = fake_pe(MACHINE_AMD64);
        assert_eq!(nt_headers(&pe), Some((0x80, MACHINE_AMD64)));
        assert_eq!(machine(&fake_pe(MACHINE_ARM64)), Ok(MACHINE_ARM64));
        assert_eq!(nt_headers(b"MZ"), None);
        assert_eq!(nt_headers(b"\x7fELF"), None);
        let mut bad = pe.clone();
        put32(&mut bad, E_LFANEW, u32::MAX);
        assert_eq!(nt_headers(&bad), None);
        bad = pe.clone();
        bad[0x80] = b'X';
        assert!(machine(&bad).unwrap_err().0.contains("not a PE image"));
        bad = pe.clone();
        put16(&mut bad, 0x98, 0x10B); // PE32
        assert!(machine(&bad).unwrap_err().0.contains("PE32+"));
        assert!(machine(&pe[..0x100]).unwrap_err().0.contains("truncated"));
    }

    #[test]
    fn relocation_shifts_file_offsets() {
        let pe = fake_pe(MACHINE_AMD64);
        let r = relocate(&pe, 0x4A1).unwrap();
        assert_eq!(r.shift, 0x600);
        assert_eq!(r.lfanew, 0x680);
        assert_eq!(r.image.len(), pe.len());
        assert_eq!(opt_field(&r.image, 60), 0x800); // SizeOfHeaders
        assert_eq!(opt_field(&r.image, 64), 0); // CheckSum
        assert_eq!(le32(&r.image, 0x188 + 20), Some(0x800));
        assert_eq!(le32(&r.image, 0x188 + 40 + 20), Some(0xA00));
        assert_eq!(le32(&r.image, 0x188 + 24), Some(0)); // zero stays zero
        assert_eq!(le32(&r.image, 0x400 + 24), Some(0x41C + 0x600));
        assert_eq!(le32(&r.image, 0x400 + 20), Some(0x201C)); // RVA untouched
    }

    #[test]
    fn relocation_without_debug_directory() {
        let mut pe = fake_pe(MACHINE_AMD64);
        put32(&mut pe, 0x98 + 112 + DIR_DEBUG * 8, 0);
        let r = relocate(&pe, 0x200).unwrap();
        assert_eq!(le32(&r.image, 0x400 + 24), Some(0x41C));
        // Fewer data directories than the debug index.
        put32(&mut pe, 0x98 + 108, 2);
        relocate(&pe, 0x200).unwrap();
    }

    #[test]
    fn relocation_errors() {
        let e = relocate(&fake_pe(MACHINE_ARM64), 0x200).unwrap_err();
        assert!(e.0.contains("must be x86_64"), "{e}");
        assert!(relocate(b"nope", 0).unwrap_err().0.contains("not a PE"));

        let mut pe = fake_pe(MACHINE_AMD64);
        put32(&mut pe, 0x98 + 32, 0x200);
        assert!(relocate(&pe, 0x200).unwrap_err().0.contains("alignment"));
        pe = fake_pe(MACHINE_AMD64);
        put32(&mut pe, 0x98 + 36, 0x300);
        assert!(relocate(&pe, 0x200).unwrap_err().0.contains("alignment"));
        pe = fake_pe(MACHINE_AMD64);
        put32(&mut pe, 0x98 + 60, 0x80);
        assert!(
            relocate(&pe, 0x200)
                .unwrap_err()
                .0
                .contains("SizeOfHeaders")
        );

        pe = fake_pe(MACHINE_AMD64);
        put32(&mut pe, 0x98 + 112 + DIR_SECURITY * 8 + 4, 8);
        assert!(relocate(&pe, 0x200).unwrap_err().0.contains("Authenticode"));

        let e = relocate(&fake_pe(MACHINE_AMD64), 0xE01).unwrap_err();
        assert!(e.0.contains("does not fit"), "{e}");
        assert!(relocate(&fake_pe(MACHINE_AMD64), usize::MAX - 0x1000).is_err());

        pe = fake_pe(MACHINE_AMD64);
        put32(&mut pe, 0x98 + 112 + DIR_DEBUG * 8, 0x5000);
        assert!(
            relocate(&pe, 0x200)
                .unwrap_err()
                .0
                .contains("debug directory")
        );
        pe = fake_pe(MACHINE_AMD64);
        put32(&mut pe, 0x98 + 112 + DIR_DEBUG * 8 + 4, 0x10_0000);
        assert!(
            relocate(&pe, 0x200)
                .unwrap_err()
                .0
                .contains("debug directory")
        );
    }

    #[test]
    fn logical_end_strips_certificate() {
        let mut pe = fake_pe(MACHINE_AMD64);
        assert_eq!(logical_end(&pe), pe.len());
        assert_eq!(logical_end(b"not a pe"), 8);
        pe.extend_from_slice(b"ZIPDATA\n!"); // ends at 0x609; signer pads to 0x610
        pe.resize(0x610, 0);
        let cert_at = pe.len();
        pe.extend_from_slice(&[0xAB; 16]);
        put32(&mut pe, 0x98 + 112 + DIR_SECURITY * 8, cert_at as u32);
        put32(&mut pe, 0x98 + 112 + DIR_SECURITY * 8 + 4, 16);
        assert_eq!(logical_end(&pe), 0x609);
        // Certificate table not at EOF: treated as unsigned.
        pe.push(0);
        assert_eq!(logical_end(&pe), pe.len());
    }
}
