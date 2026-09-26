//! Raw USN_RECORD_V2 parsing (manual byte layout — record offsets are fixed
//! by NTFS, so we don't depend on any Win32 binding's struct definitions).
//!
//! V2 record layout (offsets in bytes):
//! ```text
//!  0  RecordLength        u32
//!  4  MajorVersion        u16   (=2)
//!  6  MinorVersion        u16
//!  8  FileReferenceNumber u64
//! 16  ParentFileReferenceNumber u64
//! 24  Usn                 i64
//! 32  TimeStamp           i64
//! 40  Reason              u32
//! 44  SourceInfo          u32
//! 48  SecurityId          u32
//! 52  FileAttributes      u32
//! 56  FileNameLength      u16   (bytes)
//! 58  FileNameOffset      u16   (relative to record start)
//! 60  FileName            UTF-16LE
//! ```

use wfs_core::IndexEvent;

pub const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
pub const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
pub const USN_REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
pub const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;

/// Records with FRN below this are NTFS metafiles ($MFT, $LogFile, …) and are
/// never indexed. User files cannot live in the first 24 MFT records.
pub const MFT_METAFILE_MAX_FRN: u64 = 24;

#[inline]
pub fn rd_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

#[inline]
pub fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[inline]
pub fn rd_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

/// One parsed USN_RECORD_V2; `name` is the UTF-16LE byte slice borrowed from
/// the underlying buffer.
#[derive(Debug, Clone)]
pub struct RawRecord<'a> {
    pub frn: u64,
    pub parent_frn: u64,
    pub reason: u32,
    pub attrs: u32,
    name: &'a [u8],
}

impl<'a> RawRecord<'a> {
    pub fn parse(rec: &'a [u8]) -> Option<RawRecord<'a>> {
        if rec.len() < 60 {
            return None;
        }
        let reclen = rd_u32(rec, 0) as usize;
        if reclen == 0 || reclen > rec.len() {
            return None;
        }
        if rd_u16(rec, 4) != 2 {
            return None; // only V2 (the V0 enum/read input always yields V2)
        }
        let nlen = rd_u16(rec, 56) as usize;
        let noff = rd_u16(rec, 58) as usize;
        if noff + nlen > rec.len() || !nlen.is_multiple_of(2) {
            return None;
        }
        Some(RawRecord {
            frn: rd_u64(rec, 8),
            parent_frn: rd_u64(rec, 16),
            reason: rd_u32(rec, 40),
            attrs: rd_u32(rec, 52),
            name: &rec[noff..noff + nlen],
        })
    }

    pub fn is_dir(&self) -> bool {
        self.attrs & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    pub fn name_string(&self) -> String {
        // `parse` rejected odd name lengths, so the tail chunk is empty.
        let units: Vec<u16> = self
            .name
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16_lossy(&units)
    }

    /// Map one raw USN record to at most one index event, skipping metafiles.
    pub fn to_event(&self) -> Option<IndexEvent> {
        if self.frn < MFT_METAFILE_MAX_FRN {
            return None;
        }
        let r = self.reason;
        if r & USN_REASON_RENAME_NEW_NAME != 0 {
            Some(IndexEvent::RenameNew {
                frn: self.frn,
                parent_frn: self.parent_frn,
                name: self.name_string(),
                is_dir: self.is_dir(),
            })
        } else if r & USN_REASON_RENAME_OLD_NAME != 0 {
            Some(IndexEvent::RenameOld { frn: self.frn })
        } else if r & USN_REASON_FILE_CREATE != 0 {
            Some(IndexEvent::Create {
                frn: self.frn,
                parent_frn: self.parent_frn,
                name: self.name_string(),
                is_dir: self.is_dir(),
            })
        } else if r & USN_REASON_FILE_DELETE != 0 {
            Some(IndexEvent::Delete { frn: self.frn })
        } else {
            None
        }
    }
}

/// Parse a whole USN read/enum buffer body (records packed back to back).
/// For journal reads, the caller skips the leading 8-byte NextUsn field.
pub fn events_from_records(data: &[u8]) -> Vec<IndexEvent> {
    let mut events = Vec::new();
    let mut off = 0usize;
    while off + 8 <= data.len() {
        let reclen = rd_u32(data, off) as usize;
        if reclen == 0 || off + reclen > data.len() {
            break;
        }
        if let Some(rec) = RawRecord::parse(&data[off..off + reclen]) {
            if let Some(ev) = rec.to_event() {
                events.push(ev);
            }
        }
        off += reclen;
    }
    events
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn w16(buf: &mut Vec<u8>, v: u16) {
        buf.extend_from_slice(&v.to_le_bytes());
    }

    fn w32(buf: &mut Vec<u8>, v: u32) {
        buf.extend_from_slice(&v.to_le_bytes());
    }

    fn w64(buf: &mut Vec<u8>, v: u64) {
        buf.extend_from_slice(&v.to_le_bytes());
    }

    fn make_record(frn: u64, parent: u64, reason: u32, attrs: u32, name: &str) -> Vec<u8> {
        let name_u16: Vec<u16> = name.encode_utf16().collect();
        let mut buf = Vec::with_capacity(60 + name_u16.len() * 2);
        let total = (60 + name_u16.len() * 2) as u32;
        w32(&mut buf, total);
        w16(&mut buf, 2); // major
        w16(&mut buf, 0); // minor
        w64(&mut buf, frn);
        w64(&mut buf, parent);
        w64(&mut buf, 42); // usn
        w64(&mut buf, 1234567); // timestamp
        w32(&mut buf, reason);
        w32(&mut buf, 0); // source info
        w32(&mut buf, 0); // security id
        w32(&mut buf, attrs);
        w16(&mut buf, (name_u16.len() * 2) as u16);
        w16(&mut buf, 60);
        for u in name_u16 {
            w16(&mut buf, u);
        }
        buf
    }

    #[test]
    fn parse_single_record() {
        let buf = make_record(
            30,
            5,
            USN_REASON_FILE_CREATE,
            FILE_ATTRIBUTE_DIRECTORY,
            "work",
        );
        let rec = RawRecord::parse(&buf).unwrap();
        assert_eq!(rec.frn, 30);
        assert_eq!(rec.parent_frn, 5);
        assert!(rec.is_dir());
        assert_eq!(rec.name_string(), "work");
        match rec.to_event().unwrap() {
            IndexEvent::Create {
                frn,
                parent_frn,
                name,
                is_dir,
            } => {
                assert_eq!(
                    (frn, parent_frn, name.as_str(), is_dir),
                    (30, 5, "work", true)
                );
            }
            _ => panic!("expected create"),
        }
    }

    #[test]
    fn cjk_name_roundtrip() {
        let buf = make_record(11, 10, USN_REASON_FILE_CREATE, 0, "项目文件夹.txt");
        let rec = RawRecord::parse(&buf).unwrap();
        assert_eq!(rec.name_string(), "项目文件夹.txt");
    }

    #[test]
    fn metafiles_are_skipped() {
        let buf = make_record(3, 5, USN_REASON_FILE_CREATE, 0, "$LogFile");
        assert!(RawRecord::parse(&buf).unwrap().to_event().is_none());
    }

    #[test]
    fn multi_record_buffer() {
        let mut buf = make_record(100, 5, USN_REASON_FILE_CREATE, 0, "a.txt");
        buf.extend_from_slice(&make_record(101, 5, USN_REASON_RENAME_NEW_NAME, 0, "b.txt"));
        let events = events_from_records(&buf);
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], IndexEvent::Create { .. }));
        assert!(matches!(events[1], IndexEvent::RenameNew { .. }));
    }

    #[test]
    fn parser_stops_cleanly_on_garbage() {
        // real USN buffers never contain junk; the parser must simply stop
        // instead of panicking or emitting garbage events
        let mut buf = make_record(100, 5, USN_REASON_FILE_CREATE, 0, "a.txt");
        buf.extend_from_slice(&[0xEE; 5]);
        let events = events_from_records(&buf);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn delete_and_rename_old() {
        let d = make_record(77, 5, USN_REASON_FILE_DELETE, 0, "gone.txt");
        assert!(matches!(
            RawRecord::parse(&d).unwrap().to_event(),
            Some(IndexEvent::Delete { frn: 77 })
        ));
        let ro = make_record(77, 5, USN_REASON_RENAME_OLD_NAME, 0, "old.txt");
        assert!(matches!(
            RawRecord::parse(&ro).unwrap().to_event(),
            Some(IndexEvent::RenameOld { frn: 77 })
        ));
    }

    #[test]
    fn truncated_record_rejected() {
        let buf = make_record(1, 2, 0, 0, "x");
        assert!(RawRecord::parse(&buf[..30]).is_none());
        assert!(RawRecord::parse(&[]).is_none());
    }
}
