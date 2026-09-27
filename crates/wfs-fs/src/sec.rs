//! Windows security descriptors for the two front-ends.
//!
//! A named pipe created without an explicit descriptor inherits the *creating
//! token's* default DACL. In console mode that token belongs to the interactive
//! user, so the pipe is reachable; running as a LocalSystem service it is not,
//! and the same configuration silently stops working. Every object that gates
//! access therefore carries an explicit DACL — see [`SDDL_OPEN`].

use std::ffi::c_void;
use std::path::Path;
use std::ptr::{null, null_mut};

use crate::FsError;

const SDDL_REVISION_1: u32 = 1;

/// SYSTEM + Administrators + Interactive Users, full access.
///
/// `IU` (S-1-5-4) is a logon-scoped SID: it is present in the token of whoever
/// is signed in at the console, which is exactly the population a local engine
/// must serve. `BU`/`AU` would also admit non-interactive logons, and the
/// filename index of a whole machine should not be readable by them.
pub const SDDL_OPEN: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;IU)";

/// SYSTEM + Administrators, full access.
pub const SDDL_ADMINS_ONLY: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)";

const READ_CONTROL: u32 = 0x0002_0000;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const CREATE_ALWAYS: u32 = 2;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
/// `SE_FILE_OBJECT` — `GetSecurityInfo` object type for files.
pub const SE_FILE_OBJECT: u32 = 1;
/// `SE_KERNEL_OBJECT` — object type for named pipes, events, mutexes, semaphores.
pub const SE_KERNEL_OBJECT: u32 = 6;
const OWNER_GROUP_DACL: u32 = 0x1 | 0x2 | 0x4;
/// `BCryptGenRandom` with a NULL algorithm handle uses the system preferred RNG.
const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x2;

/// Mirrors `SECURITY_ATTRIBUTES` (DWORD, pointer, BOOL — 24 bytes on x64).
#[repr(C)]
pub struct SecurityAttributes {
    pub n_length: u32,
    pub lp_security_descriptor: *mut c_void,
    pub b_inherit_handle: i32,
}

#[link(name = "advapi32")]
extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        stringsecuritydescriptor: *const u16,
        stringsdrevision: u32,
        securitydescriptor: *mut *mut c_void,
        securitydescriptorsize: *mut u32,
    ) -> i32;
    fn GetSecurityInfo(
        handle: isize,
        objecttype: u32,
        securityinformation: u32,
        sidowner: *mut *mut c_void,
        sidgroup: *mut *mut c_void,
        dacl: *mut *mut c_void,
        sacl: *mut *mut c_void,
        securitydescriptor: *mut *mut c_void,
    ) -> u32;
    fn ConvertSecurityDescriptorToStringSecurityDescriptorW(
        securitydescriptor: *const c_void,
        stringsdrevision: u32,
        securityinformation: u32,
        stringsecuritydescriptor: *mut *mut u16,
        stringsecuritydescriptorsize: *mut u32,
    ) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn LocalFree(hmem: *mut c_void) -> *mut c_void;
    fn WriteFile(
        hfile: isize,
        buffer: *const u8,
        nbytestowrite: u32,
        written: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
}

// `bcrypt.dll` is a system DLL, so linking it does not cost the single-exe
// property the release contract in docs/deploy.md depends on.
#[link(name = "bcrypt")]
extern "system" {
    fn BCryptGenRandom(algorithm: *mut c_void, buffer: *mut u8, length: u32, flags: u32) -> i32;
}

/// An SDDL-derived security descriptor plus the `SECURITY_ATTRIBUTES` wrapper
/// that carries it. Must outlive the create call it is used for.
pub struct SecurityDescriptor {
    sd: *mut c_void,
    attrs: SecurityAttributes,
}

impl SecurityDescriptor {
    pub fn from_sddl(sddl: &str) -> Result<SecurityDescriptor, FsError> {
        let sddl_w = crate::wide(sddl);
        let mut sd: *mut c_void = null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl_w.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                null_mut(),
            )
        };
        if ok == 0 || sd.is_null() {
            return Err(FsError::Other(format!(
                "bad security descriptor (win32 error {})",
                unsafe { crate::GetLastError() }
            )));
        }
        Ok(SecurityDescriptor {
            sd,
            attrs: SecurityAttributes {
                n_length: std::mem::size_of::<SecurityAttributes>() as u32,
                lp_security_descriptor: sd,
                b_inherit_handle: 0,
            },
        })
    }

    /// Raw `SECURITY_ATTRIBUTES*` for
    /// `ServerOptions::create_with_security_attributes_raw`. Valid only while
    /// `self` is alive.
    pub fn as_raw(&mut self) -> *mut c_void {
        &mut self.attrs as *mut SecurityAttributes as *mut c_void
    }

    /// Create `path` carrying this DACL, written in one go.
    ///
    /// The descriptor is attached at create time on purpose: writing the file
    /// first and tightening it afterwards leaves a window in which a secret
    /// sits under the directory's inherited ACL.
    pub fn write_file(&mut self, path: &Path, bytes: &[u8]) -> Result<(), FsError> {
        let name = crate::wide(&path.display().to_string());
        let attrs = &mut self.attrs as *mut SecurityAttributes;
        let handle = unsafe {
            crate::CreateFileW(
                name.as_ptr(),
                GENERIC_WRITE,
                0,
                attrs as *const u8,
                CREATE_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                0,
            )
        };
        if handle == -1 {
            return Err(FsError::Other(format!(
                "create {}: win32 error {}",
                path.display(),
                unsafe { crate::GetLastError() }
            )));
        }
        let mut written: u32 = 0;
        let ok = unsafe {
            WriteFile(
                handle,
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut written,
                null_mut(),
            )
        };
        unsafe { crate::CloseHandle(handle) };
        if ok == 0 || written as usize != bytes.len() {
            return Err(FsError::Other(format!(
                "write {}: win32 error {}",
                path.display(),
                unsafe { crate::GetLastError() }
            )));
        }
        Ok(())
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.sd.is_null() {
            unsafe { LocalFree(self.sd) };
        }
    }
}

/// A hex token for HTTP bearer auth (32 chars = 128 bits of system entropy).
pub fn random_token() -> Result<String, FsError> {
    let mut buf = [0u8; 16];
    let status = unsafe {
        BCryptGenRandom(
            null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        return Err(FsError::Other(format!(
            "BCryptGenRandom failed (ntstatus {status:#x})"
        )));
    }
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// The SDDL form of an already-open handle's security descriptor.
///
/// `objecttype` is `SE_KERNEL_OBJECT` (6) for named pipes and
/// `SE_FILE_OBJECT` (1) for files; passing the wrong one fails with
/// `ERROR_INVALID_HANDLE` / `ERROR_BAD_LENGTH` rather than degrading.
pub fn sddl_of_handle(handle: isize, objecttype: u32) -> Result<String, FsError> {
    let mut sd: *mut c_void = null_mut();
    let rc = unsafe {
        GetSecurityInfo(
            handle,
            objecttype,
            OWNER_GROUP_DACL,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            &mut sd,
        )
    };
    if rc != 0 {
        return Err(FsError::Other(format!("GetSecurityInfo: win32 error {rc}")));
    }
    let mut text: *mut u16 = null_mut();
    let mut len: u32 = 0;
    let ok = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            SDDL_REVISION_1,
            OWNER_GROUP_DACL,
            &mut text,
            &mut len,
        )
    };
    unsafe { LocalFree(sd) };
    if ok == 0 || text.is_null() {
        return Err(FsError::Other(format!(
            "render security descriptor: win32 error {}",
            unsafe { crate::GetLastError() }
        )));
    }
    let sddl = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len as usize) });
    unsafe { LocalFree(text as *mut c_void) };
    // the reported length includes the terminator(s)
    Ok(sddl.trim_end_matches('\0').to_string())
}

/// The SDDL form of a file's security descriptor, opened by path.
pub fn object_sddl(path: &str) -> Result<String, FsError> {
    let handle = open_for_security_query(path, READ_CONTROL)?;
    let result = sddl_of_handle(handle, SE_FILE_OBJECT);
    unsafe { crate::CloseHandle(handle) };
    result
}

/// The SDDL form of a named pipe's DACL.
///
/// Two things make this unlike the file case. The query needs `READ_CONTROL`
/// on the *handle*, and a server instance handle from `CreateNamedPipeW` never
/// carries it — so the descriptor has to be read through a client handle. That
/// client open also consumes one waiting instance, so the caller must have one
/// queued (`ERROR_ALL_PIPE_INSTANCES_BUSY` otherwise), which is precisely what
/// makes this usable as a regression test: the DACL is checked by an open that
/// the DACL itself gates.
pub fn pipe_sddl(path: &str) -> Result<String, FsError> {
    let handle = open_for_security_query(path, READ_CONTROL | GENERIC_READ | GENERIC_WRITE)?;
    let result = sddl_of_handle(handle, SE_KERNEL_OBJECT);
    unsafe { crate::CloseHandle(handle) };
    result
}

fn open_for_security_query(path: &str, access: u32) -> Result<isize, FsError> {
    let name = crate::wide(path);
    let handle =
        unsafe { crate::CreateFileW(name.as_ptr(), access, 0, null(), crate::OPEN_EXISTING, 0, 0) };
    if handle == -1 {
        return Err(FsError::Other(format!(
            "open {path} for security query: win32 error {}",
            unsafe { crate::GetLastError() }
        )));
    }
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admins_only_sddl_converts() {
        let mut sec = SecurityDescriptor::from_sddl(SDDL_ADMINS_ONLY).expect("SDDL converts");
        assert!(!sec.as_raw().is_null());
        // the wrapper reports the Win32 SECURITY_ATTRIBUTES size (24 on x64)
        let attrs = unsafe { &*(sec.as_raw() as *const SecurityAttributes) };
        assert_eq!(
            attrs.n_length,
            std::mem::size_of::<SecurityAttributes>() as u32
        );
        assert_eq!(std::mem::size_of::<SecurityAttributes>(), 24);
        assert!(!attrs.lp_security_descriptor.is_null());
    }

    #[test]
    fn bad_sddl_is_rejected() {
        assert!(SecurityDescriptor::from_sddl("definitely not sddl").is_err());
    }

    #[test]
    fn tokens_are_random_hex_and_distinct() {
        let a = random_token().unwrap();
        let b = random_token().unwrap();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    /// Round-trips through the DACL the file actually carries.
    ///
    /// Uses [`SDDL_OPEN`] rather than [`SDDL_ADMINS_ONLY`] because the query
    /// itself needs `READ_CONTROL`: a test run from a UAC-filtered token has
    /// `BA` marked deny-only, so it could not read back a descriptor that only
    /// grants `SY` + `BA`. That rejection is the restriction working, and
    /// `pipe.rs` asserts it from the other side.
    #[test]
    fn write_file_carries_the_explicit_dacl() {
        let dir = std::env::temp_dir().join(format!("wfs-sec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        let mut sd = SecurityDescriptor::from_sddl(SDDL_OPEN).unwrap();
        sd.write_file(&path, b"deadbeef").unwrap();

        let sddl = object_sddl(&path.display().to_string()).unwrap();
        assert!(sddl.contains("SY"), "{sddl}");
        assert!(sddl.contains("BA"), "{sddl}");
        assert!(sddl.contains("IU"), "{sddl}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "deadbeef");
        std::fs::remove_dir_all(&dir).ok();
    }
}
