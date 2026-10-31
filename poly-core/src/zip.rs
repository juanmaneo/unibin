//! Minimal stored-only ZIP writer and reader.
//!
//! Offsets in the central directory are absolute file offsets, so the
//! archive stays valid with the shell prologue and PE launcher in front of
//! it (`unzip -l` works on a packed file). Entries are never compressed:
//! each slice's bytes are directly addressable at a fixed offset.

use crate::{Error, Result, Span, le16, le32};
use std::sync::OnceLock;

const LOCAL_SIG: u32 = 0x0403_4b50;
const CD_SIG: u32 = 0x0201_4b50;
const EOCD_SIG: u32 = 0x0605_4b50;
/// Size of the end-of-central-directory record without its comment.
pub const EOCD_LEN: usize = 22;
const LOCAL_LEN: usize = 30;
const CD_LEN: usize = 46;
/// Largest EOCD comment the format can express.
pub const MAX_COMMENT: usize = 0xFFFF;
/// 1980-01-01, the DOS epoch: keeps output byte-for-byte reproducible.
const DOS_DATE: u16 = (1 << 5) | 1;
const VERSION_NEEDED: u16 = 10;
/// Made by UNIX (3), spec version 3.0, so `unzip` restores the mode bits.
const VERSION_MADE_BY: u16 = (3 << 8) | 30;
const EXEC_MODE: u32 = 0o100_755 << 16;

fn crc_table() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *slot = c;
        }
        t
    })
}

/// CRC-32 (IEEE 802.3), as used by ZIP.
pub fn crc32(data: &[u8]) -> u32 {
    let t = crc_table();
    !data.iter().fold(!0u32, |c, &b| {
        t[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8)
    })
}

/// Convert to a 32-bit ZIP field, refusing values that would need ZIP64.
pub fn fit32(v: u64, what: &str) -> Result<u32> {
    match u32::try_from(v) {
        Ok(x) if x != u32::MAX => Ok(x),
        _ => Err(Error::new(format!(
            "{what} ({v}) needs ZIP64, which is not supported"
        ))),
    }
}

/// Convert to a 16-bit ZIP field, refusing the ZIP64 marker value.
pub fn fit16(v: usize, what: &str) -> Result<u16> {
    match u16::try_from(v) {
        Ok(x) if x != u16::MAX => Ok(x),
        _ => Err(Error::new(format!(
            "{what} ({v}) does not fit in a ZIP header"
        ))),
    }
}

/// One central-directory record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdEntry {
    pub name: String,
    pub crc: u32,
    /// Absolute offset of the local file header.
    pub header: u64,
    /// Absolute location of the stored bytes.
    pub data: Span,
}

/// Appends stored entries to an existing byte buffer, then the central
/// directory and EOCD.
pub struct ZipWriter {
    out: Vec<u8>,
    entries: Vec<CdEntry>,
}

impl ZipWriter {
    /// `prefix` is everything that precedes the first local header.
    pub fn new(prefix: Vec<u8>) -> Self {
        ZipWriter {
            out: prefix,
            entries: Vec::new(),
        }
    }

    /// Store `data` under `name`; returns the absolute span of the bytes.
    pub fn add(&mut self, name: &str, data: &[u8]) -> Result<Span> {
        let header = self.out.len() as u64;
        fit32(header, "local header offset")?;
        let size = fit32(data.len() as u64, "entry size")?;
        let name_len = fit16(name.len(), "entry name length")?;
        let crc = crc32(data);
        let o = &mut self.out;
        o.extend_from_slice(&LOCAL_SIG.to_le_bytes());
        o.extend_from_slice(&VERSION_NEEDED.to_le_bytes());
        o.extend_from_slice(&0u16.to_le_bytes()); // flags
        o.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        o.extend_from_slice(&0u16.to_le_bytes()); // time
        o.extend_from_slice(&DOS_DATE.to_le_bytes());
        o.extend_from_slice(&crc.to_le_bytes());
        o.extend_from_slice(&size.to_le_bytes());
        o.extend_from_slice(&size.to_le_bytes());
        o.extend_from_slice(&name_len.to_le_bytes());
        o.extend_from_slice(&0u16.to_le_bytes()); // extra length
        o.extend_from_slice(name.as_bytes());
        let span = Span {
            off: o.len() as u64,
            len: data.len() as u64,
        };
        o.extend_from_slice(data);
        self.entries.push(CdEntry {
            name: name.to_owned(),
            crc,
            header,
            data: span,
        });
        Ok(span)
    }

    /// Absolute offset at which the EOCD record will be written by `finish`.
    pub fn eocd_offset(&self) -> u64 {
        let cd: usize = self.entries.iter().map(|e| CD_LEN + e.name.len()).sum();
        (self.out.len() + cd) as u64
    }

    /// Write the central directory and EOCD (with `comment`).
    pub fn finish(mut self, comment: &[u8]) -> Result<Vec<u8>> {
        let count = fit16(self.entries.len(), "entry count")?;
        let comment_len = fit16(comment.len(), "EOCD comment length")?;
        let cd_offset = fit32(self.out.len() as u64, "central directory offset")?;
        let o = &mut self.out;
        for e in &self.entries {
            o.extend_from_slice(&CD_SIG.to_le_bytes());
            o.extend_from_slice(&VERSION_MADE_BY.to_le_bytes());
            o.extend_from_slice(&VERSION_NEEDED.to_le_bytes());
            o.extend_from_slice(&0u16.to_le_bytes()); // flags
            o.extend_from_slice(&0u16.to_le_bytes()); // method
            o.extend_from_slice(&0u16.to_le_bytes()); // time
            o.extend_from_slice(&DOS_DATE.to_le_bytes());
            o.extend_from_slice(&e.crc.to_le_bytes());
            o.extend_from_slice(&(e.data.len as u32).to_le_bytes());
            o.extend_from_slice(&(e.data.len as u32).to_le_bytes());
            o.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            o.extend_from_slice(&[0; 8]); // extra len, comment len, disk, internal attrs
            o.extend_from_slice(&EXEC_MODE.to_le_bytes());
            o.extend_from_slice(&(e.header as u32).to_le_bytes());
            o.extend_from_slice(e.name.as_bytes());
        }
        let cd_len = o.len() as u64 - u64::from(cd_offset);
        let cd_size = fit32(cd_len, "central directory size")?;
        fit32(o.len() as u64, "EOCD offset")?;
        o.extend_from_slice(&EOCD_SIG.to_le_bytes());
        o.extend_from_slice(&[0; 4]); // this disk, CD disk
        o.extend_from_slice(&count.to_le_bytes());
        o.extend_from_slice(&count.to_le_bytes());
        o.extend_from_slice(&cd_size.to_le_bytes());
        o.extend_from_slice(&cd_offset.to_le_bytes());
        o.extend_from_slice(&comment_len.to_le_bytes());
        o.extend_from_slice(comment);
        Ok(self.out)
    }
}

/// Parsed end-of-central-directory record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Eocd {
    /// Absolute offset of the `PK\x05\x06` signature.
    pub offset: u64,
    pub entries: u16,
    pub cd_size: u32,
    pub cd_offset: u32,
    pub comment: Span,
}

/// Locate the EOCD by scanning backwards from the end of `data`, the same way
/// every ZIP reader does. The comment must end exactly at `data.len()`.
pub fn find_eocd(data: &[u8]) -> Result<Eocd> {
    let last = data
        .len()
        .checked_sub(EOCD_LEN)
        .ok_or_else(|| Error::new("file is too small to contain a ZIP archive"))?;
    let first = last.saturating_sub(MAX_COMMENT);
    let pos = (first..=last)
        .rev()
        .find(|&p| {
            le32(data, p) == Some(EOCD_SIG) && le16(data, p + 20).map(usize::from) == Some(last - p)
        })
        .ok_or_else(|| Error::new("no ZIP end-of-central-directory record found"))?;
    let f16 = |at| le16(data, pos + at).unwrap_or(0);
    let f32 = |at| le32(data, pos + at).unwrap_or(0);
    let eocd = Eocd {
        offset: pos as u64,
        entries: f16(10),
        cd_size: f32(12),
        cd_offset: f32(16),
        comment: Span {
            off: (pos + EOCD_LEN) as u64,
            len: u64::from(f16(20)),
        },
    };
    if eocd.entries == u16::MAX || eocd.cd_size == u32::MAX || eocd.cd_offset == u32::MAX {
        return Err(Error::new("ZIP64 archives are not supported"));
    }
    if f16(4) != 0 || f16(6) != 0 || f16(8) != eocd.entries {
        return Err(Error::new("multi-disk ZIP archives are not supported"));
    }
    if u64::from(eocd.cd_offset) + u64::from(eocd.cd_size) > eocd.offset {
        return Err(Error::new("ZIP central directory overlaps the EOCD record"));
    }
    Ok(eocd)
}

/// Parse one central-directory record at `at`; `None` on any structural damage.
fn cd_record(data: &[u8], at: usize, cd_end: usize, limit: u64) -> Option<(CdEntry, u16, usize)> {
    let rec = data.get(at..cd_end)?;
    if le32(rec, 0)? != CD_SIG {
        return None;
    }
    let method = le16(rec, 10)?;
    let crc = le32(rec, 16)?;
    let csize = le32(rec, 20)?;
    let name_len = usize::from(le16(rec, 28)?);
    let skip = usize::from(le16(rec, 30)?) + usize::from(le16(rec, 32)?);
    let header = le32(rec, 42)? as usize;
    let name = std::str::from_utf8(rec.get(CD_LEN..CD_LEN + name_len)?).ok()?;
    let local = data.get(header..)?;
    if le32(local, 0)? != LOCAL_SIG || csize != le32(rec, 24)? {
        return None;
    }
    let off = header + LOCAL_LEN + usize::from(le16(local, 26)?) + usize::from(le16(local, 28)?);
    let span = Span {
        off: off as u64,
        len: u64::from(csize),
    };
    (span.off + span.len <= limit).then_some(())?;
    let entry = CdEntry {
        name: name.to_owned(),
        crc,
        header: header as u64,
        data: span,
    };
    Some((entry, method, at + CD_LEN + name_len + skip))
}

/// Read and validate the central directory described by `eocd`.
pub fn read_central_directory(data: &[u8], eocd: &Eocd) -> Result<Vec<CdEntry>> {
    let mut at = eocd.cd_offset as usize;
    let end = at + eocd.cd_size as usize;
    let mut out = Vec::with_capacity(usize::from(eocd.entries));
    for i in 0..eocd.entries {
        let (entry, method, next) = cd_record(data, at, end, eocd.offset)
            .ok_or_else(|| Error::new(format!("corrupt ZIP central directory (entry {i})")))?;
        if method != 0 {
            return Err(Error::new(format!(
                "ZIP entry {:?} is compressed; only stored entries are supported",
                entry.name
            )));
        }
        out.push(entry);
        at = next;
    }
    if at != end {
        return Err(Error::new(
            "ZIP central directory size does not match its entries",
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::put32;

    fn sample() -> Vec<u8> {
        let mut w = ZipWriter::new(b"PREFIX".to_vec());
        let a = w.add("a.bin", b"hello").unwrap();
        let b = w.add("dir/b", b"").unwrap();
        assert_eq!(
            a,
            Span {
                off: 6 + 30 + 5,
                len: 5
            }
        );
        assert_eq!(b.len, 0);
        let eocd = w.eocd_offset();
        let out = w.finish(b"poly=1\n").unwrap();
        assert_eq!(&out[eocd as usize..eocd as usize + 4], b"PK\x05\x06");
        out
    }

    #[test]
    fn crc32_known_vectors() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn roundtrip() {
        let z = sample();
        let eocd = find_eocd(&z).unwrap();
        assert_eq!(eocd.entries, 2);
        assert_eq!(eocd.comment.slice(&z).unwrap(), b"poly=1\n");
        let cd = read_central_directory(&z, &eocd).unwrap();
        assert_eq!(cd.len(), 2);
        assert_eq!(cd[0].name, "a.bin");
        assert_eq!(cd[0].header, 6);
        assert_eq!(cd[0].data.slice(&z).unwrap(), b"hello");
        assert_eq!(cd[0].crc, crc32(b"hello"));
        assert_eq!(cd[1].name, "dir/b");
    }

    #[test]
    fn eocd_scan_errors() {
        let e = find_eocd(b"short").unwrap_err();
        assert!(e.0.contains("too small"));
        let e = find_eocd(&[0u8; 100]).unwrap_err();
        assert!(e.0.contains("no ZIP end"));
        // Trailing garbage after the comment hides the EOCD.
        let mut z = sample();
        z.push(b'x');
        assert!(find_eocd(&z).is_err());
    }

    fn eocd_pos(z: &[u8]) -> usize {
        find_eocd(z).unwrap().offset as usize
    }

    #[test]
    fn eocd_zip64_marker() {
        for field in [10usize, 12, 16] {
            let mut z = sample();
            let p = eocd_pos(&z);
            z[p + field] = 0xFF;
            z[p + field + 1] = 0xFF;
            if field != 10 {
                put32(&mut z, p + field, u32::MAX);
            }
            assert!(
                find_eocd(&z).unwrap_err().0.contains("ZIP64"),
                "field {field}"
            );
        }
    }

    #[test]
    fn eocd_multi_disk_and_overlap() {
        for field in [4usize, 6, 8] {
            let mut z = sample();
            let p = eocd_pos(&z);
            z[p + field] = 1;
            assert!(
                find_eocd(&z).unwrap_err().0.contains("multi-disk"),
                "field {field}"
            );
        }
        let mut z = sample();
        let p = eocd_pos(&z);
        put32(&mut z, p + 12, 10_000);
        assert!(find_eocd(&z).unwrap_err().0.contains("overlaps"));
    }

    fn cd_err(mutate: impl Fn(&mut Vec<u8>, &Eocd)) -> String {
        let mut z = sample();
        let eocd = find_eocd(&z).unwrap();
        mutate(&mut z, &eocd);
        read_central_directory(&z, &eocd).unwrap_err().0
    }

    #[test]
    fn central_directory_corruption() {
        let cd = |e: &Eocd| e.cd_offset as usize;
        // Bad record signature.
        assert!(cd_err(|z, e| z[cd(e)] = b'X').contains("corrupt"));
        // Compressed entry.
        assert!(cd_err(|z, e| z[cd(e) + 10] = 8).contains("compressed"));
        // Name not UTF-8.
        assert!(cd_err(|z, e| z[cd(e) + CD_LEN] = 0xFF).contains("corrupt"));
        // Local header offset points at garbage.
        assert!(cd_err(|z, e| put32(z, cd(e) + 42, 1)).contains("corrupt"));
        // Local header offset beyond EOF.
        assert!(cd_err(|z, e| put32(z, cd(e) + 42, 1 << 30)).contains("corrupt"));
        // Compressed size != uncompressed size.
        assert!(cd_err(|z, e| put32(z, cd(e) + 20, 4)).contains("corrupt"));
        // Data runs into the central directory.
        assert!(
            cd_err(|z, e| {
                put32(z, cd(e) + 20, 1000);
                put32(z, cd(e) + 24, 1000);
            })
            .contains("corrupt")
        );
        // Name length runs past the central directory.
        assert!(cd_err(|z, e| z[cd(e) + 29] = 0x10).contains("corrupt"));
        // Truncated reads inside the record (header at very end of the CD slice).
        assert!(
            cd_err(|z, e| {
                let near_eof = (z.len() - 2) as u32;
                put32(z, cd(e) + 42, near_eof)
            })
            .contains("corrupt")
        );
    }

    #[test]
    fn central_directory_size_mismatch() {
        let mut z = sample();
        let mut eocd = find_eocd(&z).unwrap();
        eocd.entries = 1;
        assert!(
            read_central_directory(&z, &eocd)
                .unwrap_err()
                .0
                .contains("size does not match")
        );
        // A record truncated by a too-small cd_size.
        eocd.entries = 2;
        eocd.cd_size = 10;
        z.truncate(z.len());
        assert!(
            read_central_directory(&z, &eocd)
                .unwrap_err()
                .0
                .contains("corrupt")
        );
    }

    #[test]
    fn writer_limits() {
        assert_eq!(fit32(5, "x"), Ok(5));
        assert!(
            fit32(u64::from(u32::MAX), "x")
                .unwrap_err()
                .0
                .contains("ZIP64")
        );
        assert!(fit32(1 << 40, "x").is_err());
        assert_eq!(fit16(5, "x"), Ok(5));
        assert!(fit16(0xFFFF, "x").is_err());
        let mut w = ZipWriter::new(Vec::new());
        assert!(w.add(&"n".repeat(0x1_0000), b"").is_err());
        let w = ZipWriter::new(Vec::new());
        assert!(w.finish(&vec![b' '; MAX_COMMENT + 1]).is_err());
    }

    #[test]
    fn interop_with_system_unzip_listing_format() {
        // Mode bits and version-made-by are what `unzip` uses to restore +x.
        let z = sample();
        let eocd = find_eocd(&z).unwrap();
        let cd = eocd.cd_offset as usize;
        assert_eq!(le16(&z, cd + 4), Some(VERSION_MADE_BY));
        assert_eq!(le32(&z, cd + 38), Some(EXEC_MODE));
    }
}
