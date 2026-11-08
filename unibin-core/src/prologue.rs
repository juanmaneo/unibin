//! The POSIX `sh` prologue that doubles as the PE DOS header.
//!
//! ```text
//! 0x00  MZqFpD='unibin' #<spaces>        line 1: e_magic "MZ", then a comment
//! 0x38  \n                               first newline, before e_lfanew
//! 0x39  #<sp><sp><e_lfanew LE32>\n       line 2: comment hiding e_lfanew (0x3C)
//! 0x41  script ... exit 1\n              selects, extracts and execs a slice
//!       \n padding up to the launcher's FileAlignment
//! ```
//!
//! Shells run a shebang-less file as a script after `execve` fails with
//! `ENOEXEC`, but only if the first line contains no NUL (POSIX.1-2024; bash
//! and zsh apply the same test). Hence spaces, not NULs, in the DOS header.
//!
//! The EOCD offset is baked into the script (`e=`) rather than found with
//! `tail -c 22`, so the file still works after Authenticode signing appends
//! a certificate table behind the ZIP. Binary data is only ever piped,
//! never captured with `$(...)`, which would drop NUL bytes.

use crate::{Error, Result, put32};

/// Offset of PE `e_lfanew` in the DOS header.
pub const LFANEW_AT: usize = 0x3C;
/// Offset of the first newline; must be below [`LFANEW_AT`].
pub const FIRST_NEWLINE: usize = 0x38;
const HEADER_LEN: usize = 0x41;
const LINE1: &[u8] = b"MZqFpD='unibin' #";
const EOCD_FIELD: &str = "@EOCD@";
const EOCD_WIDTH: usize = 20; // u64::MAX has 20 digits

const SCRIPT: &str = r#"# unibin: one file, many OSes. Re-sign only after packing.
o=$0
case $o in */*) ;; *) [ -f "$o" ] && o=./$o || o=$(command -v "$o") || exit 127 ;; esac
e=@EOCD@
z() { tail -c +$(($1+1)) "$o" | head -c $2; }
u() { echo $(($1+256*$2)); }
w() { echo $*; }
[ "$(w $(z $e 4 | od -An -tx1))" = "50 4b 05 06" ] || { echo "unibin: $o: damaged archive" >&2; exit 126; }
m=$(z $((e+22)) $(u $(z $((e+20)) 2 | od -An -tu1)))
g() { printf '%s\n' "$m" | sed -n "s/^$1=//p"; }
s=$(uname -s) a=$(uname -m) x=
case $a in amd64|x64) a=x86_64 ;; arm64) a=aarch64 ;; esac
k=$s-$a
r=$(g $k)
if [ -z "$r" ] && [ $k = Darwin-aarch64 ]; then r=$(g Darwin-x86_64); fi
if [ -z "$r" ] && [ $k = Linux-aarch64 ] && [ "$(g emu)" = $k ]; then r=$(g Linux-x86_64) x=$(g emu_payload); fi
[ -n "$r" ] || { echo "unibin: $o has no slice for $k" >&2; exit 126; }
d=$(mktemp -d "${TMPDIR:-/tmp}/unibin.XXXXXX") || exit 126
(trap '' HUP INT; while kill -0 $$; do sleep 1; done; rm -rf "$d") >/dev/null 2>&1 &
f=$d/${o##*/}
z ${r%,*} ${r#*,} >"$f" && chmod 700 "$f" || exit 126
if [ -n "$x" ]; then z ${x%,*} ${x#*,} >"$d/.emu" && chmod 700 "$d/.emu" && exec "$d/.emu" "$f" "$@"; exit 126; fi
exec "$f" "$@"
exit 1
"#;

/// Length of the prologue before padding; independent of the baked values.
pub fn len() -> usize {
    HEADER_LEN + SCRIPT.len() - EOCD_FIELD.len() + EOCD_WIDTH
}

/// Build a prologue of exactly `total` bytes.
pub fn build(total: usize, eocd: u64, lfanew: u32) -> Result<Vec<u8>> {
    if total < len() {
        return Err(Error::new(format!(
            "prologue needs {} bytes, only {total} available",
            len()
        )));
    }
    let mut out = vec![b' '; HEADER_LEN];
    out[..LINE1.len()].copy_from_slice(LINE1);
    out[FIRST_NEWLINE] = b'\n';
    out[FIRST_NEWLINE + 1] = b'#';
    put32(&mut out, LFANEW_AT, lfanew);
    out[HEADER_LEN - 1] = b'\n';
    out.extend_from_slice(
        SCRIPT
            .replace(EOCD_FIELD, &format!("{eocd:<EOCD_WIDTH$}"))
            .as_bytes(),
    );
    out.resize(total, b'\n');
    check(&out)?;
    Ok(out)
}

/// Verify the invariants that make the file acceptable to both shells and
/// the Windows loader.
pub fn check(p: &[u8]) -> Result<()> {
    if !p.starts_with(b"MZ") || p.len() < HEADER_LEN {
        return Err(Error::new(
            "prologue must start with MZ and hold a full DOS header",
        ));
    }
    let nl = p.iter().position(|&b| b == b'\n').unwrap_or(usize::MAX);
    if nl >= LFANEW_AT || p[..nl].contains(&0) {
        return Err(Error::new(
            "prologue's first line must end before e_lfanew (0x3C) and contain no NUL, or shells treat the file as binary",
        ));
    }
    let line2 = p[..LFANEW_AT]
        .iter()
        .rposition(|&b| b == b'\n')
        .unwrap_or(0)
        + 1;
    if p[line2] != b'#' || p[LFANEW_AT..LFANEW_AT + 4].contains(&b'\n') {
        return Err(Error::new(
            "e_lfanew must sit inside a shell comment with no newline byte in it",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::le32;

    #[test]
    fn layout() {
        let p = build(0x600, 123_456, 0x680).unwrap();
        assert_eq!(p.len(), 0x600);
        assert!(len() < 0x600, "prologue grew to {} bytes", len());
        assert_eq!(&p[..2], b"MZ");
        assert_eq!(p.iter().position(|&b| b == b'\n'), Some(FIRST_NEWLINE));
        assert!(!p[..FIRST_NEWLINE].contains(&0));
        assert!(p[2..FIRST_NEWLINE].iter().all(|&b| b != 0));
        assert_eq!(le32(&p, LFANEW_AT), Some(0x680));
        let text = String::from_utf8_lossy(&p);
        assert!(text.contains(&format!("\ne={:<20}\n", 123_456)));
        assert!(text.trim_end().ends_with("exit 1"));
        assert_eq!(build(len(), u64::MAX, 0).unwrap().len(), len());
    }

    #[test]
    fn script_is_plain_posix_sh_text() {
        assert!(SCRIPT.is_ascii());
        assert!(!SCRIPT.contains('\t') && !SCRIPT.contains('\r'));
        // Command substitution would strip NULs: binary data is only piped.
        assert!(!SCRIPT.contains("=$(z ${r"));
    }

    #[test]
    fn build_errors() {
        assert!(build(len() - 1, 0, 0x80).unwrap_err().0.contains("needs"));
        let e = build(0x1000, 0, 0x0A80).unwrap_err();
        assert!(e.0.contains("newline byte"), "{e}");
    }

    #[test]
    fn check_rejects_broken_headers() {
        let good = build(0x600, 1, 0x680).unwrap();
        check(&good).unwrap();
        assert!(check(b"ELF").unwrap_err().0.contains("start with MZ"));
        assert!(check(&good[..0x40]).is_err());

        let mut nul_padded = good.clone();
        nul_padded[0x20] = 0;
        assert!(check(&nul_padded).unwrap_err().0.contains("contain no NUL"));

        let mut late_newline = good.clone();
        late_newline[FIRST_NEWLINE] = b' ';
        assert!(check(&late_newline).unwrap_err().0.contains("first line"));

        let mut no_comment = good.clone();
        no_comment[FIRST_NEWLINE + 1] = b'x';
        assert!(check(&no_comment).unwrap_err().0.contains("comment"));
    }
}
