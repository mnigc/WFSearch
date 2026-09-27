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

/// SYSTEM + Administrators full, Interactive Users read-only. The default for
/// engine-owned *files*: the population that may query the engine may also read
/// the HTTP token and the index, but only the engine may rewrite them.
pub const SDDL_OPEN_RO: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GR;;;IU)";

/// The data directory, under `acl = "open"` and `acl = "restricted"`.
///
/// `P` drops the inherited ACEs — `%ProgramData%` ships
/// `Users:(OI)(CI)(WD,AD,WEA,WA)`, which lets any local user create files in a
/// subdirectory and, if they created it, own it. `(OI)(CI)` passes these ACEs
/// on to the files created inside.
pub const SDDL_DIR_OPEN: &str = "D:P(A;OICI;GA;;;SY)(A;OICI;GA;;;BA)(A;OICI;GR;;;IU)";
pub const SDDL_DIR_ADMINS_ONLY: &str = "D:P(A;OICI;GA;;;SY)(A;OICI;GA;;;BA)";

const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
const PROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x8000_0000;
/// `CreateFileW` refuses to open a directory without it.
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
/// `WELL_KNOWN_SID_TYPE` values used by [`current_process_is_privileged`].
const WIN_LOCAL_SYSTEM_SID: u32 = 22;
const WIN_BUILTIN_ADMINISTRATORS_SID: u32 = 26;
/// `SECURITY_MAX_SID_SIZE`
const MAX_SID_BYTES: usize = 68;

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
    fn SetNamedSecurityInfoW(
        objectname: *mut u16,
        objecttype: u32,
        securityinformation: u32,
        sidowner: *mut c_void,
        sidgroup: *mut c_void,
        dacl: *mut c_void,
        sacl: *mut c_void,
    ) -> u32;
    fn GetSecurityDescriptorDacl(
        securitydescriptor: *const c_void,
        daclpresent: *mut i32,
        dacl: *mut *mut c_void,
        dacldefaulted: *mut i32,
    ) -> i32;
    fn CheckTokenMembership(
        tokenhandle: *mut c_void,
        sidtocheck: *mut c_void,
        ismember: *mut i32,
    ) -> i32;
    fn CreateWellKnownSid(
        wellknownsidtype: u32,
        domainsid: *mut c_void,
        sid: *mut c_void,
        cbsid: *mut u32,
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

    /// The DACL inside this descriptor, for [`SetNamedSecurityInfoW`]. Valid
    /// only while `self` is alive.
    ///
    /// Every out-parameter gets a real local: despite the SDK documenting
    /// `lpbDaclDefaulted` as optional, passing NULL faults on at least
    /// Windows 11 24H2 (observed `STATUS_ACCESS_VIOLATION`).
    fn dacl(&mut self) -> Result<*mut c_void, FsError> {
        let mut present: i32 = 0;
        let mut dacl: *mut c_void = null_mut();
        let mut defaulted: i32 = 0;
        let ok =
            unsafe { GetSecurityDescriptorDacl(self.sd, &mut present, &mut dacl, &mut defaulted) };
        if ok == 0 || present == 0 || dacl.is_null() {
            return Err(FsError::Other(format!(
                "security descriptor carries no DACL (win32 error {})",
                unsafe { crate::GetLastError() }
            )));
        }
        Ok(dacl)
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

/// Replace `path`'s DACL with `sddl`, and stop it inheriting anything else.
///
/// This is the existing-object counterpart of [`SecurityDescriptor::write_file`]
/// (which attaches a descriptor at create time). The data directory usually
/// predates the first hardened boot, and so do the files inside it.
pub fn set_dacl(path: &Path, sddl: &str) -> Result<(), FsError> {
    let mut sd = SecurityDescriptor::from_sddl(sddl)?;
    let dacl = sd.dacl()?;
    let mut name = crate::wide(&path.display().to_string());
    let rc = unsafe {
        SetNamedSecurityInfoW(
            name.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            dacl,
            null_mut(),
        )
    };
    if rc != 0 {
        return Err(FsError::Other(format!(
            "set the ACL of {}: win32 error {rc}",
            path.display()
        )));
    }
    Ok(())
}

/// Whether this process's token really holds Administrators (or is SYSTEM).
///
/// A UAC-filtered token carries Administrators deny-only, and
/// `CheckTokenMembership` reports false for it — which is exactly the
/// distinction that matters: rewriting the data directory's DACL down to
/// SYSTEM + Administrators would lock an unprivileged caller out of the
/// directory it is about to write into.
pub fn current_process_is_privileged() -> bool {
    fn holds(sid_type: u32) -> bool {
        let mut sid = [0u8; MAX_SID_BYTES];
        let mut len = MAX_SID_BYTES as u32;
        let ok = unsafe {
            CreateWellKnownSid(
                sid_type,
                null_mut(),
                sid.as_mut_ptr() as *mut c_void,
                &mut len,
            )
        };
        if ok == 0 {
            return false;
        }
        let mut member: i32 = 0;
        let ok = unsafe {
            CheckTokenMembership(null_mut(), sid.as_mut_ptr() as *mut c_void, &mut member)
        };
        ok != 0 && member != 0
    }
    holds(WIN_BUILTIN_ADMINISTRATORS_SID) || holds(WIN_LOCAL_SYSTEM_SID)
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
    let handle = open_for_security_query(path, READ_CONTROL, 0)?;
    let result = sddl_of_handle(handle, SE_FILE_OBJECT);
    unsafe { crate::CloseHandle(handle) };
    result
}

/// The SDDL form of a directory's security descriptor (owner first, so
/// `O:SY`/`O:BA` says the owner is trusted and anything else is a warning
/// worth printing).
pub fn dir_sddl(path: &str) -> Result<String, FsError> {
    let handle = open_for_security_query(path, READ_CONTROL, FILE_FLAG_BACKUP_SEMANTICS)?;
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
    let handle = open_for_security_query(path, READ_CONTROL | GENERIC_READ | GENERIC_WRITE, 0)?;
    let result = sddl_of_handle(handle, SE_KERNEL_OBJECT);
    unsafe { crate::CloseHandle(handle) };
    result
}

fn open_for_security_query(path: &str, access: u32, flags: u32) -> Result<isize, FsError> {
    let name = crate::wide(path);
    let handle = unsafe {
        crate::CreateFileW(
            name.as_ptr(),
            access,
            0,
            null(),
            crate::OPEN_EXISTING,
            flags,
            0,
        )
    };
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

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wfs-sec-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `set_dacl` is the existing-object path: it has to replace whatever was
    /// inherited *and* mark the result protected, or the `Users:(WD,AD)` ACE
    /// that `%ProgramData%` hands down would survive underneath it.
    #[test]
    fn set_dacl_replaces_and_protects_a_directory() {
        let dir = temp_dir("dir");
        set_dacl(&dir, SDDL_DIR_OPEN).unwrap();

        let sddl = dir_sddl(&dir.display().to_string()).unwrap();
        assert!(sddl.contains("SY"), "{sddl}");
        assert!(sddl.contains("BA"), "{sddl}");
        assert!(sddl.contains("IU"), "{sddl}");
        let flags = sddl
            .split("D:")
            .nth(1)
            .and_then(|rest| rest.split('(').next())
            .unwrap_or_default();
        assert!(flags.contains('P'), "the DACL must be protected: {sddl}");
        assert!(!flags.contains("WD"), "{sddl}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Files get read-only for interactive users — writing one back is the
    /// engine's job, and a user-writable `index.bin` is the whole bug.
    ///
    /// The DACL is read back through the object, so the ACEs come out in their
    /// *file-specific* spelling: `SetNamedSecurityInfoW` maps the generic
    /// `GA`/`GR` down to `FA`/`FR` when it applies them.
    #[test]
    fn set_dacl_leaves_an_existing_file_read_only_for_users() {
        let dir = temp_dir("file");
        let path = dir.join("index.bin");
        std::fs::write(&path, b"x").unwrap();
        set_dacl(&path, SDDL_OPEN_RO).unwrap();

        let sddl = object_sddl(&path.display().to_string()).unwrap();
        assert!(sddl.contains("A;;FR;;;IU"), "{sddl}");
        assert!(!sddl.contains("A;;FA;;;IU"), "{sddl}");
        assert!(!sddl.contains("A;;GA;;;IU"), "{sddl}");
        assert!(sddl.contains("A;;FA;;;SY"), "{sddl}");
        assert!(sddl.contains("A;;FA;;;BA"), "{sddl}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn every_sddl_constant_converts() {
        for sddl in [
            SDDL_OPEN,
            SDDL_ADMINS_ONLY,
            SDDL_OPEN_RO,
            SDDL_DIR_OPEN,
            SDDL_DIR_ADMINS_ONLY,
        ] {
            assert!(SecurityDescriptor::from_sddl(sddl).is_ok(), "{sddl}");
        }
    }

    /// The probe's whole job is to predict whether a directory hardened down to
    /// SYSTEM + Administrators would still admit this process. Ownership is not
    /// enough to write into a directory, so a create is the honest check.
    #[test]
    fn privileged_probe_matches_reality() {
        let dir = temp_dir("priv");
        set_dacl(&dir, SDDL_DIR_ADMINS_ONLY).unwrap();

        let can_write = std::fs::write(dir.join("probe"), b"x").is_ok();
        assert_eq!(
            can_write,
            current_process_is_privileged(),
            "SY+BA alone must admit exactly the tokens the probe calls privileged"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
