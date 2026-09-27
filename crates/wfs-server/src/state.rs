use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use wfs_core::Engine;
use wfs_fs::sec;

use crate::config::Config;

pub struct AppState {
    pub engine: Arc<Engine>,
    pub config: Config,
    pub started: Instant,
    pub stop: Arc<AtomicBool>,
    /// entries indexed so far per drive that is still building
    pub build_progress: Mutex<BTreeMap<char, u64>>,
    /// Bearer token for the HTTP gateway. It is also written to
    /// `<data dir>/http.token` under the DACL that `acl` selects, so *reading
    /// that file* is the credential: loopback carries no per-user identity of
    /// its own, and the filesystem does.
    pub http_token: String,
}

impl AppState {
    pub fn try_new(config: Config) -> anyhow::Result<Arc<AppState>> {
        harden_data_dir(&config)?;
        let http_token = issue_http_token(&config)?;
        Ok(Arc::new(AppState {
            engine: Arc::new(Engine::new()),
            started: Instant::now(),
            stop: Arc::new(AtomicBool::new(false)),
            build_progress: Mutex::new(BTreeMap::new()),
            http_token,
            config,
        }))
    }
}

/// Take the data directory, and the engine's own files in it, out of the
/// population that can reach the engine.
///
/// `%ProgramData%` hands every local user `Users:(OI)(CI)(WD,AD,WEA,WA)` on the
/// subdirectories underneath it, and `CREATOR OWNER` hands a directory to
/// whoever created it. Left alone, that makes `config.toml` and `index.bin`
/// attacker-controlled input for a SYSTEM process — the config decides volumes,
/// port and `acl`; the snapshot is deserialized and then queried.
///
/// Skipped when this token cannot keep its own access: a plain-user `console`
/// run would lock itself out of the directory it is about to write into.
fn harden_data_dir(config: &Config) -> anyhow::Result<()> {
    let dir = config.data_dir_path();
    if !sec::current_process_is_privileged() {
        tracing::warn!(
            "{} keeps its inherited ACL: this process is not elevated, so tightening it \
             would lock the engine out (fine in console mode, not as a service)",
            dir.display()
        );
        return Ok(());
    }
    std::fs::create_dir_all(&dir).map_err(|e| anyhow::anyhow!("create {}: {e}", dir.display()))?;
    let restricted = config.acl_restricted();
    let dir_sddl = if restricted {
        sec::SDDL_DIR_ADMINS_ONLY
    } else {
        sec::SDDL_DIR_OPEN
    };
    sec::set_dacl(&dir, dir_sddl).map_err(|e| anyhow::anyhow!("{e}"))?;

    // The new DACL does not heal a descriptor the directory handed down before
    // it, and an untrusted owner can always rewrite it: report both rather than
    // let `acl = "restricted"` look stronger than it is.
    match sec::dir_sddl(&dir.display().to_string()) {
        Ok(sddl) if !owner_is_trusted(owner_sid(&sddl)) => tracing::warn!(
            "data directory {} is owned by {}, which can rewrite its own ACL; \
             icacls \"{}\" /setowner \"NT AUTHORITY\\SYSTEM\" hands it to SYSTEM",
            dir.display(),
            owner_sid(&sddl),
            dir.display()
        ),
        Err(e) => tracing::warn!("cannot read the data directory's descriptor: {e}"),
        _ => {}
    }

    let file_sddl = if restricted {
        sec::SDDL_ADMINS_ONLY
    } else {
        sec::SDDL_OPEN_RO
    };
    let entries =
        std::fs::read_dir(&dir).map_err(|e| anyhow::anyhow!("read {}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|e| anyhow::anyhow!("read {}: {e}", dir.display()))?
            .path();
        if !path.is_file() {
            continue;
        }
        // A file we cannot re-ACL (locked, odd volume) must not take the engine
        // down — the directory is the boundary that matters.
        if let Err(e) = sec::set_dacl(&path, file_sddl) {
            tracing::warn!("{e}");
        }
    }
    Ok(())
}

/// The owner component of an SDDL descriptor: `SY` for SYSTEM, `BA` for
/// Administrators, a raw `S-1-5-21-…` for anyone else.
fn owner_sid(sddl: &str) -> &str {
    let rest = sddl.strip_prefix("O:").unwrap_or(sddl);
    let end = rest
        .find("G:")
        .or_else(|| rest.find("D:"))
        .unwrap_or(rest.len());
    &rest[..end]
}

fn owner_is_trusted(sid: &str) -> bool {
    matches!(sid, "SY" | "BA")
}

/// Mint the HTTP token and publish it to the data directory.
///
/// Failing here takes the engine down rather than degrading: serving the
/// gateway with an undiscoverable token would look healthy while locking out
/// every client, which is the failure mode this whole path exists to avoid.
fn issue_http_token(config: &Config) -> anyhow::Result<String> {
    let token = wfs_fs::sec::random_token()?;
    let path = config.token_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    // Read-only for interactive users: they must be able to *read* the token to
    // use the gateway, but nothing but the engine has any business rewriting it.
    let sddl = if config.acl_restricted() {
        wfs_fs::sec::SDDL_ADMINS_ONLY
    } else {
        wfs_fs::sec::SDDL_OPEN_RO
    };
    let mut sd = wfs_fs::sec::SecurityDescriptor::from_sddl(sddl)?;
    sd.write_file(&path, token.as_bytes()).map_err(|e| {
        anyhow::anyhow!(
            "write {}: {e} (set data_dir to a writable path)",
            path.display()
        )
    })?;
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    #[test]
    fn owner_sid_picks_the_owner_out_of_the_descriptor() {
        assert_eq!(owner_sid("O:SYG:SYD:(A;;GA;;;SY)"), "SY");
        assert_eq!(owner_sid("O:BAG:BAD:(A;;GA;;;BA)"), "BA");
        assert_eq!(
            owner_sid("O:S-1-5-21-1-2-3-1001G:S-1-5-21-1-2-3-513D:(A;;GA;;;SY)"),
            "S-1-5-21-1-2-3-1001"
        );
        assert!(!owner_is_trusted(owner_sid(
            "O:S-1-5-21-1-2-3-1001G:SYD:(A;;GA;;;SY)"
        )));
    }

    /// Whichever way the privilege probe answers, boot must leave the data
    /// directory usable by the process that just rewrote its ACL — locking
    /// ourselves out here would take the engine down on its own hardening.
    #[test]
    fn boot_never_locks_the_process_out_of_the_data_dir() {
        let dir = testutil::temp_dir("acl");
        let st = AppState::try_new(testutil::config_in(dir.clone())).expect("state");

        std::fs::write(dir.join("probe"), b"ok").expect("the data dir is still writable");
        let token = std::fs::read_to_string(st.config.token_path()).expect("token readable");
        assert_eq!(token.len(), 32);
    }
}
