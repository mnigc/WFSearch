//! NTFS volume access: open volume handles, enumerate all file records via
//! `FSCTL_ENUM_USN_DATA` (MFT walk) and stream changes from the USN journal.
//!
//! The handful of kernel32 entry points are declared by hand: their ABI is
//! frozen, and this avoids any dependency on windows-rs struct/enum drift.
//! Requires an elevated token (the service runs as LocalSystem).

pub mod records;
pub mod sec;

use records::{events_from_records, rd_u32, rd_u64, RawRecord};
use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use std::ptr::{null, null_mut};
use wfs_core::IndexEvent;
use wfs_core::JournalPos;

// FSCTL control codes (Win32::System::Ioctl values, frozen ABI)
pub const FSCTL_ENUM_USN_DATA: u32 = 0x0009_00b3;
pub const FSCTL_CREATE_USN_JOURNAL: u32 = 0x0009_00e7;
pub const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00bb;
pub const FSCTL_GET_USN_JOURNAL: u32 = 0x0009_00f0;

// Win32 error codes we branch on
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_NOT_READY: u32 = 21;
const ERROR_HANDLE_EOF: u32 = 38;
const ERROR_JOURNAL_DELETE: u32 = 1178;
const ERROR_JOURNAL_ENTRY_DELETED: u32 = 1179;

// CreateFileW constants
const GENERIC_READ: u32 = 0x8000_0000;
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
        ERROR_JOURNAL_DELETE | ERROR_JOURNAL_ENTRY_DELETED => FsError::JournalGone,
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

/// Snapshot of `FSCTL_GET_USN_JOURNAL` output.
#[derive(Debug, Clone, Copy)]
pub struct JournalInfo {
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub lowest_valid_usn: i64,
    pub max_usn: i64,
    pub maximum_size: u64,
    pub allocation_delta: u64,
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
}

// The raw HANDLE is process-global; we only move it between threads to hand
// it to its owning worker.
unsafe impl Send for VolumeHandle {}

impl VolumeHandle {
    pub fn open(drive: char) -> Result<VolumeHandle, FsError> {
        if !drive.is_ascii_alphabetic() {
            return Err(FsError::Other(format!("invalid drive '{drive}'")));
        }
        let path = wide(&format!(r"\\.\{drive}:"));
        let h = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ,
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
        Ok(VolumeHandle { drive, handle: h })
    }

    pub fn drive(&self) -> char {
        self.drive
    }

    fn dio(&self, code: u32, inb: Option<&[u8]>, mut out: Option<&mut [u8]>) -> Result<usize, u32> {
        let (iptr, isz) = match inb {
            Some(b) => (b.as_ptr(), b.len() as u32),
            None => (null(), 0),
        };
        let (optr, osz) = match out.as_mut() {
            Some(b) => (b.as_mut_ptr(), b.len() as u32),
            None => (null_mut(), 0),
        };
        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
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

    pub fn journal_info(&self) -> Result<JournalInfo, FsError> {
        let mut out = [0u8; 56];
        match self.dio(FSCTL_GET_USN_JOURNAL, None, Some(&mut out)) {
            Ok(n) if n >= 56 => Ok(JournalInfo {
                journal_id: rd_u64(&out, 0),
                first_usn: rd_u64(&out, 8) as i64,
                next_usn: rd_u64(&out, 16) as i64,
                lowest_valid_usn: rd_u64(&out, 24) as i64,
                max_usn: rd_u64(&out, 32) as i64,
                maximum_size: rd_u64(&out, 40),
                allocation_delta: rd_u64(&out, 48),
            }),
            Ok(_) => Err(FsError::Other("short FSCTL_GET_USN_JOURNAL reply".into())),
            Err(e) => Err(win_err(e)),
        }
    }

    /// Enable the USN journal if absent (64 MB, 16 MB delta — reduces wrap
    /// frequency), then return its info.
    pub fn ensure_journal(&self) -> Result<JournalInfo, FsError> {
        if let Ok(j) = self.journal_info() {
            return Ok(j);
        }
        let mut inb = [0u8; 16];
        inb[0..8].copy_from_slice(&0x0400_0000u64.to_le_bytes()); // MaximumSize 64MB
        inb[8..16].copy_from_slice(&0x0100_0000u64.to_le_bytes()); // AllocationDelta 16MB
        self.dio(FSCTL_CREATE_USN_JOURNAL, Some(&inb), None)
            .map_err(win_err)?;
        self.journal_info()
    }

    /// Enumerate every file record on the volume (the whole MFT). Blocking;
    /// typically a few seconds per million files on an SSD. Metafiles
    /// (FRN < 24) and the root record are filtered out.
    pub fn enumerate_mft(&self, mut sink: impl FnMut(MftEntry)) -> Result<(), FsError> {
        const BUF: usize = 1 << 24; // 16MB output buffer
        let mut buf = vec![0u8; BUF];
        // MFT_ENUM_DATA_V0 { StartFileReferenceNumber, LowUsn, HighUsn }
        let mut med = [0u8; 24];
        med[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        let mut start_frn: u64 = 0;
        loop {
            med[0..8].copy_from_slice(&start_frn.to_le_bytes());
            let returned = match self.dio(FSCTL_ENUM_USN_DATA, Some(&med), Some(&mut buf)) {
                Ok(n) => n,
                Err(ERROR_HANDLE_EOF) => return Ok(()),
                Err(e) => return Err(win_err(e)),
            };
            if returned < 8 {
                return Ok(());
            }
            let mut off = 0usize;
            while off + 8 <= returned {
                let reclen = rd_u32(&buf, off) as usize;
                if reclen == 0 || off + reclen > returned {
                    break;
                }
                if let Some(rec) = RawRecord::parse(&buf[off..off + reclen]) {
                    if rec.frn >= records::MFT_METAFILE_MAX_FRN {
                        start_frn = start_frn.max(rec.frn);
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
        }
    }

    /// Read pending journal records starting at `pos`. Returns the applied
    /// events and the updated position. Never returns partial buffers: the
    /// whole output buffer is consumed before advancing `next_usn`.
    pub fn read_journal(
        &self,
        pos: JournalPos,
        max_events: usize,
    ) -> Result<(Vec<IndexEvent>, JournalPos), FsError> {
        const BUF: usize = 1 << 20; // 1MB per read
        let mut out = vec![0u8; BUF];
        let mut events = Vec::new();
        let mut cur = pos;
        loop {
            // READ_USN_JOURNAL_DATA_V0 { StartUsn, ReasonMask, BytesToWaitFor,
            // Timeout, UsnJournalID } — built manually (padded layout).
            let mut inb = [0u8; 40];
            inb[0..8].copy_from_slice(&(cur.next_usn as i64).to_le_bytes());
            inb[8..12].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // all reasons
            inb[16..24].copy_from_slice(&0u64.to_le_bytes()); // BytesToWaitFor: poll
            inb[24..32].copy_from_slice(&0u64.to_le_bytes()); // Timeout
            inb[32..40].copy_from_slice(&cur.journal_id.to_le_bytes());

            let returned = match self.dio(FSCTL_READ_USN_JOURNAL, Some(&inb), Some(&mut out)) {
                Ok(n) => n,
                Err(e) => return Err(win_err(e)),
            };
            if returned < 8 {
                return Ok((events, cur));
            }
            cur.next_usn = rd_u64(&out, 0);
            events.extend(events_from_records(&out[8..returned]));
            if returned > 8 && events.len() < max_events {
                continue; // more records may be pending
            }
            return Ok((events, cur));
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

    fn info(journal_id: u64, lowest_valid_usn: i64, next_usn: i64) -> JournalInfo {
        JournalInfo {
            journal_id,
            first_usn: 0,
            next_usn,
            lowest_valid_usn,
            max_usn: 0,
            maximum_size: 0,
            allocation_delta: 0,
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
}
