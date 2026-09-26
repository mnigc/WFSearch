//! Fixtures shared by the server's unit tests. Compiled only under `cfg(test)`
//! because `wfs-server` is a binary crate — there is no lib target for an
//! integration test to import.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use wfs_core::{JournalPos, VolumeIndex, VolumePhase, ROOT_FRN};

use crate::config::Config;
use crate::state::AppState;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique(tag: &str) -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{tag}-{n}", std::process::id())
}

/// Per-test temp directory (no `tempfile` dependency).
pub fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wfs-test-{}", unique(tag)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// `C:\work\notes.txt` and `C:\notes.md` — enough to tell a name match from a
/// path match, and to exercise the directory's own path.
pub fn fixture_index() -> VolumeIndex {
    let mut idx = VolumeIndex::new('C');
    idx.insert(10, ROOT_FRN, "work", true);
    idx.insert(11, 10, "notes.txt", false);
    idx.insert(12, ROOT_FRN, "notes.md", false);
    idx
}

pub fn config_in(dir: PathBuf) -> Config {
    Config {
        data_dir: Some(dir.display().to_string()),
        http_port: 0, // let the OS pick
        pipe_name: unique_pipe(),
        ..Config::default()
    }
}

/// A pipe name no other test (or a running service) can be holding.
pub fn unique_pipe() -> String {
    format!(r"\\.\pipe\wfs-test-{}", unique("pipe"))
}

/// A ready engine with the fixture tree indexed, listening nowhere.
pub fn state() -> Arc<AppState> {
    let st = AppState::new(config_in(temp_dir("state")));
    st.engine.init_volume(
        'C',
        fixture_index(),
        Some(JournalPos {
            journal_id: 1,
            next_usn: 100,
        }),
        VolumePhase::Ready,
    );
    st
}
