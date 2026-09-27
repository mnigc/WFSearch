use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use wfs_core::Engine;

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
    let sddl = if config.acl_restricted() {
        wfs_fs::sec::SDDL_ADMINS_ONLY
    } else {
        wfs_fs::sec::SDDL_OPEN
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
