//! Optional restrictive DACL for the query pipe.
//!
//! The engine serves applications running as the interactive user, so the
//! default DACL (any local user may connect) is what `pipe_acl = "open"`
//! means. `pipe_acl = "restricted"` builds a DACL from SDDL that only SYSTEM
//! and Administrators can open, for machine-wide deployments where the
//! filename index must not be readable by every interactive user.

use std::ffi::c_void;
use std::ptr::null_mut;

use crate::FsError;

const SDDL_REVISION_1: u32 = 1;

/// SYSTEM + Administrators, full access.
pub const SDDL_ADMINS_ONLY: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)";

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
}

#[link(name = "kernel32")]
extern "system" {
    fn LocalFree(hmem: *mut c_void) -> *mut c_void;
}

/// An SDDL-derived security descriptor plus the `SECURITY_ATTRIBUTES` wrapper
/// that carries it. Must outlive the pipe creation call it is used for.
pub struct PipeSecurity {
    sd: *mut c_void,
    attrs: SecurityAttributes,
}

impl PipeSecurity {
    pub fn from_sddl(sddl: &str) -> Result<PipeSecurity, FsError> {
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
        Ok(PipeSecurity {
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
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        if !self.sd.is_null() {
            unsafe { LocalFree(self.sd) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admins_only_sddl_converts() {
        let mut sec = PipeSecurity::from_sddl(SDDL_ADMINS_ONLY).expect("SDDL converts");
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
        assert!(PipeSecurity::from_sddl("definitely not sddl").is_err());
    }
}
