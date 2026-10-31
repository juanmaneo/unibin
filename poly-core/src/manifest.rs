//! Text manifest stored in the ZIP EOCD comment.
//!
//! ```text
//! poly=1
//! emu=Linux-aarch64
//! emu_payload=<off>,<len>
//! Linux-x86_64=<off>,<len>
//! Darwin-aarch64=<off>,<len>
//! ```
//!
//! Keys are `OS-arch` with `OS` as printed by `uname -s` (`Linux`, `FreeBSD`,
//! `Darwin`, plus `Windows`) and `arch` normalised to `x86_64` / `aarch64`.
//! The shell prologue looks keys up with `sed`, so they are restricted to
//! `[A-Za-z0-9_]` around a single `-`.

use crate::{Error, Result, Span};

/// Manifest format version written by this crate.
pub const VERSION: &str = "1";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Manifest {
    /// Host key that should run the x86_64 slice under the bundled emulator,
    /// and where the emulator binary is stored.
    pub emu: Option<(String, Span)>,
    /// Native slices, in file order. Several keys may share one span (a fat
    /// Mach-O serves both `Darwin-x86_64` and `Darwin-aarch64`).
    pub slices: Vec<(String, Span)>,
}

/// Validate a manifest key (`OS-arch`).
pub fn check_key(key: &str) -> Result<()> {
    let ok_part =
        |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    match key.split_once('-') {
        Some((os, arch)) if ok_part(os) && ok_part(arch) => Ok(()),
        _ => Err(Error::new(format!(
            "invalid slice key {key:?}: expected OS-arch, e.g. Linux-x86_64"
        ))),
    }
}

fn parse_span(v: &str) -> Result<Span> {
    let parsed = v.split_once(',').and_then(|(o, l)| {
        Some(Span {
            off: o.parse().ok()?,
            len: l.parse().ok()?,
        })
    });
    parsed.ok_or_else(|| Error::new(format!("invalid span {v:?}: expected <offset>,<length>")))
}

impl Manifest {
    pub fn get(&self, key: &str) -> Option<Span> {
        self.slices.iter().find(|(k, _)| k == key).map(|(_, s)| *s)
    }

    pub fn render(&self) -> String {
        let mut s = format!("poly={VERSION}\n");
        if let Some((host, span)) = &self.emu {
            s += &format!("emu={host}\nemu_payload={},{}\n", span.off, span.len);
        }
        for (key, span) in &self.slices {
            s += &format!("{key}={},{}\n", span.off, span.len);
        }
        s
    }

    pub fn parse(text: &str) -> Result<Manifest> {
        let mut version = None;
        let mut emu_host = None;
        let mut emu_payload = None;
        let mut m = Manifest::default();
        for line in text.lines().filter(|l| !l.is_empty()) {
            let (k, v) = line
                .split_once('=')
                .ok_or_else(|| Error::new(format!("manifest line {line:?} has no '='")))?;
            match k {
                "poly" => version = Some(v),
                "emu" => emu_host = Some(check_key(v).map(|()| v.to_owned())?),
                "emu_payload" => emu_payload = Some(parse_span(v)?),
                _ => {
                    check_key(k)?;
                    if m.get(k).is_some() {
                        return Err(Error::new(format!("duplicate manifest key {k:?}")));
                    }
                    m.slices.push((k.to_owned(), parse_span(v)?));
                }
            }
        }
        match version {
            Some(VERSION) => {}
            None => return Err(Error::new("not a poly manifest (missing poly=1)")),
            Some(v) => {
                return Err(Error::new(format!(
                    "unsupported poly manifest version {v:?}"
                )));
            }
        }
        m.emu = match (emu_host, emu_payload) {
            (Some(h), Some(p)) => Some((h, p)),
            (None, None) => None,
            _ => {
                return Err(Error::new(
                    "manifest has emu without emu_payload (or vice versa)",
                ));
            }
        };
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            emu: Some(("Linux-aarch64".into(), Span { off: 9000, len: 77 })),
            slices: vec![
                (
                    "Linux-x86_64".into(),
                    Span {
                        off: 4608,
                        len: 12345,
                    },
                ),
                (
                    "Darwin-x86_64".into(),
                    Span {
                        off: 20000,
                        len: 10,
                    },
                ),
                (
                    "Darwin-aarch64".into(),
                    Span {
                        off: 20000,
                        len: 10,
                    },
                ),
            ],
        }
    }

    #[test]
    fn render_format() {
        let text = sample().render();
        assert_eq!(
            text,
            "poly=1\nemu=Linux-aarch64\nemu_payload=9000,77\nLinux-x86_64=4608,12345\n\
             Darwin-x86_64=20000,10\nDarwin-aarch64=20000,10\n"
        );
    }

    #[test]
    fn roundtrip_and_lookup() {
        let m = sample();
        let back = Manifest::parse(&(m.render() + "\n\n\n")).unwrap();
        assert_eq!(back, m);
        assert_eq!(
            back.get("Darwin-aarch64"),
            Some(Span {
                off: 20000,
                len: 10
            })
        );
        assert_eq!(back.get("FreeBSD-x86_64"), None);
        let plain = Manifest {
            emu: None,
            slices: vec![],
        };
        assert_eq!(Manifest::parse(&plain.render()).unwrap(), plain);
    }

    #[test]
    fn keys() {
        for good in [
            "Linux-x86_64",
            "FreeBSD-aarch64",
            "Darwin-x86_64",
            "Windows-x86_64",
        ] {
            check_key(good).unwrap();
        }
        for bad in [
            "",
            "Linux",
            "-x86_64",
            "Linux-",
            "Linux-x86-64",
            "Li nux-x",
            "a.b-c",
            "a-b/c",
        ] {
            assert!(
                check_key(bad).unwrap_err().0.contains("invalid slice key"),
                "{bad}"
            );
        }
    }

    #[test]
    fn parse_errors() {
        let cases = [
            ("Linux-x86_64=1,2\n", "missing poly=1"),
            ("poly=2\n", "unsupported"),
            ("poly=1\nnonsense\n", "no '='"),
            ("poly=1\nLinux-x86_64=1\n", "invalid span"),
            ("poly=1\nLinux-x86_64=a,2\n", "invalid span"),
            ("poly=1\nLinux-x86_64=1,b\n", "invalid span"),
            ("poly=1\nbad key=1,2\n", "invalid slice key"),
            ("poly=1\nLinux-x86_64=1,2\nLinux-x86_64=3,4\n", "duplicate"),
            ("poly=1\nemu=Linux-aarch64\n", "emu without"),
            ("poly=1\nemu_payload=1,2\n", "emu without"),
            ("poly=1\nemu=nope\n", "invalid slice key"),
            ("poly=1\nemu_payload=x\n", "invalid span"),
        ];
        for (text, want) in cases {
            let e = Manifest::parse(text).unwrap_err();
            assert!(e.0.contains(want), "{text:?}: {e}");
        }
    }
}
