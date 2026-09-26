//! Per-volume worker threads: full MFT build on first start, then a USN
//! journal watch loop applying incremental events. One thread per volume;
//! the thread owns the volume handle for its whole life.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wfs_core::{InsertOutcome, JournalPos, VolumeIndex, VolumePhase};
use wfs_fs::{resume_position, FsError, VolumeHandle};

use crate::state::AppState;

/// Start one worker per drive. `resume` carries snapshot-restored journal
/// positions — those volumes skip the full build.
pub fn start(state: &Arc<AppState>, drives: Vec<char>, resume: &HashMap<char, JournalPos>) {
    for d in drives {
        let st = state.clone();
        let r = resume.get(&d).copied();
        let name = format!("wfs-{}", d.to_ascii_uppercase());
        let _ = std::thread::Builder::new().name(name).spawn(move || {
            volume_thread(st, d, r);
        });
    }
}

enum VolErr {
    /// transient — sleep and retry the whole open/build
    Retry(u64),
    /// journal gone/wrapped or volume changed — rebuild from scratch
    Rebuild,
    /// cannot succeed by retrying (e.g. the token lacks volume access) — the
    /// reason is reported and the volume is marked failed
    Fatal(String),
}

fn volume_thread(state: Arc<AppState>, drive: char, resume: Option<JournalPos>) {
    let mut resume = resume;
    let mut retries: u32 = 0;
    loop {
        if state.stop.load(Ordering::Relaxed) {
            return;
        }
        match run_volume(&state, drive, resume.take()) {
            Ok(()) => return, // stop requested
            Err(VolErr::Retry(ms)) => {
                retries += 1;
                tracing::warn!("volume {drive}: retrying in {ms}ms (attempt {retries})");
                if retries >= 10 {
                    tracing::error!(
                        "volume {drive}: giving up after {retries} attempts; volume marked failed"
                    );
                    fail_volume(&state, drive);
                    return;
                }
                std::thread::sleep(Duration::from_millis(ms));
            }
            Err(VolErr::Rebuild) => {
                tracing::info!("volume {drive}: rebuilding index from MFT");
                retries = 0;
            }
            Err(VolErr::Fatal(why)) => {
                tracing::error!("volume {drive}: {why}; volume marked failed");
                fail_volume(&state, drive);
                return;
            }
        }
    }
}

fn run_volume(
    state: &Arc<AppState>,
    drive: char,
    resume: Option<JournalPos>,
) -> Result<(), VolErr> {
    let vol = VolumeHandle::open(drive).map_err(|e| vol_err(drive, e))?;
    let pos = match resume.and_then(|p| {
        vol.journal_info()
            .ok()
            .and_then(|info| resume_position(p, &info))
    }) {
        Some(pos) => pos,
        None => build_volume(state, &vol, drive)?,
    };
    watch_loop(state, &vol, drive, pos)
}

fn build_volume(
    state: &Arc<AppState>,
    vol: &VolumeHandle,
    drive: char,
) -> Result<JournalPos, VolErr> {
    let started = Instant::now();
    state.build_progress.lock().unwrap().insert(drive, 0);
    vol.ensure_journal().map_err(|e| vol_err(drive, e))?;

    let mut index = VolumeIndex::new(drive);
    let mut orphans: Vec<(u64, u64, String, bool)> = Vec::new();
    let mut count = 0u64;
    vol.enumerate_mft(|e| {
        if let InsertOutcome::Orphan = index.insert(e.frn, e.parent_frn, &e.name, e.is_dir) {
            orphans.push((e.frn, e.parent_frn, e.name, e.is_dir));
        }
        count += 1;
        if count.is_multiple_of(65536) {
            if let Some(p) = state.build_progress.lock().unwrap().get_mut(&drive) {
                *p = index.live_count();
            }
        }
    })
    .map_err(|e| vol_err(drive, e))?;

    // Parents can appear after children in MFT order — resolve iteratively.
    let mut dropped = 0usize;
    while !orphans.is_empty() {
        let before = orphans.len();
        let mut next = Vec::new();
        for (f, p, n, d) in orphans.drain(..) {
            if let InsertOutcome::Orphan = index.insert(f, p, &n, d) {
                next.push((f, p, n, d));
            }
        }
        if next.len() == before {
            dropped = next.len();
            break;
        }
        orphans = next;
    }
    if dropped > 0 {
        tracing::warn!("volume {drive}: dropped {dropped} entries with missing parents");
    }

    // Capture the journal position AFTER the enum: every change that happened
    // while scanning is covered by the journal from this point on.
    let info = vol.journal_info().map_err(|e| vol_err(drive, e))?;
    let pos = JournalPos {
        journal_id: info.journal_id,
        next_usn: info.next_usn as u64,
    };
    let files = index.live_count();
    let elapsed = started.elapsed();
    state
        .engine
        .init_volume(drive, index, Some(pos), VolumePhase::Ready);
    state.build_progress.lock().unwrap().remove(&drive);
    tracing::info!(
        "volume {drive}: indexed {files} entries in {:.2}s",
        elapsed.as_secs_f32()
    );
    Ok(pos)
}

fn vol_err(drive: char, e: FsError) -> VolErr {
    match e {
        // Retrying cannot help: the token does not have volume access.
        FsError::AccessDenied => VolErr::Fatal(format!(
            "{e}: MFT/USN access to volume {drive} needs an elevated (Administrator) \
             or LocalSystem token"
        )),
        FsError::JournalGone | FsError::NotReady => VolErr::Rebuild,
        other => {
            tracing::warn!("volume {drive}: {other}");
            VolErr::Retry(2000)
        }
    }
}

fn watch_loop(
    state: &Arc<AppState>,
    vol: &VolumeHandle,
    drive: char,
    mut pos: JournalPos,
) -> Result<(), VolErr> {
    let poll = Duration::from_millis(state.config.poll_ms.max(20));
    let mut warned_versions = false;
    let mut polls: u64 = 0;
    let mut records_seen: u64 = 0;
    let mut heartbeat = Instant::now() + Duration::from_secs(15);
    loop {
        if state.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        std::thread::sleep(poll);
        if state.engine.take_rebuild_flag(drive) {
            return Err(VolErr::Rebuild);
        }
        match vol.read_journal(pos, 500_000) {
            Ok((scan, next)) => {
                pos = next;
                polls += 1;
                records_seen += scan.records as u64;
                if scan.records > 0 {
                    // The numbers that matter when a change "does not show up":
                    // records the driver handed over vs events the index got.
                    tracing::debug!(
                        "volume {drive}: journal +{} records -> {} events ({} unparsable)",
                        scan.records,
                        scan.events.len(),
                        scan.other_versions
                    );
                }
                // A watch loop that polls and reads nothing is indistinguishable
                // from an idle volume without this line.
                if Instant::now() >= heartbeat {
                    heartbeat = Instant::now() + Duration::from_secs(15);
                    tracing::debug!(
                        "volume {drive}: journal at usn {} - {polls} poll(s), {records_seen} \
                         record(s) read so far",
                        pos.next_usn
                    );
                }
                if scan.other_versions > 0 && !warned_versions {
                    warned_versions = true;
                    tracing::warn!(
                        "volume {drive}: {} journal records are not USN_RECORD_V2 ({} total read) \
                         - those changes cannot be indexed",
                        scan.other_versions,
                        scan.records
                    );
                }
                if !scan.events.is_empty() {
                    state.engine.apply(drive, &scan.events);
                }
            }
            Err(FsError::JournalGone) | Err(FsError::NotReady) => {
                tracing::warn!("volume {drive}: journal unavailable - rebuilding");
                return Err(VolErr::Rebuild);
            }
            Err(e) => {
                tracing::warn!("volume {drive}: journal read: {e}");
            }
        }
    }
}

fn fail_volume(state: &Arc<AppState>, drive: char) {
    state
        .engine
        .init_volume(drive, VolumeIndex::new(drive), None, VolumePhase::Failed);
    state.build_progress.lock().unwrap().remove(&drive);
}
