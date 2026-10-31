//! `poly-core`: the container format behind single-file polyglot executables.
//!
//! One output file is simultaneously:
//!
//! 1. a POSIX `sh` script (Linux, FreeBSD, macOS start it via the shell's
//!    `ENOEXEC` fallback),
//! 2. a PE image (Windows loads the launcher stub through `e_lfanew`),
//! 3. a ZIP archive (stored entries hold the native slices; the EOCD comment
//!    holds a text manifest).
//!
//! See `README.md` at the workspace root for the byte-level layout.

pub mod extract;
pub mod manifest;
pub mod pack;
pub mod pe;
pub mod prologue;
pub mod sniff;
pub mod zip;

use std::fmt;

/// Error type shared by every module: a human-readable message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl Error {
    pub fn new(msg: impl Into<String>) -> Self {
        Error(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Byte range inside the polyglot file, by absolute offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub off: u64,
    pub len: u64,
}

impl Span {
    /// Borrow the bytes covered by this span, or fail if it lies outside `data`.
    pub fn slice<'a>(&self, data: &'a [u8]) -> Result<&'a [u8]> {
        let end = self.off.checked_add(self.len);
        match end {
            Some(end) if end <= data.len() as u64 => Ok(&data[self.off as usize..end as usize]),
            _ => Err(Error::new(format!(
                "span {},{} is out of range (file is {} bytes)",
                self.off,
                self.len,
                data.len()
            ))),
        }
    }
}

fn bytes<const N: usize>(b: &[u8], at: usize) -> Option<[u8; N]> {
    b.get(at..at.checked_add(N)?)?.try_into().ok()
}

pub(crate) fn le16(b: &[u8], at: usize) -> Option<u16> {
    bytes(b, at).map(u16::from_le_bytes)
}

pub(crate) fn le32(b: &[u8], at: usize) -> Option<u32> {
    bytes(b, at).map(u32::from_le_bytes)
}

pub(crate) fn le64(b: &[u8], at: usize) -> Option<u64> {
    bytes(b, at).map(u64::from_le_bytes)
}

pub(crate) fn be32(b: &[u8], at: usize) -> Option<u32> {
    bytes(b, at).map(u32::from_be_bytes)
}

#[cfg(test)]
pub(crate) fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

pub(crate) fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_slice_bounds() {
        let d = [1u8, 2, 3, 4];
        assert_eq!(Span { off: 1, len: 2 }.slice(&d).unwrap(), &[2, 3]);
        assert_eq!(Span { off: 4, len: 0 }.slice(&d).unwrap(), &[] as &[u8]);
        let e = Span { off: 3, len: 2 }.slice(&d).unwrap_err();
        assert!(e.to_string().contains("out of range"), "{e}");
        assert!(
            Span {
                off: u64::MAX,
                len: 2
            }
            .slice(&d)
            .is_err()
        );
    }

    #[test]
    fn error_conversions() {
        let io = std::io::Error::from(std::io::ErrorKind::NotFound);
        let e: Error = io.into();
        assert!(!e.0.is_empty());
        let boxed: Box<dyn std::error::Error> = Box::new(Error::new("x"));
        assert_eq!(boxed.to_string(), "x");
    }

    #[test]
    fn byte_helpers() {
        let mut b = [0u8; 8];
        put16(&mut b, 0, 0x1234);
        put32(&mut b, 2, 0xdead_beef);
        assert_eq!(le16(&b, 0), Some(0x1234));
        assert_eq!(le32(&b, 2), Some(0xdead_beef));
        assert_eq!(be32(&b, 2), Some(0xefbe_adde));
        assert_eq!(le64(&b, 0), Some(0x0000_dead_beef_1234));
        assert_eq!(le64(&b, 1), None);
        assert_eq!(le16(&b, 7), None);
        assert_eq!(le32(&b, 6), None);
        assert_eq!(be32(&b, usize::MAX - 1), None);
    }
}
