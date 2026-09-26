//! Index snapshots: serialize all volume indexes + journal positions so the
//! service starts warm instead of re-walking every MFT.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use wfs_core::{JournalPos, VolumeIndex, VolumePhase};
use wfs_fs::{resume_position, VolumeHandle};

use crate::state::AppState;

const MAGIC: [u8; 4] = *b"WFS1";
// v2: file references are normalized to 48-bit record numbers at parse time;
// v1 snapshots hold raw references with sequence bits, which journal replay
// no longer matches — reject them so affected volumes rebuild once.
const FORMAT_VERSION: u32 = 2;

#[derive(serde::Serialize, serde::Deserialize)]
struct VolumeSnap {
    drive: char,
    journal: Option<JournalPos>,
    index: VolumeIndex,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SnapshotFile {
    magic: [u8; 4],
    version: u32,
    saved_at_secs: u64,
    volumes: Vec<VolumeSnap>,
}

pub fn file_path(state: &AppState) -> PathBuf {
    state.config.data_dir_path().join("index.bin")
}

pub fn save(state: &AppState) -> anyhow::Result<PathBuf> {
    let volumes = state
        .engine
        .snapshot_data()
        .into_iter()
        .map(|(drive, index, journal)| VolumeSnap {
            drive,
            index,
            journal,
        })
        .collect();
    let snap = SnapshotFile {
        magic: MAGIC,
        version: FORMAT_VERSION,
        saved_at_secs: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        volumes,
    };
    let bytes = bincode::serialize(&snap)?;
    let dir = state.config.data_dir_path();
    fs::create_dir_all(&dir)?;
    let final_path = dir.join("index.bin");
    let tmp = dir.join("index.bin.tmp");
    fs::write(&tmp, &bytes)?;
    fs::rename(&tmp, &final_path)?;
    tracing::info!("snapshot written to {}", final_path.display());
    Ok(final_path)
}

/// Read and validate the snapshot file. `None` covers every reason to ignore
/// it (missing, truncated, foreign format) — all of which mean a full rebuild.
fn read_snapshot(state: &AppState) -> Option<SnapshotFile> {
    let bytes = fs::read(file_path(state)).ok()?;
    let snap: SnapshotFile = match bincode::deserialize(&bytes) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("snapshot unreadable ({e}) - full rebuild");
            return None;
        }
    };
    if snap.magic != MAGIC || snap.version != FORMAT_VERSION {
        tracing::warn!("snapshot version mismatch - full rebuild");
        return None;
    }
    Some(snap)
}

/// Try to restore all volumes from the snapshot. A volume is only restored if
/// its USN journal is still the same one (same id, position inside the valid
/// range) — otherwise it is rebuilt from scratch. Returns the journal
/// positions of restored volumes for the watcher threads.
pub fn try_load(state: &AppState) -> HashMap<char, JournalPos> {
    let mut resumed = HashMap::new();
    let snap = match read_snapshot(state) {
        Some(s) => s,
        None => return resumed,
    };
    tracing::info!(
        "snapshot from {}s ago, {} volume(s)",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_sub(snap.saved_at_secs),
        snap.volumes.len()
    );
    for v in snap.volumes {
        let ok = VolumeHandle::open(v.drive)
            .ok()
            .and_then(|h| h.journal_info().ok())
            .and_then(|info| resume_position(v.journal?, &info));
        match ok {
            Some(jp) => {
                tracing::info!("volume {}: resumed from snapshot", v.drive);
                state
                    .engine
                    .init_volume(v.drive, v.index, Some(jp), VolumePhase::Ready);
                resumed.insert(v.drive, jp);
            }
            None => {
                tracing::info!("volume {}: snapshot stale, will rebuild", v.drive);
            }
        }
    }
    resumed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    fn snapshot_bytes(state: &AppState) -> Vec<u8> {
        fs::read(file_path(state)).expect("snapshot file written")
    }

    #[test]
    fn save_then_read_roundtrips_the_index_and_journal() {
        let st = testutil::state();
        let path = save(&st).unwrap();
        assert!(path.exists());

        let snap = read_snapshot(&st).expect("freshly saved snapshot is readable");
        assert_eq!(snap.magic, MAGIC);
        assert_eq!(snap.version, FORMAT_VERSION);
        assert_eq!(snap.volumes.len(), 1);
        assert!(snap.saved_at_secs > 0);

        let v = snap.volumes.into_iter().next().unwrap();
        assert_eq!(v.drive, 'C');
        assert_eq!(
            v.journal,
            Some(JournalPos {
                journal_id: 1,
                next_usn: 100
            })
        );
        assert_eq!(v.index.live_count(), 4); // root + work/ + notes.txt + notes.md

        // the deserialized index must still answer queries against the same engine API
        let engine = wfs_core::Engine::new();
        engine.init_volume(v.drive, v.index, v.journal, VolumePhase::Ready);
        let out = engine.search(&wfs_core::Query::parse("notes*"), &Default::default());
        assert_eq!(out.total_matched, 2);
    }

    #[test]
    fn missing_snapshot_reads_as_none() {
        let st = testutil::state();
        assert!(read_snapshot(&st).is_none());
    }

    #[test]
    fn corrupt_image_is_rejected_not_fatal() {
        let st = testutil::state();
        fs::create_dir_all(st.config.data_dir_path()).unwrap();
        fs::write(file_path(&st), b"garbage-not-bincode").unwrap();
        assert!(read_snapshot(&st).is_none());
    }

    #[test]
    fn foreign_magic_or_version_is_rejected() {
        let st = testutil::state();
        save(&st).unwrap();

        let mut snap = read_snapshot(&st).unwrap();
        snap.magic = *b"XXXX";
        fs::write(file_path(&st), bincode::serialize(&snap).unwrap()).unwrap();
        assert!(read_snapshot(&st).is_none(), "foreign magic must not load");

        snap.magic = MAGIC;
        snap.version = FORMAT_VERSION + 1;
        fs::write(file_path(&st), bincode::serialize(&snap).unwrap()).unwrap();
        assert!(read_snapshot(&st).is_none(), "future version must not load");
    }

    #[test]
    fn save_is_atomic_and_leaves_no_tmp_file() {
        let st = testutil::state();
        let path = save(&st).unwrap();
        assert!(path.ends_with("index.bin"));
        assert!(
            !st.config.data_dir_path().join("index.bin.tmp").exists(),
            "tmp file must be renamed away"
        );
        assert!(snapshot_bytes(&st).starts_with(b"WFS1"));
    }
}
