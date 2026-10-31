//! Reading a packed file back: open the archive, pick a slice for the host
//! (natively, via the OS's own translator, or via the bundled emulator) and
//! write the executables to disk.

use crate::manifest::Manifest;
use crate::zip::{self, CdEntry};
use crate::{Error, Result, Span, pe};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Normalise an OS / arch pair (Rust `consts` or `uname` spelling) to a
/// manifest key such as `Linux-x86_64` or `Darwin-aarch64`.
pub fn host_key(os: &str, arch: &str) -> String {
    let os = match os {
        "linux" => "Linux",
        "freebsd" => "FreeBSD",
        "macos" => "Darwin",
        "windows" => "Windows",
        other => other,
    };
    let arch = match arch {
        "amd64" | "x64" => "x86_64",
        "arm64" => "aarch64",
        other => other,
    };
    format!("{os}-{arch}")
}

/// Key for the machine this code was compiled for.
pub fn current_host() -> String {
    host_key(std::env::consts::OS, std::env::consts::ARCH)
}

/// How to run the packed program on a given host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Run the slice directly. `key` may differ from the host when the OS
    /// translates transparently (Rosetta 2, Windows Prism).
    Native { key: String, slice: Span },
    /// Run `emu <slice> args...` (FEX on Linux/aarch64).
    Emulated { key: String, slice: Span, emu: Span },
}

/// x86_64 key the OS itself can run on this host, if any.
fn translated(host: &str) -> Option<&'static str> {
    match host {
        "Darwin-aarch64" => Some("Darwin-x86_64"),
        "Windows-aarch64" => Some("Windows-x86_64"),
        _ => None,
    }
}

/// Choose how to run the manifest's program on `host`.
pub fn plan(m: &Manifest, host: &str) -> Result<Plan> {
    if let Some(slice) = m.get(host) {
        return Ok(Plan::Native {
            key: host.to_owned(),
            slice,
        });
    }
    if let Some((key, slice)) = translated(host).and_then(|k| Some((k, m.get(k)?))) {
        return Ok(Plan::Native {
            key: key.to_owned(),
            slice,
        });
    }
    if let Some((emu_host, emu)) = &m.emu
        && emu_host == host
        && let Some(slice) = m.get("Linux-x86_64")
    {
        return Ok(Plan::Emulated {
            key: "Linux-x86_64".to_owned(),
            slice,
            emu: *emu,
        });
    }
    let have: Vec<&str> = m.slices.iter().map(|(k, _)| k.as_str()).collect();
    Err(Error::new(format!(
        "no slice for {host} (this file has: {})",
        have.join(", ")
    )))
}

/// A parsed polyglot file.
#[derive(Debug)]
pub struct Archive<'a> {
    data: &'a [u8],
    pub manifest: Manifest,
    pub entries: Vec<CdEntry>,
}

impl<'a> Archive<'a> {
    /// Parse the ZIP tail and manifest. An Authenticode certificate table
    /// appended after packing is skipped.
    pub fn open(data: &'a [u8]) -> Result<Self> {
        let data = &data[..pe::logical_end(data)];
        let eocd = zip::find_eocd(data)?;
        let entries = zip::read_central_directory(data, &eocd)?;
        let text = std::str::from_utf8(eocd.comment.slice(data)?)
            .map_err(|_| Error::new("unibin manifest is not UTF-8"))?;
        let manifest = Manifest::parse(text)?;
        Ok(Archive {
            data,
            manifest,
            entries,
        })
    }

    /// Bytes of a manifest span, checked against its ZIP entry's CRC-32.
    pub fn payload(&self, span: Span) -> Result<&'a [u8]> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.data == span)
            .ok_or_else(|| {
                Error::new(format!("span {},{} has no ZIP entry", span.off, span.len))
            })?;
        let bytes = span.slice(self.data)?;
        if zip::crc32(bytes) != entry.crc {
            return Err(Error::new(format!(
                "ZIP entry {:?} fails its CRC check",
                entry.name
            )));
        }
        Ok(bytes)
    }

    /// Write the plan's executables into `dir`. Returns the argv prefix:
    /// `[program]` or `[emulator, program]`. `name` becomes the program's
    /// file name so multi-call binaries still see a meaningful `argv[0]`.
    pub fn materialize(&self, plan: &Plan, dir: &Path, name: &str) -> Result<Vec<PathBuf>> {
        match plan {
            Plan::Native { slice, .. } => {
                Ok(vec![write_exec(dir.join(name), self.payload(*slice)?)?])
            }
            Plan::Emulated { slice, emu, .. } => {
                let emu = write_exec(dir.join(".emu"), self.payload(*emu)?)?;
                Ok(vec![
                    emu,
                    write_exec(dir.join(name), self.payload(*slice)?)?,
                ])
            }
        }
    }
}

/// Create `path` exclusively (never follow a pre-planted file) as 0700.
fn write_exec(path: PathBuf, bytes: &[u8]) -> Result<PathBuf> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o700);
    let mut f = opts
        .open(&path)
        .map_err(|e| Error::new(format!("cannot create {}: {e}", path.display())))?;
    f.write_all(bytes)?;
    Ok(path)
}

/// Create a fresh private directory under `base`.
pub fn temp_dir(base: &Path) -> Result<PathBuf> {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = base.join(format!("unibin-{}-{n}-{nanos}", std::process::id()));
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(&dir)
        .map_err(|e| Error::new(format!("cannot create {}: {e}", dir.display())))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zip::ZipWriter;

    fn span(off: u64) -> Span {
        Span { off, len: 1 }
    }

    fn manifest(keys: &[&str], emu: bool) -> Manifest {
        Manifest {
            emu: emu.then(|| ("Linux-aarch64".to_owned(), span(99))),
            slices: keys
                .iter()
                .enumerate()
                .map(|(i, k)| (k.to_string(), span(i as u64)))
                .collect(),
        }
    }

    #[test]
    fn host_keys() {
        assert_eq!(host_key("linux", "x86_64"), "Linux-x86_64");
        assert_eq!(host_key("freebsd", "amd64"), "FreeBSD-x86_64");
        assert_eq!(host_key("FreeBSD", "arm64"), "FreeBSD-aarch64");
        assert_eq!(host_key("macos", "aarch64"), "Darwin-aarch64");
        assert_eq!(host_key("windows", "x64"), "Windows-x86_64");
        assert_eq!(host_key("Darwin", "arm64"), "Darwin-aarch64");
        assert!(current_host().contains('-'));
    }

    #[test]
    fn plans() {
        let all = manifest(&["Linux-x86_64", "Darwin-x86_64", "Windows-x86_64"], true);
        let native = |key: &str, off| Plan::Native {
            key: key.into(),
            slice: span(off),
        };
        assert_eq!(plan(&all, "Linux-x86_64"), Ok(native("Linux-x86_64", 0)));
        assert_eq!(plan(&all, "Darwin-aarch64"), Ok(native("Darwin-x86_64", 1)));
        assert_eq!(
            plan(&all, "Windows-aarch64"),
            Ok(native("Windows-x86_64", 2))
        );
        assert_eq!(
            plan(&all, "Linux-aarch64"),
            Ok(Plan::Emulated {
                key: "Linux-x86_64".into(),
                slice: span(0),
                emu: span(99)
            })
        );
        let e = plan(&all, "FreeBSD-aarch64").unwrap_err();
        assert!(e.0.contains("no slice for FreeBSD-aarch64"), "{e}");
        assert!(e.0.contains("Linux-x86_64, Darwin-x86_64"), "{e}");
        // A native slice beats the emulator.
        let both = manifest(&["Linux-x86_64", "Linux-aarch64"], true);
        assert_eq!(plan(&both, "Linux-aarch64"), Ok(native("Linux-aarch64", 1)));
        // No emulator, or emulator but no x86_64 slice.
        assert!(plan(&manifest(&["Linux-x86_64"], false), "Linux-aarch64").is_err());
        assert!(plan(&manifest(&["Darwin-x86_64"], true), "Linux-aarch64").is_err());
        assert!(plan(&manifest(&["Linux-x86_64"], true), "FreeBSD-x86_64").is_err());
    }

    fn archive_bytes(comment: &str) -> Vec<u8> {
        let mut w = ZipWriter::new(b"prefix".to_vec());
        w.add("Linux-x86_64", b"#!/bin/sh\necho native\n").unwrap();
        w.add(".emu", b"EMU").unwrap();
        w.finish(comment.as_bytes()).unwrap()
    }

    // 6-byte prefix + 30-byte local header + 12-byte name = 48, and so on.
    const GOOD: &str = "unibin=1\nemu=Linux-aarch64\nemu_payload=104,3\nLinux-x86_64=48,22\n";

    #[test]
    fn open_and_payload() {
        let z = archive_bytes(GOOD);
        let a = Archive::open(&z).unwrap();
        assert_eq!(a.entries.len(), 2);
        let s = a.manifest.get("Linux-x86_64").unwrap();
        assert_eq!(a.payload(s).unwrap(), b"#!/bin/sh\necho native\n");
        let e = a.payload(Span { off: 48, len: 5 }).unwrap_err();
        assert!(e.0.contains("no ZIP entry"), "{e}");
    }

    #[test]
    fn open_errors() {
        assert!(Archive::open(b"tiny").is_err());
        assert!(
            Archive::open(&archive_bytes("unibin=7\n"))
                .unwrap_err()
                .0
                .contains("version")
        );
        let e = Archive::open(&archive_bytes("unibin=1\n\u{e9}")).unwrap_err();
        assert!(e.0.contains("no '='"), "{e}");
        let mut z = archive_bytes(GOOD);
        let n = z.len();
        z[n - 3] = 0xFF;
        assert!(Archive::open(&z).unwrap_err().0.contains("UTF-8"));
        let mut z = archive_bytes(GOOD);
        let eocd = zip::find_eocd(&z).unwrap();
        z[eocd.cd_offset as usize] = 0;
        assert!(Archive::open(&z).unwrap_err().0.contains("corrupt"));
    }

    #[test]
    fn crc_mismatch_detected() {
        let mut z = archive_bytes(GOOD);
        z[50] ^= 1;
        let a = Archive::open(&z).unwrap();
        let e = a
            .payload(a.manifest.get("Linux-x86_64").unwrap())
            .unwrap_err();
        assert!(e.0.contains("CRC"), "{e}");
    }

    #[test]
    fn materialize_native_and_emulated() {
        let z = archive_bytes(GOOD);
        let a = Archive::open(&z).unwrap();
        let base = std::env::temp_dir();
        let dir = temp_dir(&base).unwrap();
        let native = plan(&a.manifest, "Linux-x86_64").unwrap();
        let argv = a.materialize(&native, &dir, "prog").unwrap();
        assert_eq!(argv, [dir.join("prog")]);
        assert_eq!(fs::read(&argv[0]).unwrap(), b"#!/bin/sh\necho native\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&argv[0]).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        // Exclusive creation: an existing file is never overwritten.
        let e = a.materialize(&native, &dir, "prog").unwrap_err();
        assert!(e.0.contains("cannot create"), "{e}");

        let emulated = plan(&a.manifest, "Linux-aarch64").unwrap();
        let argv = a.materialize(&emulated, &dir, "prog2").unwrap();
        assert_eq!(argv, [dir.join(".emu"), dir.join("prog2")]);
        assert_eq!(fs::read(&argv[0]).unwrap(), b"EMU");

        let other = temp_dir(&base).unwrap();
        assert_ne!(other, dir);
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&other).unwrap();
        let e = temp_dir(&base.join("unibin-does-not-exist/x")).unwrap_err();
        assert!(e.0.contains("cannot create"), "{e}");
    }
}
