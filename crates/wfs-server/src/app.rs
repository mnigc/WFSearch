//! Shared boot path for console and service modes.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wfs_core::JournalPos;

use crate::config::Config;
use crate::state::AppState;
use crate::{http, pipe, snapshot, watcher};

pub struct App {
    pub state: Arc<AppState>,
    pub rt: tokio::runtime::Runtime,
}

pub fn boot(config: Config) -> anyhow::Result<App> {
    let state = AppState::new(config);

    // warm start: restore whatever the snapshot can cover
    let resumed: HashMap<char, JournalPos> = snapshot::try_load(&state);

    let mut to_build = Vec::new();
    for d in resolve_drives(&state.config) {
        if !resumed.contains_key(&d) {
            to_build.push(d);
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    {
        let st = state.clone();
        rt.block_on(async move {
            http::spawn(st.clone()).await?;
            pipe::spawn(st).await
        })?;
    }

    let initial_build = to_build.clone();
    watcher::start(&state, to_build, &resumed);
    spawn_warm_snapshot(&state, initial_build);
    Ok(App { state, rt })
}

/// Write the snapshot once the first full build has settled, so the next start
/// is warm even on a machine that never shuts the service down gracefully.
fn spawn_warm_snapshot(state: &Arc<AppState>, initial: Vec<char>) {
    if initial.is_empty() {
        return;
    }
    let st = state.clone();
    let _ = std::thread::Builder::new()
        .name("wfs-snapshot".into())
        .spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30 * 60);
            loop {
                if st.stop.load(Ordering::Relaxed) || Instant::now() > deadline {
                    return;
                }
                let status = st.engine.status();
                let settled = initial.iter().all(|d| {
                    status
                        .volumes
                        .iter()
                        .any(|v| v.drive == *d && v.phase != wfs_core::VolumePhase::Building)
                });
                if settled {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            match snapshot::save(&st) {
                Ok(p) => tracing::info!("warm snapshot written to {}", p.display()),
                Err(e) => tracing::warn!("warm snapshot failed: {e}"),
            }
        });
}

impl App {
    /// Signal watchers to stop; snapshot should be saved before calling.
    pub fn shutdown(self) {
        self.state.stop.store(true, Ordering::SeqCst);
        // let the watcher threads observe the flag before process exit
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn resolve_drives(config: &Config) -> Vec<char> {
    if config.drives.iter().any(|d| d.eq_ignore_ascii_case("auto")) {
        wfs_fs::detect_fixed_drives()
    } else {
        config
            .drives
            .iter()
            .filter_map(|d| {
                let t = d.trim().trim_end_matches(':');
                t.chars().next().map(|c| c.to_ascii_uppercase())
            })
            .filter(|c| c.is_ascii_alphabetic())
            .collect()
    }
}
