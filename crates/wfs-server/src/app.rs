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

    let configured = resolve_drives(&state.config);
    let mut to_build = Vec::new();
    for d in &configured {
        if !resumed.contains_key(d) {
            to_build.push(*d);
        }
    }
    // A restored volume needs a worker as much as a fresh one: the resume
    // position only skips the full build — without the watch loop the index
    // would silently freeze at the snapshot.
    let mut restored: Vec<char> = resumed.keys().copied().collect();
    restored.sort_unstable();
    let workers = worker_drives(&configured, &restored);

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
    watcher::start(&state, workers, &resumed);
    spawn_warm_snapshot(&state, initial_build);
    Ok(App { state, rt })
}

/// Every drive that gets a worker thread: everything configured, plus any
/// snapshot-restored drive the config no longer lists. Restored drives skip
/// the full build in their worker and resume the journal watch directly.
fn worker_drives(configured: &[char], restored: &[char]) -> Vec<char> {
    let mut drives = configured.to_vec();
    for d in restored {
        if !drives.contains(d) {
            drives.push(*d);
        }
    }
    drives
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

pub(crate) fn resolve_drives(config: &Config) -> Vec<char> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_start_builds_every_configured_drive() {
        assert_eq!(worker_drives(&['C', 'D'], &[]), vec!['C', 'D']);
    }

    #[test]
    fn restored_drive_keeps_a_worker() {
        // regression: boot used to pass only the to-build list to
        // watcher::start, so snapshot-restored volumes got no watch thread
        // and their indexes silently went stale
        assert_eq!(worker_drives(&['C', 'D'], &['D']), vec!['C', 'D']);
    }

    #[test]
    fn restored_drive_dropped_from_config_is_still_watched() {
        assert_eq!(worker_drives(&['C'], &['C', 'E']), vec!['C', 'E']);
    }
}
