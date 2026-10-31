//! Identify a slice's OS and architecture from its header bytes.

use crate::{Error, Result, be32, le16, le32, le64, pe};

const ELFOSABI_NONE: u8 = 0;
const ELFOSABI_LINUX: u8 = 3;
/// `EI_OSABI` value for FreeBSD (`e_ident[7]`).
pub const ELFOSABI_FREEBSD: u8 = 9;
const PT_NOTE: u32 = 4;
const NT_FREEBSD_ABI_TAG: u32 = 1;
const EM_X86_64: u16 = 62;
const EM_AARCH64: u16 = 183;
const CPU_X86_64: u32 = 0x0100_0007;
const CPU_ARM64: u32 = 0x0100_000C;
/// Real fat binaries have a handful of arches; Java class files share the
/// `0xcafebabe` magic but have a version number here.
const MAX_FAT_ARCHES: u32 = 16;

fn elf_arch(machine: u16) -> Result<&'static str> {
    match machine {
        EM_X86_64 => Ok("x86_64"),
        EM_AARCH64 => Ok("aarch64"),
        m => Err(Error::new(format!(
            "unsupported ELF machine {m}; only x86_64 and aarch64"
        ))),
    }
}

fn macho_arch(cpu: u32) -> Option<&'static str> {
    match cpu {
        CPU_X86_64 => Some("x86_64"),
        CPU_ARM64 => Some("aarch64"),
        _ => None,
    }
}

/// zig/lld leave `EI_OSABI` at 0 and brand FreeBSD executables only with a
/// `FreeBSD` ABI-tag note, which the FreeBSD kernel also accepts.
fn has_freebsd_note(b: &[u8]) -> bool {
    let phoff = le64(b, 32).unwrap_or(0) as usize;
    let phentsize = le16(b, 54).unwrap_or(0) as usize;
    let phnum = le16(b, 56).unwrap_or(0) as usize;
    (0..phnum).any(|i| {
        let ph = phoff.saturating_add(i.saturating_mul(phentsize));
        le32(b, ph) == Some(PT_NOTE) && note_segment(b, ph).is_some_and(freebsd_tag)
    })
}

fn note_segment(b: &[u8], ph: usize) -> Option<(&[u8], usize)> {
    let off = le64(b, ph.checked_add(8)?)? as usize;
    let len = le64(b, ph.checked_add(32)?)? as usize;
    let align = if le64(b, ph.checked_add(48)?)? == 8 {
        8
    } else {
        4
    };
    Some((b.get(off..off.checked_add(len)?)?, align))
}

fn freebsd_tag((mut n, align): (&[u8], usize)) -> bool {
    while let (Some(namesz), Some(descsz), Some(kind)) = (le32(n, 0), le32(n, 4), le32(n, 8)) {
        let name_end = 12 + namesz as usize;
        if kind == NT_FREEBSD_ABI_TAG && n.get(12..name_end) == Some(b"FreeBSD\0") {
            return true;
        }
        let next = name_end.next_multiple_of(align) + (descsz as usize).next_multiple_of(align);
        n = n.get(next..).unwrap_or_default();
    }
    false
}

fn sniff_elf(b: &[u8]) -> Result<Vec<String>> {
    if b.get(4..6) != Some(&[2, 1]) {
        return Err(Error::new(
            "only 64-bit little-endian ELF slices are supported",
        ));
    }
    let os = match b[7] {
        ELFOSABI_FREEBSD => "FreeBSD",
        ELFOSABI_NONE if has_freebsd_note(b) => "FreeBSD",
        ELFOSABI_NONE | ELFOSABI_LINUX => "Linux",
        abi => return Err(Error::new(format!("unsupported ELF OS/ABI {abi}"))),
    };
    let arch = elf_arch(le16(b, 18).unwrap_or(0))?;
    Ok(vec![format!("{os}-{arch}")])
}

fn sniff_fat(b: &[u8]) -> Result<Vec<String>> {
    let n = be32(b, 4).unwrap_or(0);
    if n == 0 || n > MAX_FAT_ARCHES {
        return Err(Error::new(format!("implausible fat Mach-O arch count {n}")));
    }
    // fat_arch is 20 bytes; fat_arch_64 (magic 0xcafebabf) is 32. cputype leads both.
    let stride = if b[3] == 0xBF { 32 } else { 20 };
    let mut keys = Vec::new();
    for i in 0..n as usize {
        let cpu =
            be32(b, 8 + i * stride).ok_or_else(|| Error::new("truncated fat Mach-O header"))?;
        if let Some(arch) = macho_arch(cpu).filter(|a| !keys.contains(&format!("Darwin-{a}"))) {
            keys.push(format!("Darwin-{arch}"));
        }
    }
    if keys.is_empty() {
        return Err(Error::new("fat Mach-O contains neither x86_64 nor arm64"));
    }
    Ok(keys)
}

/// Return every manifest key this slice can serve (two for a fat Mach-O).
pub fn sniff(b: &[u8]) -> Result<Vec<String>> {
    match b.get(..4) {
        Some(b"\x7fELF") if b.len() >= 20 => sniff_elf(b),
        Some(&[0xCA, 0xFE, 0xBA, 0xBE | 0xBF]) => sniff_fat(b),
        Some(&[0xCF, 0xFA, 0xED, 0xFE]) => {
            let cpu = le32(b, 4).unwrap_or(0);
            let arch = macho_arch(cpu)
                .ok_or_else(|| Error::new(format!("unsupported Mach-O CPU type 0x{cpu:x}")))?;
            Ok(vec![format!("Darwin-{arch}")])
        }
        Some(&[b'M', b'Z', ..]) => match pe::machine(b)? {
            pe::MACHINE_AMD64 => Ok(vec!["Windows-x86_64".into()]),
            pe::MACHINE_ARM64 => Ok(vec!["Windows-aarch64".into()]),
            m => Err(Error::new(format!("unsupported PE machine 0x{m:04x}"))),
        },
        _ => Err(Error::new(
            "unrecognised executable format (expected 64-bit ELF, Mach-O or PE)",
        )),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::pe::tests::fake_pe;

    /// 64-byte ELF header with the given OS/ABI and machine.
    pub(crate) fn fake_elf(osabi: u8, machine: u16) -> Vec<u8> {
        let mut b = vec![0u8; 64];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[6] = 1;
        b[7] = osabi;
        b[16] = 2; // ET_EXEC
        b[18..20].copy_from_slice(&machine.to_le_bytes());
        b
    }

    /// OS/ABI 0 ELF whose single `PT_NOTE` holds `notes` (`p_align` = `align`).
    pub(crate) fn fake_noted_elf(machine: u16, align: u64, notes: &[u8]) -> Vec<u8> {
        let mut b = fake_elf(0, machine);
        b[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
        b[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
        b[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
        let mut ph = [0u8; 56];
        ph[..4].copy_from_slice(&PT_NOTE.to_le_bytes());
        ph[8..16].copy_from_slice(&120u64.to_le_bytes());
        ph[32..40].copy_from_slice(&(notes.len() as u64).to_le_bytes());
        ph[48..56].copy_from_slice(&align.to_le_bytes());
        b.extend_from_slice(&ph);
        b.extend_from_slice(notes);
        b
    }

    pub(crate) fn note(name: &[u8], kind: u32, desc: &[u8], align: usize) -> Vec<u8> {
        let mut n = Vec::new();
        n.extend_from_slice(&(name.len() as u32).to_le_bytes());
        n.extend_from_slice(&(desc.len() as u32).to_le_bytes());
        n.extend_from_slice(&kind.to_le_bytes());
        n.extend_from_slice(name);
        n.resize(n.len().next_multiple_of(align), 0);
        n.extend_from_slice(desc);
        n.resize(n.len().next_multiple_of(align), 0);
        n
    }

    /// What zig/lld emit for `*-unknown-freebsd`: a GNU property note, then
    /// the FreeBSD ABI tag.
    pub(crate) fn freebsd_notes(align: usize) -> Vec<u8> {
        let mut n = note(b"GNU\0", 5, &[0; 12], align);
        n.extend(note(b"FreeBSD\0", 4, &1_400_500u32.to_le_bytes(), align));
        n.extend(note(
            b"FreeBSD\0",
            NT_FREEBSD_ABI_TAG,
            &1_400_500u32.to_le_bytes(),
            align,
        ));
        n
    }

    pub(crate) fn fake_fat(cpus: &[u32]) -> Vec<u8> {
        fat_with(0xBE, 20, cpus)
    }

    fn fat_with(last_magic: u8, stride: usize, cpus: &[u32]) -> Vec<u8> {
        let mut b = vec![0xCA, 0xFE, 0xBA, last_magic];
        b.extend_from_slice(&(cpus.len() as u32).to_be_bytes());
        for &cpu in cpus {
            b.extend_from_slice(&cpu.to_be_bytes());
            b.resize(b.len() + stride - 4, 0);
        }
        b
    }

    fn thin(cpu: u32) -> Vec<u8> {
        let mut b = vec![0xCF, 0xFA, 0xED, 0xFE];
        b.extend_from_slice(&cpu.to_le_bytes());
        b.resize(32, 0);
        b
    }

    fn one(b: &[u8]) -> String {
        let keys = sniff(b).unwrap();
        assert_eq!(keys.len(), 1);
        keys[0].clone()
    }

    #[test]
    fn elf() {
        assert_eq!(one(&fake_elf(0, EM_X86_64)), "Linux-x86_64");
        assert_eq!(one(&fake_elf(3, EM_AARCH64)), "Linux-aarch64");
        assert_eq!(one(&fake_elf(9, EM_X86_64)), "FreeBSD-x86_64");
        assert_eq!(one(&fake_elf(9, EM_AARCH64)), "FreeBSD-aarch64");
        assert!(
            sniff(&fake_elf(6, EM_X86_64))
                .unwrap_err()
                .0
                .contains("OS/ABI 6")
        );
        assert!(sniff(&fake_elf(0, 3)).unwrap_err().0.contains("machine 3"));
        let mut elf32 = fake_elf(0, EM_X86_64);
        elf32[4] = 1;
        assert!(sniff(&elf32).unwrap_err().0.contains("64-bit"));
        assert!(sniff(b"\x7fELF").unwrap_err().0.contains("unrecognised"));
    }

    #[test]
    fn elf_freebsd_abi_note() {
        let noted = |align, notes: &[u8]| one(&fake_noted_elf(EM_X86_64, align, notes));
        assert_eq!(noted(4, &freebsd_notes(4)), "FreeBSD-x86_64");
        assert_eq!(noted(8, &freebsd_notes(8)), "FreeBSD-x86_64");
        // Other owners or note types, and truncated notes, are not a brand.
        assert_eq!(noted(4, &note(b"GNU\0", 1, &[0; 16], 4)), "Linux-x86_64");
        assert_eq!(noted(4, &note(b"FreeBSD\0", 4, &[0; 4], 4)), "Linux-x86_64");
        assert_eq!(noted(4, &freebsd_notes(4)[..40]), "Linux-x86_64");
        assert_eq!(noted(4, &[]), "Linux-x86_64");
        // A note segment outside the file, or program headers past EOF.
        let mut b = fake_noted_elf(EM_X86_64, 4, &freebsd_notes(4));
        b[64 + 32..64 + 40].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(one(&b), "Linux-x86_64");
        b[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(one(&b), "Linux-x86_64");
        // OS/ABI 3 is explicitly Linux whatever the notes say.
        let mut b = fake_noted_elf(EM_X86_64, 4, &freebsd_notes(4));
        b[7] = ELFOSABI_LINUX;
        assert_eq!(one(&b), "Linux-x86_64");
    }

    #[test]
    fn macho() {
        assert_eq!(one(&thin(CPU_X86_64)), "Darwin-x86_64");
        assert_eq!(one(&thin(CPU_ARM64)), "Darwin-aarch64");
        assert!(sniff(&thin(7)).unwrap_err().0.contains("CPU type 0x7"));
        assert_eq!(
            sniff(&fake_fat(&[CPU_X86_64, CPU_ARM64])).unwrap(),
            ["Darwin-x86_64", "Darwin-aarch64"]
        );
        assert_eq!(
            sniff(&fat_with(0xBF, 32, &[CPU_X86_64, CPU_ARM64])).unwrap(),
            ["Darwin-x86_64", "Darwin-aarch64"]
        );
        // arm64e (subtype differs, same cputype) collapses into one key; unknown CPUs are skipped.
        assert_eq!(
            sniff(&fake_fat(&[CPU_ARM64, CPU_ARM64, 18])).unwrap(),
            ["Darwin-aarch64"]
        );
        assert!(sniff(&fake_fat(&[18])).unwrap_err().0.contains("neither"));
        assert!(
            sniff(&fake_fat(&[]))
                .unwrap_err()
                .0
                .contains("arch count 0")
        );
        // Java class file: same magic, then minor/major version.
        assert!(
            sniff(b"\xca\xfe\xba\xbe\x00\x00\x00\x41")
                .unwrap_err()
                .0
                .contains("implausible")
        );
        let mut truncated = fake_fat(&[CPU_X86_64, CPU_ARM64]);
        truncated.truncate(30);
        assert!(sniff(&truncated).unwrap_err().0.contains("truncated"));
    }

    #[test]
    fn pe_images() {
        assert_eq!(one(&fake_pe(pe::MACHINE_AMD64)), "Windows-x86_64");
        assert_eq!(one(&fake_pe(pe::MACHINE_ARM64)), "Windows-aarch64");
        assert!(sniff(&fake_pe(0x14C)).unwrap_err().0.contains("0x014c"));
        assert!(sniff(b"MZ\0\0").unwrap_err().0.contains("not a PE"));
    }

    #[test]
    fn unknown() {
        assert!(
            sniff(b"#!/bin/sh\n")
                .unwrap_err()
                .0
                .contains("unrecognised")
        );
        assert!(sniff(b"").is_err());
    }
}
