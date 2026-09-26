//! NTFS volume access: open volume handles, enumerate all file records via
//! `FSCTL_ENUM_USN_DATA` (MFT walk) and stream changes from the USN journal.
//!
//! The handful of kernel32 entry points are declared by hand: their ABI is
//! frozen, and this avoids any dependency on windows-rs struct/enum drift.
//! Requires an elevated token (the service runs as LocalSystem).

pub mod records;
pub mod sec;

use records::{rd_u16, rd_u32, rd_u64, scan_records, RawRecord, RecordScan};
use std::cell::Cell;
use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use std::ptr::{null, null_mut};
use wfs_core::JournalPos;

// FSCTL control codes: CTL_CODE(FILE_DEVICE_FILE_SYSTEM = 0x0009, function,
// method, FILE_ANY_ACCESS), i.e. 0x0009_0000 | function << 2 | method. The
// function numbers are frozen and checked against winioctl.h. Note that
// function 60 is FSCTL_EXTEND_VOLUME, not a journal ioctl: asking it for
// journal data is rejected by NTFS (win32 error 87) and, on a driver that
// took the request at face value, would grow the volume.
pub const FSCTL_ENUM_USN_DATA: u32 = 0x0009_00b3; // fn 44, METHOD_NEITHER
pub const FSCTL_CREATE_USN_JOURNAL: u32 = 0x0009_00e7; // fn 57, METHOD_NEITHER
pub const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00bb; // fn 46, METHOD_NEITHER
pub const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00f4; // fn 61, METHOD_BUFFERED

// Win32 error codes we branch on
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_NOT_READY: u32 = 21;
const ERROR_HANDLE_EOF: u32 = 38;
// 1178 = ERROR_JOURNAL_DELETE_IN_PROGRESS, 1179 = ERROR_JOURNAL_NOT_ACTIVE
// (no journal on this volume), 1181 = ERROR_JOURNAL_ENTRY_DELETED (the USN we
// asked for is older than LowestValidUsn).
const ERROR_JOURNAL_DELETE_IN_PROGRESS: u32 = 1178;
const ERROR_JOURNAL_NOT_ACTIVE: u32 = 1179;
const ERROR_JOURNAL_ENTRY_DELETED: u32 = 1181;
// A driver that rejects our input buffer for being too small (the versioned
// structs are longer than the V0 ones) answers with one of these.
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_INVALID_USER_BUFFER: u32 = 1784;

// CreateFileW constants
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_SHARE_READ: u32 = 0x0000_0001;
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
const OPEN_EXISTING: u32 = 3;

const DRIVE_FIXED: u32 = 3;

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileW(
        lpfilename: *const u16,
        dwdesiredaccess: u32,
        dwsharemode: u32,
        lpsecurityattributes: *const u8,
        dwcreationdisposition: u32,
        dwflagsandattributes: u32,
        htemplatefile: isize,
    ) -> isize;
    fn DeviceIoControl(
        hdevice: isize,
        dwiocontrolcode: u32,
        lpinbuffer: *const u8,
        ninbuffersize: u32,
        lpoutbuffer: *mut u8,
        noutbuffersize: u32,
        lpbytesreturned: *mut u32,
        lpoverlapped: *mut u8,
    ) -> i32;
    fn CloseHandle(hobject: isize) -> i32;
    fn GetLogicalDrives() -> u32;
    fn GetDriveTypeW(lprootpathname: *const u16) -> u32;
    fn GetLastError() -> u32;
}

#[derive(Debug, Clone)]
pub enum FsError {
    /// USN journal deleted or wrapped — full rebuild required
    JournalGone,
    AccessDenied,
    /// volume not ready (ejected, spun down)
    NotReady,
    /// stream/enum completed
    Eof,
    Other(String),
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FsError::JournalGone => write!(f, "USN journal deleted or wrapped"),
            FsError::AccessDenied => write!(f, "access denied (elevation required)"),
            FsError::NotReady => write!(f, "volume not ready"),
            FsError::Eof => write!(f, "end of stream"),
            FsError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for FsError {}

fn win_err(code: u32) -> FsError {
    match code {
        ERROR_JOURNAL_DELETE_IN_PROGRESS | ERROR_JOURNAL_ENTRY_DELETED => FsError::JournalGone,
        // No journal on the volume is a state the caller can fix by creating
        // one, so it is reported as JournalGone too (there is nothing to
        // replay either way).
        ERROR_JOURNAL_NOT_ACTIVE => FsError::JournalGone,
        ERROR_ACCESS_DENIED => FsError::AccessDenied,
        ERROR_NOT_READY => FsError::NotReady,
        ERROR_HANDLE_EOF => FsError::Eof,
        other => FsError::Other(format!("win32 error {other}")),
    }
}

fn wide(s: &str) -> Vec<u16> {
    OsString::from(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// One `DeviceIoControl` round trip; `Err` carries the raw Win32 error code.
fn dio_on(
    handle: isize,
    code: u32,
    inb: Option<&[u8]>,
    out: Option<&mut [u8]>,
) -> Result<usize, u32> {
    let (iptr, isz) = match inb {
        Some(b) => (b.as_ptr(), b.len() as u32),
        None => (null(), 0),
    };
    let (optr, osz) = match out {
        Some(b) => (b.as_mut_ptr(), b.len() as u32),
        None => (null_mut(), 0),
    };
    let mut returned: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            code,
            iptr,
            isz,
            optr,
            osz,
            &mut returned,
            null_mut(),
        )
    };
    if ok != 0 {
        Ok(returned as usize)
    } else {
        Err(unsafe { GetLastError() })
    }
}

/// Snapshot of `FSCTL_QUERY_USN_JOURNAL` output.
#[derive(Debug, Clone, Copy)]
pub struct JournalInfo {
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub lowest_valid_usn: i64,
    pub max_usn: i64,
    pub maximum_size: u64,
    pub allocation_delta: u64,
    /// `(MinSupportedMajorVersion, MaxSupportedMajorVersion)` — the USN_RECORD
    /// versions the volume's journal can emit. `(0, 0)` when the driver
    /// answered with the V0 layout, which carries no version fields.
    pub record_versions: (u16, u16),
}

/// One file record from the MFT enumeration.
#[derive(Debug, Clone)]
pub struct MftEntry {
    pub frn: u64,
    pub parent_frn: u64,
    pub name: String,
    pub is_dir: bool,
}

/// Where the journal reader should continue from, given a persisted position
/// and the live journal. `None` means the position cannot be replayed and the
/// volume has to be rebuilt from the MFT — either the journal object is a
/// different one, or it wrapped past the persisted `next_usn`, losing records.
pub fn resume_position(saved: JournalPos, live: &JournalInfo) -> Option<JournalPos> {
    if live.journal_id != saved.journal_id {
        return None;
    }
    if (saved.next_usn as i64) < live.lowest_valid_usn {
        return None;
    }
    Some(JournalPos {
        journal_id: saved.journal_id,
        next_usn: saved.next_usn.min(live.next_usn as u64),
    })
}

/// Open NTFS volume handle (`\\.\C:`). One per volume; not thread-safe, keep
/// it on the owning thread.
pub struct VolumeHandle {
    drive: char,
    handle: isize,
    /// Length of the `MFT_ENUM_DATA` input last handed to the driver (24 = V0,
    /// 32 = V1); 0 before the first enumeration. Diagnostic only.
    enum_input_len: Cell<u32>,
}

// The raw HANDLE is process-global; we only move it between threads to hand
// it to its owning worker.
unsafe impl Send for VolumeHandle {}

impl VolumeHandle {
    pub fn open(drive: char) -> Result<VolumeHandle, FsError> {
        Self::open_with(drive, GENERIC_READ)
    }

    fn open_with(drive: char, access: u32) -> Result<VolumeHandle, FsError> {
        if !drive.is_ascii_alphabetic() {
            return Err(FsError::Other(format!("invalid drive '{drive}'")));
        }
        let path = wide(&format!(r"\\.\{drive}:"));
        let h = unsafe {
            CreateFileW(
                path.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                0,
                0,
            )
        };
        if h == -1 {
            return Err(win_err(unsafe { GetLastError() }));
        }
        Ok(VolumeHandle {
            drive,
            handle: h,
            enum_input_len: Cell::new(0),
        })
    }

    /// `MFT_ENUM_DATA` input length in use, or 0 when no enumeration has run
    /// yet.
    pub fn enum_input_len(&self) -> u32 {
        self.enum_input_len.get()
    }

    fn dio(&self, code: u32, inb: Option<&[u8]>, out: Option<&mut [u8]>) -> Result<usize, u32> {
        dio_on(self.handle, code, inb, out)
    }

    /// Read the whole `USN_JOURNAL_DATA` the driver has for us. The output
    /// buffer is deliberately larger than `USN_JOURNAL_DATA_V0` (56 B): the V1
    /// and V2 layouts are prefixes-compatible extensions, and a driver that
    /// wants to hand out V1 refuses a buffer sized for V0 alone.
    pub fn journal_info(&self) -> Result<JournalInfo, FsError> {
        let mut out = [0u8; 128];
        match self.dio(FSCTL_QUERY_USN_JOURNAL, None, Some(&mut out)) {
            Ok(n) if n >= 56 => Ok(JournalInfo {
                journal_id: rd_u64(&out, 0),
                first_usn: rd_u64(&out, 8) as i64,
                next_usn: rd_u64(&out, 16) as i64,
                lowest_valid_usn: rd_u64(&out, 24) as i64,
                max_usn: rd_u64(&out, 32) as i64,
                maximum_size: rd_u64(&out, 40),
                allocation_delta: rd_u64(&out, 48),
                record_versions: if n >= 60 {
                    (rd_u16(&out, 56), rd_u16(&out, 58))
                } else {
                    (0, 0)
                },
            }),
            Ok(n) => Err(FsError::Other(format!(
                "short FSCTL_QUERY_USN_JOURNAL reply ({n} bytes)"
            ))),
            Err(e) => Err(win_err(e)),
        }
    }

    /// Enable the USN journal if absent (64 MB, 16 MB delta — reduces wrap
    /// frequency), then return its info. Only the volume's own "no journal"
    /// answer is treated as a reason to create one; every other failure is
    /// reported unchanged, so a diagnostic never gets replaced by a misleading
    /// second error from the create attempt.
    pub fn ensure_journal(&self) -> Result<JournalInfo, FsError> {
        match self.journal_info() {
            Ok(j) => return Ok(j),
            Err(FsError::JournalGone) => {}
            Err(e) => return Err(e),
        }
        let mut inb = [0u8; 16];
        inb[0..8].copy_from_slice(&0x0400_0000u64.to_le_bytes()); // MaximumSize 64MB
        inb[8..16].copy_from_slice(&0x0100_0000u64.to_le_bytes()); // AllocationDelta 16MB
                                                                   // Creating a journal modifies the volume, so it needs a writable
                                                                   // handle; the long-lived one is read-only.
        let writable = Self::open_with(self.drive, GENERIC_READ | GENERIC_WRITE).ok();
        let h = writable.as_ref().map_or(self.handle, |v| v.handle);
        dio_on(h, FSCTL_CREATE_USN_JOURNAL, Some(&inb), None).map_err(|code| {
            if writable.is_none() {
                FsError::Other(format!(
                    "volume {}: cannot be opened for writing, so the missing USN journal \
                     cannot be created (win32 error {code})",
                    self.drive
                ))
            } else {
                win_err(code)
            }
        })?;
        self.journal_info()
    }

    /// Enumerate every file record on the volume (the whole MFT). Blocking;
    /// typically a few seconds per million files on an SSD. Metafiles
    /// (FRN < 24) and the root record are filtered out.
    pub fn enumerate_mft(&self, mut sink: impl FnMut(MftEntry)) -> Result<(), FsError> {
        const BUF: usize = 1 << 24; // 16MB output buffer
        let mut buf = vec![0u8; BUF];
        // MFT_ENUM_DATA_V0 { StartFileReferenceNumber, LowUsn, HighUsn } is the
        // documented 24-byte baseline; MFT_ENUM_DATA_V1 adds MinMajorVersion /
        // MaxMajorVersion (2 = USN_RECORD_V2). Newer drivers reject the V0
        // length outright, so start with V0 and retry once with V1.
        let mut med = [0u8; 32];
        // HighUsn is a USN, i.e. signed: -1 (u64::MAX) is *below* every real
        // USN and filters out the whole MFT, so the enumeration comes back as
        // an immediate end-of-file. MAXLONGLONG is the documented "all".
        med[16..24].copy_from_slice(&i64::MAX.to_le_bytes()); // HighUsn
        med[24..26].copy_from_slice(&2u16.to_le_bytes()); // MinMajorVersion
        med[26..28].copy_from_slice(&2u16.to_le_bytes()); // MaxMajorVersion
        let mut len = 24usize;
        let mut start_frn: u64 = 0;
        loop {
            med[0..8].copy_from_slice(&start_frn.to_le_bytes());
            // Recorded before the call so a failure still names the struct the
            // driver was handed (V0 vs the fallback V1).
            self.enum_input_len.set(len as u32);
            let returned = match self.dio(FSCTL_ENUM_USN_DATA, Some(&med[..len]), Some(&mut buf)) {
                Ok(n) => n,
                Err(ERROR_HANDLE_EOF) => return Ok(()),
                Err(e)
                    if len == 24
                        && (e == ERROR_INVALID_PARAMETER || e == ERROR_INVALID_USER_BUFFER) =>
                {
                    len = 32; // the driver wants the versioned struct
                    continue;
                }
                Err(e) => return Err(win_err(e)),
            };
            if returned < 8 {
                return Ok(());
            }
            let next = walk_enum_batch(&buf, returned, &mut sink);
            if returned == 8 && next <= start_frn {
                return Ok(()); // nothing left and no forward progress
            }
            start_frn = next.max(start_frn + 1);
        }
    }

    /// Read pending journal records starting at `pos`. Returns the parsed
    /// events, how many raw records the reply carried (so a caller can tell
    /// "nothing happened" from "records we cannot read"), and the updated
    /// position. Never returns partial buffers: the whole output buffer is
    /// consumed before advancing `next_usn`.
    pub fn read_journal(
        &self,
        pos: JournalPos,
        max_events: usize,
    ) -> Result<(RecordScan, JournalPos), FsError> {
        const BUF: usize = 1 << 20; // 1MB per read
        let mut out = vec![0u8; BUF];
        let mut scan = RecordScan::default();
        let mut cur = pos;
        // READ_USN_JOURNAL_DATA_V1 is 48 bytes and pins the record version we
        // can parse (Min = Max = USN_RECORD_V2); V0 is 40 bytes and leaves the
        // version to the driver. Ask for V2 first — a driver that answers a V0
        // request with V3 records makes every event unreadable — and fall back
        // to V0 for drivers that reject the longer struct.
        let mut inb = [0u8; 48];
        inb[8..12].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // all reasons
        inb[40..42].copy_from_slice(&2u16.to_le_bytes()); // MinMajorVersion
        inb[42..44].copy_from_slice(&2u16.to_le_bytes()); // MaxMajorVersion
        let mut len = 48usize;
        loop {
            inb[0..8].copy_from_slice(&(cur.next_usn as i64).to_le_bytes());
            // Timeout (16..24) and BytesToWaitFor (24..32) stay zero: return
            // immediately with whatever is pending.
            inb[32..40].copy_from_slice(&cur.journal_id.to_le_bytes());

            let returned = match self.dio(FSCTL_READ_USN_JOURNAL, Some(&inb[..len]), Some(&mut out))
            {
                Ok(n) => n,
                // Nothing pending: the position is unchanged, so the caller
                // simply polls again. Reporting this as an error turns an idle
                // volume into a warning flood that buries real ones.
                Err(ERROR_HANDLE_EOF) => return Ok((scan, cur)),
                Err(e)
                    if len == 48
                        && (e == ERROR_INVALID_PARAMETER || e == ERROR_INVALID_USER_BUFFER) =>
                {
                    len = 40; // driver only knows the V0 struct
                    continue;
                }
                Err(e) => return Err(win_err(e)),
            };
            if returned < 8 {
                return Ok((scan, cur));
            }
            cur.next_usn = rd_u64(&out, 0);
            scan.merge(scan_records(&out[8..returned]));
            if returned > 8 && scan.events.len() < max_events {
                continue; // more records may be pending
            }
            return Ok((scan, cur));
        }
    }
}

impl Drop for VolumeHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

/// Walk one `FSCTL_ENUM_USN_DATA` reply: the buffer opens with a DWORDLONG —
/// the `StartFileReferenceNumber` the next call must use — and the records
/// follow. Parsing from offset 0 would read that FRN as a record length and
/// desynchronize everything after it. Returns the next start FRN.
fn walk_enum_batch(buf: &[u8], returned: usize, mut sink: impl FnMut(MftEntry)) -> u64 {
    let next = rd_u64(buf, 0);
    let mut off = 8usize;
    while off + 8 <= returned {
        let reclen = rd_u32(buf, off) as usize;
        if reclen == 0 || off + reclen > returned {
            break;
        }
        if let Some(rec) = RawRecord::parse(&buf[off..off + reclen]) {
            if rec.frn >= records::MFT_METAFILE_MAX_FRN {
                sink(MftEntry {
                    frn: rec.frn,
                    parent_frn: rec.parent_frn,
                    name: rec.name_string(),
                    is_dir: rec.is_dir(),
                });
            }
        }
        off += reclen;
    }
    next
}

/// All drive letters backed by fixed disks (NTFS or otherwise — the caller
/// probes journal support per volume).
pub fn detect_fixed_drives() -> Vec<char> {
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) != 0 {
            let letter = (b'A' + i as u8) as char;
            let root = wide(&format!("{letter}:\\"));
            if unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_FIXED {
                out.push(letter);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal USN_RECORD_V2 (60-byte header + UTF-16 name).
    fn record(frn: u64, parent: u64, name: &str, attrs: u32) -> Vec<u8> {
        let name: Vec<u16> = name.encode_utf16().collect();
        let mut r = vec![0u8; 60 + name.len() * 2];
        let len = r.len() as u32;
        r[0..4].copy_from_slice(&len.to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes()); // MajorVersion = 2
        r[8..16].copy_from_slice(&frn.to_le_bytes());
        r[16..24].copy_from_slice(&parent.to_le_bytes());
        r[52..56].copy_from_slice(&attrs.to_le_bytes());
        r[56..58].copy_from_slice(&((name.len() * 2) as u16).to_le_bytes());
        r[58..60].copy_from_slice(&60u16.to_le_bytes());
        for (i, c) in name.iter().enumerate() {
            r[60 + i * 2..62 + i * 2].copy_from_slice(&c.to_le_bytes());
        }
        r
    }

    fn info(journal_id: u64, lowest_valid_usn: i64, next_usn: i64) -> JournalInfo {
        JournalInfo {
            journal_id,
            first_usn: 0,
            next_usn,
            lowest_valid_usn,
            max_usn: 0,
            maximum_size: 0,
            allocation_delta: 0,
            record_versions: (2, 2),
        }
    }

    #[test]
    fn resume_requires_the_same_journal() {
        let saved = JournalPos {
            journal_id: 7,
            next_usn: 500,
        };
        assert_eq!(resume_position(saved, &info(7, 100, 900)), Some(saved));
        assert_eq!(resume_position(saved, &info(8, 100, 900)), None);
    }

    #[test]
    fn resume_rejected_after_wrap() {
        let saved = JournalPos {
            journal_id: 7,
            next_usn: 50,
        };
        // LowestValidUsn moved past the saved position: those records are gone
        assert_eq!(resume_position(saved, &info(7, 100, 900)), None);
    }

    #[test]
    fn resume_clamps_to_the_live_next_usn() {
        let saved = JournalPos {
            journal_id: 7,
            next_usn: 5000,
        };
        assert_eq!(
            resume_position(saved, &info(7, 100, 900)),
            Some(JournalPos {
                journal_id: 7,
                next_usn: 900
            })
        );
    }

    #[test]
    fn enum_batch_starts_after_the_leading_next_frn() {
        let mut buf = 4242u64.to_le_bytes().to_vec();
        buf.extend(record(5, 5, "$MFT", 0)); // metafile: filtered out
        buf.extend(record(100, 5, "notes.txt", 0));
        buf.extend(record(101, 100, "work", 0x10)); // FILE_ATTRIBUTE_DIRECTORY
        let mut seen = Vec::new();
        let next = walk_enum_batch(&buf, buf.len(), |e| {
            seen.push((e.frn, e.parent_frn, e.name, e.is_dir));
        });
        assert_eq!(next, 4242);
        assert_eq!(
            seen,
            vec![
                (100, 5, "notes.txt".to_string(), false),
                (101, 100, "work".to_string(), true)
            ]
        );
    }

    #[test]
    fn sequence_bits_do_not_confuse_the_walk() {
        // File references arrive as (seq << 48) | record; metafile filtering
        // and parent anchoring must see plain record numbers.
        let seq = 0x0007_0000_0000_0000u64;
        let mut buf = 99u64.to_le_bytes().to_vec();
        buf.extend(record(seq | 9, seq | 5, "$Secure", 0)); // metafile: filtered
        buf.extend(record(seq | 100, seq | 5, "root-child.txt", 0));
        let mut seen = Vec::new();
        walk_enum_batch(&buf, buf.len(), |e| {
            seen.push((e.frn, e.parent_frn, e.name, e.is_dir));
        });
        assert_eq!(seen, vec![(100, 5, "root-child.txt".to_string(), false)]);
    }

    #[test]
    fn enum_batch_stops_at_a_truncated_record() {
        let mut buf = 7u64.to_le_bytes().to_vec();
        buf.extend(record(100, 5, "notes.txt", 0));
        buf.extend([0xff, 0xff, 0xff, 0xff]); // a record header cut short
        let mut seen = 0;
        let next = walk_enum_batch(&buf, buf.len(), |_| seen += 1);
        assert_eq!(next, 7);
        assert_eq!(seen, 1);
    }
}
