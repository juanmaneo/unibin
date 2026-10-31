//! Assemble prologue + relocated PE launcher + ZIP of slices + manifest.

use crate::manifest::{Manifest, check_key};
use crate::zip::{EOCD_LEN, ZipWriter};
use crate::{Error, Result, pe, prologue, sniff};

/// Host on which the bundled emulator runs the `Linux-x86_64` slice.
pub const EMU_HOST: &str = "Linux-aarch64";

/// One native executable and the manifest keys it serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slice {
    pub keys: Vec<String>,
    pub data: Vec<u8>,
}

impl Slice {
    /// Identify `data` by content. If `key` is given it must agree with the
    /// content, and the slice is registered under that key only.
    ///
    /// FreeBSD ELF slices branded only by their ABI note (what zig/lld emit)
    /// get `EI_OSABI` set to FreeBSD, like `brandelf -t FreeBSD`.
    pub fn new(mut data: Vec<u8>, key: Option<&str>) -> Result<Slice> {
        let found = sniff::sniff(&data)?;
        if data.starts_with(b"\x7fELF") && found[0].starts_with("FreeBSD-") {
            data[7] = sniff::ELFOSABI_FREEBSD;
        }
        let keys = match key {
            None => found,
            Some(k) if found.iter().any(|f| f == k) => vec![k.to_owned()],
            Some(k) => {
                return Err(Error::new(format!(
                    "slice declared as {k} but its content is {}",
                    found.join(" + ")
                )));
            }
        };
        Ok(Slice { keys, data })
    }
}

/// Everything that goes into one packed file.
#[derive(Debug, Clone, Default)]
pub struct Input {
    /// x86_64 PE32+ launcher stub (`unibin-launch.exe`), unsigned.
    pub launcher: Vec<u8>,
    pub slices: Vec<Slice>,
    /// Static Linux/aarch64 x86_64 emulator (FEX), run when no native
    /// `Linux-aarch64` slice exists.
    pub emu: Option<Vec<u8>>,
}

fn check_input(input: &Input) -> Result<()> {
    if input.slices.is_empty() {
        return Err(Error::new("nothing to pack: give at least one slice"));
    }
    let mut seen: Vec<&str> = Vec::new();
    for key in input.slices.iter().flat_map(|s| &s.keys) {
        check_key(key)?;
        if seen.contains(&key.as_str()) {
            return Err(Error::new(format!("duplicate slice for {key}")));
        }
        seen.push(key);
    }
    if let Some(emu) = &input.emu {
        if sniff::sniff(emu).ok() != Some(vec![EMU_HOST.to_owned()]) {
            return Err(Error::new(format!(
                "emulator must be a static {EMU_HOST} ELF executable"
            )));
        }
        if !seen.contains(&"Linux-x86_64") {
            return Err(Error::new("an emulator needs a Linux-x86_64 slice to run"));
        }
    }
    Ok(())
}

/// Produce the polyglot file.
pub fn pack(input: &Input) -> Result<Vec<u8>> {
    check_input(input)?;
    let launcher = pe::relocate(&input.launcher, prologue::len())?;
    let mut prefix = vec![0u8; launcher.shift];
    prefix.extend_from_slice(&launcher.image);

    let mut zip = ZipWriter::new(prefix);
    let mut manifest = Manifest::default();
    for slice in &input.slices {
        let span = zip.add(&slice.keys.join("+"), &slice.data)?;
        manifest
            .slices
            .extend(slice.keys.iter().map(|k| (k.clone(), span)));
    }
    if let Some(emu) = &input.emu {
        manifest.emu = Some((EMU_HOST.to_owned(), zip.add("emu-Linux-aarch64", emu)?));
    }

    let eocd = zip.eocd_offset();
    let mut comment = manifest.render().into_bytes();
    // Make the file length a multiple of 8 so Authenticode signing appends its
    // certificate table without inserting padding after the EOCD comment.
    let unpadded = eocd as usize + EOCD_LEN + comment.len();
    comment.resize(
        comment.len() + unpadded.next_multiple_of(8) - unpadded,
        b'\n',
    );
    let mut out = zip.finish(&comment)?;

    let head = prologue::build(launcher.shift, eocd, launcher.lfanew)?;
    out[..launcher.shift].copy_from_slice(&head);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{Archive, Plan, plan};
    use crate::pe::tests::fake_pe;
    use crate::pe::{MACHINE_AMD64, MACHINE_ARM64};
    use crate::sniff::tests::{fake_elf, fake_fat, fake_noted_elf, freebsd_notes};

    const X86: u16 = 62;
    const A64: u16 = 183;

    fn slice(data: Vec<u8>) -> Slice {
        Slice::new(data, None).unwrap()
    }

    fn full_input() -> Input {
        Input {
            launcher: fake_pe(MACHINE_AMD64),
            slices: vec![
                slice(fake_elf(0, X86)),
                slice(fake_elf(9, X86)),
                slice(fake_fat(&[0x0100_0007, 0x0100_000C])),
                slice(fake_pe(MACHINE_AMD64)),
            ],
            emu: Some(fake_elf(3, A64)),
        }
    }

    #[test]
    fn packs_every_format() {
        let out = pack(&full_input()).unwrap();
        assert_eq!(out.len() % 8, 0);
        prologue::check(&out).unwrap();
        assert_eq!(pe::nt_headers(&out), Some((0x680, MACHINE_AMD64)));

        let a = Archive::open(&out).unwrap();
        let keys: Vec<_> = a.manifest.slices.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "Linux-x86_64",
                "FreeBSD-x86_64",
                "Darwin-x86_64",
                "Darwin-aarch64",
                "Windows-x86_64"
            ]
        );
        assert_eq!(
            a.manifest.get("Darwin-x86_64"),
            a.manifest.get("Darwin-aarch64")
        );
        assert_eq!(a.entries[2].name, "Darwin-x86_64+Darwin-aarch64");
        let win = a
            .payload(a.manifest.get("Windows-x86_64").unwrap())
            .unwrap();
        assert_eq!(win, fake_pe(MACHINE_AMD64));
        let emu = a.manifest.emu.clone().unwrap().1;
        let slice = a.manifest.get("Linux-x86_64").unwrap();
        let key = "Linux-x86_64".to_owned();
        assert_eq!(
            plan(&a.manifest, "Linux-aarch64"),
            Ok(Plan::Emulated { key, slice, emu })
        );
        assert_eq!(a.payload(emu).unwrap(), fake_elf(3, A64));

        // The baked-in EOCD offset matches what a ZIP reader finds.
        let eocd = crate::zip::find_eocd(&out).unwrap().offset;
        let text = String::from_utf8_lossy(&out[..0x600]);
        assert!(text.contains(&format!("\ne={eocd} ")), "{text}");
    }

    #[test]
    fn survives_authenticode_signing() {
        let mut out = pack(&full_input()).unwrap();
        let unsigned = out.clone();
        // What signtool does: append WIN_CERTIFICATE at the (8-aligned) EOF
        // and point the security data directory at it.
        let cert_at = out.len();
        out.extend_from_slice(&[0xAB; 24]);
        let dir = 0x680 + 24 + 112 + 4 * 8;
        crate::put32(&mut out, dir, cert_at as u32);
        crate::put32(&mut out, dir + 4, 24);
        assert_eq!(pe::logical_end(&out), unsigned.len());
        let a = Archive::open(&out).unwrap();
        let b = Archive::open(&unsigned).unwrap();
        assert_eq!(a.manifest, b.manifest);
        let win = a.manifest.get("Windows-x86_64").unwrap();
        assert_eq!(a.payload(win).unwrap(), fake_pe(MACHINE_AMD64));
    }

    #[test]
    fn output_is_deterministic() {
        assert_eq!(pack(&full_input()).unwrap(), pack(&full_input()).unwrap());
    }

    #[test]
    fn slice_key_validation() {
        let s = Slice::new(
            fake_fat(&[0x0100_0007, 0x0100_000C]),
            Some("Darwin-aarch64"),
        )
        .unwrap();
        assert_eq!(s.keys, ["Darwin-aarch64"]);
        let e = Slice::new(fake_elf(0, X86), Some("FreeBSD-x86_64")).unwrap_err();
        assert!(
            e.0.contains("declared as FreeBSD-x86_64 but its content is Linux-x86_64"),
            "{e}"
        );
        assert!(Slice::new(b"junk".to_vec(), None).is_err());
    }

    #[test]
    fn note_branded_freebsd_slice_gets_osabi() {
        let elf = fake_noted_elf(X86, 4, &freebsd_notes(4));
        let s = Slice::new(elf.clone(), Some("FreeBSD-x86_64")).unwrap();
        assert_eq!(s.data[7], 9);
        assert_eq!(s.data[8..], elf[8..]);
        assert_eq!(slice(fake_elf(9, A64)).data[7], 9);
    }

    fn pack_err(f: impl FnOnce(&mut Input)) -> String {
        let mut input = full_input();
        f(&mut input);
        pack(&input).unwrap_err().0
    }

    #[test]
    fn input_errors() {
        assert!(pack_err(|i| i.slices.clear()).contains("nothing to pack"));
        assert!(
            pack_err(|i| i.slices.push(slice(fake_elf(3, X86))))
                .contains("duplicate slice for Linux-x86_64")
        );
        let bad_key = Slice {
            keys: vec!["nope".into()],
            data: vec![],
        };
        assert!(pack_err(|i| i.slices.push(bad_key)).contains("invalid slice key"));
        assert!(pack_err(|i| i.emu = Some(fake_elf(0, X86))).contains("emulator must be"));
        assert!(pack_err(|i| i.emu = Some(b"x".to_vec())).contains("emulator must be"));
        assert!(pack_err(|i| drop(i.slices.remove(0))).contains("needs a Linux-x86_64"));
        assert!(pack_err(|i| i.launcher = fake_pe(MACHINE_ARM64)).contains("must be x86_64"));
        assert!(pack_err(|i| i.launcher = b"MZ".to_vec()).contains("not a PE"));
    }

    #[test]
    fn without_emulator() {
        let mut input = full_input();
        input.emu = None;
        let out = pack(&input).unwrap();
        let a = Archive::open(&out).unwrap();
        assert_eq!(a.manifest.emu, None);
        assert!(plan(&a.manifest, "Linux-aarch64").is_err());
    }
}
