//! The multi-volume engine: owns per-volume state (index + journal position
//! + incremental event application) and executes queries with rayon.

use crate::index::{InsertOutcome, Node, RenameOutcome, VolumeIndex, FLAG_DELETED, FLAG_DIR};
use crate::matcher::{Query, SortKind, Term};
use parking_lot::RwLock;
use rayon::prelude::*;
use std::time::SystemTime;

const CHUNK: usize = 8192;
/// When sorting by name/path we must collect every match before ordering —
/// guard memory with a cap.
const MAX_SORT_COLLECT: usize = 500_000;
/// Orphaned creates are retried this many batches before being dropped.
const ORPHAN_MAX_TRIES: u32 = 600;
/// Tombstone ratio that marks a volume for full rebuild.
const REBUILD_MIN_DELETED: u32 = 1024;
const REBUILD_RATIO: usize = 20;

/// One change event coming from the USN journal (already parsed).
#[derive(Debug, Clone)]
pub enum IndexEvent {
    Create {
        frn: u64,
        parent_frn: u64,
        name: String,
        is_dir: bool,
    },
    Delete {
        frn: u64,
    },
    RenameOld {
        frn: u64,
    },
    RenameNew {
        frn: u64,
        parent_frn: u64,
        name: String,
        is_dir: bool,
    },
}

/// Where the journal reader should continue from; persisted in snapshots.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JournalPos {
    pub journal_id: u64,
    pub next_usn: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VolumePhase {
    Building,
    Ready,
    /// unrecoverable build/watch error (missing volume, access denied…)
    Failed,
}

struct PendingCreate {
    frn: u64,
    parent_frn: u64,
    name: String,
    is_dir: bool,
    tries: u32,
}

pub struct VolumeState {
    pub index: VolumeIndex,
    pub journal: Option<JournalPos>,
    pub phase: VolumePhase,
    pending: Vec<PendingCreate>,
    rename_pending: ahash::AHashSet<u64>,
    needs_rebuild: bool,
    last_apply: Option<SystemTime>,
}

pub struct Engine {
    inner: RwLock<EngineInner>,
}

struct EngineInner {
    volumes: ahash::AHashMap<char, VolumeState>,
}

#[derive(Debug, Clone, Copy)]
pub struct SearchOptions {
    pub limit: u32,
    pub offset: u32,
    pub sort: SortKind,
    /// match every term against the full path instead of the file name
    pub match_path: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions {
            limit: 100,
            offset: 0,
            sort: SortKind::None,
            match_path: false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SearchHit {
    pub drive: char,
    pub slot: u32,
}

pub struct SearchOutput {
    pub hits: Vec<SearchHit>,
    pub total_matched: u64,
    /// non-directory matches. `total_matched` counts directories too; a
    /// content scan only ever opens files, so its truncation verdict must be
    /// judged against this.
    pub non_dir_matched: u64,
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VolumeStatusInfo {
    pub drive: char,
    pub phase: VolumePhase,
    pub files: u64,
    pub deleted: u64,
    pub journal: bool,
    pub needs_rebuild: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_update_ms_ago: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EngineStatus {
    pub volumes: Vec<VolumeStatusInfo>,
    pub approx_memory_bytes: u64,
}

impl Default for Engine {
    fn default() -> Engine {
        Engine::new()
    }
}

impl Engine {
    pub fn new() -> Engine {
        Engine {
            inner: RwLock::new(EngineInner {
                volumes: ahash::AHashMap::default(),
            }),
        }
    }

    /// Install (or replace) a whole volume index — used after a full MFT
    /// build, snapshot load, or rebuild.
    pub fn init_volume(
        &self,
        drive: char,
        index: VolumeIndex,
        journal: Option<JournalPos>,
        phase: VolumePhase,
    ) {
        let mut g = self.inner.write();
        g.volumes.insert(
            drive,
            VolumeState {
                index,
                journal,
                phase,
                pending: Vec::new(),
                rename_pending: ahash::AHashSet::default(),
                needs_rebuild: false,
                last_apply: None,
            },
        );
    }

    pub fn remove_volume(&self, drive: char) {
        self.inner.write().volumes.remove(&drive);
    }

    /// Apply a batch of incremental events for one volume.
    pub fn apply(&self, drive: char, events: &[IndexEvent]) {
        let mut g = self.inner.write();
        let Some(vs) = g.volumes.get_mut(&drive) else {
            return;
        };
        for ev in events {
            match ev.clone() {
                IndexEvent::Create {
                    frn,
                    parent_frn,
                    name,
                    is_dir,
                } => {
                    if let InsertOutcome::Orphan = vs.index.insert(frn, parent_frn, &name, is_dir) {
                        push_pending(&mut vs.pending, frn, parent_frn, name, is_dir);
                    }
                }
                IndexEvent::Delete { frn } => {
                    vs.rename_pending.remove(&frn);
                    vs.index.mark_deleted(frn);
                }
                IndexEvent::RenameOld { frn } => {
                    vs.rename_pending.insert(frn);
                }
                IndexEvent::RenameNew {
                    frn,
                    parent_frn,
                    name,
                    is_dir,
                } => {
                    if vs.rename_pending.remove(&frn) {
                        if vs.index.rename(frn, parent_frn, &name) == RenameOutcome::Orphan {
                            push_pending(&mut vs.pending, frn, parent_frn, name, is_dir);
                        }
                    } else {
                        // old-name record missed (journal window) — treat as create
                        if let InsertOutcome::Orphan =
                            vs.index.insert(frn, parent_frn, &name, is_dir)
                        {
                            push_pending(&mut vs.pending, frn, parent_frn, name, is_dir);
                        }
                    }
                }
            }
        }
        // retry pending creates whose parents have since appeared
        vs.pending.retain_mut(|p| {
            !matches!(
                vs.index.insert(p.frn, p.parent_frn, &p.name, p.is_dir),
                InsertOutcome::Added | InsertOutcome::Updated
            ) && {
                p.tries += 1;
                p.tries < ORPHAN_MAX_TRIES
            }
        });
        if vs.rename_pending.len() > 4096 {
            vs.rename_pending.clear();
        }
        vs.last_apply = Some(SystemTime::now());
        if vs.index.deleted > REBUILD_MIN_DELETED
            && vs.index.deleted as usize * REBUILD_RATIO > vs.index.nodes.len()
        {
            vs.needs_rebuild = true;
        }
    }

    /// Volumes flagged for rebuild (consumed by the watcher threads).
    pub fn take_rebuild_flag(&self, drive: char) -> bool {
        self.inner
            .write()
            .volumes
            .get_mut(&drive)
            .map(|vs| std::mem::take(&mut vs.needs_rebuild))
            .unwrap_or(false)
    }

    /// Execute a query. Names only (v1); empty term list matches everything.
    pub fn search(&self, query: &Query, opts: &SearchOptions) -> SearchOutput {
        let g = self.inner.read();
        let mut hits: Vec<SearchHit> = Vec::new();
        let mut total: u64 = 0;
        let mut non_dir: u64 = 0;

        // Terms that must match the file name (cheap) and terms that must match
        // the materialized path. AND order does not change the result, so path
        // terms are only evaluated for candidates the name terms let through.
        let mut name_terms: Vec<&Term> = Vec::new();
        let mut path_terms: Vec<&Term> = Vec::new();
        for t in &query.terms {
            if opts.match_path {
                path_terms.push(t);
            } else {
                name_terms.push(t);
            }
        }
        path_terms.extend(query.path_terms.iter());

        let mut drives: Vec<char> = g.volumes.keys().copied().collect();
        drives.sort_unstable();
        for drive in drives {
            if !query.drives.is_empty() && !query.drives.contains(&drive.to_ascii_lowercase()) {
                continue;
            }
            let vs = &g.volumes[&drive];
            if vs.phase != VolumePhase::Ready {
                continue;
            }
            let index = &vs.index;
            let names = &index.names;
            let nodes = &index.nodes;

            let qualifies = |slot: u32, node: &Node| -> bool {
                if !name_terms
                    .iter()
                    .all(|t| t.matches(names.get(node.name_off, node.name_len)))
                {
                    return false;
                }
                if path_terms.is_empty() {
                    return true;
                }
                let path = index.path_of(slot);
                path_terms.iter().all(|t| t.matches(path.as_bytes()))
            };

            if opts.sort == SortKind::Name || opts.sort == SortKind::Path {
                // collect everything (capped, deterministically by slot) then sort
                let mut collected: Vec<u32> = nodes
                    .par_iter()
                    .enumerate()
                    .filter_map(|(slot, node)| {
                        if node.flags & FLAG_DELETED != 0 {
                            return None;
                        }
                        qualifies(slot as u32, node).then_some(slot as u32)
                    })
                    .collect();
                total += collected.len() as u64;
                non_dir += collected
                    .iter()
                    .filter(|&&s| nodes[s as usize].flags & FLAG_DIR == 0)
                    .count() as u64;
                if collected.len() > MAX_SORT_COLLECT {
                    collected.sort_unstable();
                    collected.truncate(MAX_SORT_COLLECT);
                }
                hits.extend(collected.into_iter().map(|slot| SearchHit { drive, slot }));
            } else {
                let cap = opts
                    .offset
                    .saturating_add(opts.limit)
                    .min(MAX_SORT_COLLECT as u32) as usize;
                let chunk_hits: Vec<(u64, u64, Vec<u32>)> = nodes
                    .par_chunks(CHUNK)
                    .enumerate()
                    .map(|(ci, chunk)| {
                        let base = (ci * CHUNK) as u32;
                        let mut out = Vec::new();
                        let mut cnt: u64 = 0;
                        let mut nd: u64 = 0;
                        for (i, node) in chunk.iter().enumerate() {
                            if node.flags & FLAG_DELETED != 0 {
                                continue;
                            }
                            if qualifies(base + i as u32, node) {
                                cnt += 1;
                                if node.flags & FLAG_DIR == 0 {
                                    nd += 1;
                                }
                                if out.len() < cap {
                                    out.push(base + i as u32);
                                }
                            }
                        }
                        (cnt, nd, out)
                    })
                    .collect();
                let mut merged: Vec<u32> = Vec::with_capacity(chunk_hits.len() * cap.min(64));
                for (cnt, nd, mut part) in chunk_hits {
                    total += cnt;
                    non_dir += nd;
                    merged.append(&mut part);
                }
                merged.sort_unstable();
                hits.extend(
                    merged
                        .into_iter()
                        .take(cap)
                        .map(|slot| SearchHit { drive, slot }),
                );
            }
        }

        if opts.sort == SortKind::Name {
            hits.sort_by(|a, b| name_key(&g, a).cmp(name_key(&g, b)));
        } else if opts.sort == SortKind::Path {
            // Cached key: `path_of` allocates, so calling it twice per
            // comparison (as `sort_by` would) dominates the cost of the sort.
            hits.sort_by_cached_key(|h| path_key(&g, h));
        }

        let start = opts.offset as usize;
        let end = start + opts.limit as usize;
        let sliced: Vec<SearchHit> = hits.into_iter().skip(start).take(end - start).collect();
        SearchOutput {
            hits: sliced,
            total_matched: total,
            non_dir_matched: non_dir,
        }
    }

    /// Build result strings for the returned page (one read lock).
    pub fn materialize(&self, hits: &[SearchHit]) -> Vec<FileEntry> {
        let g = self.inner.read();
        hits.iter()
            .filter_map(|h| {
                let vs = g.volumes.get(&h.drive)?;
                let node = vs.index.nodes.get(h.slot as usize)?;
                if node.flags & FLAG_DELETED != 0 {
                    return None;
                }
                let name = vs
                    .index
                    .names
                    .get_str(node.name_off, node.name_len)
                    .to_string();
                let path = vs.index.path_of(h.slot);
                Some(FileEntry {
                    name,
                    path,
                    is_dir: node.flags & crate::index::FLAG_DIR != 0,
                })
            })
            .collect()
    }

    pub fn status(&self) -> EngineStatus {
        let g = self.inner.read();
        let mut volumes = Vec::new();
        let mut mem = 0u64;
        let mut drives: Vec<char> = g.volumes.keys().copied().collect();
        drives.sort_unstable();
        for d in drives {
            let vs = &g.volumes[&d];
            mem += vs.index.approx_memory();
            volumes.push(VolumeStatusInfo {
                drive: d,
                phase: vs.phase,
                files: vs.index.live_count(),
                deleted: vs.index.deleted as u64,
                journal: vs.journal.is_some(),
                needs_rebuild: vs.needs_rebuild,
                last_update_ms_ago: vs.last_apply.map(|t| {
                    SystemTime::now()
                        .duration_since(t)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0)
                }),
            });
        }
        EngineStatus {
            volumes,
            approx_memory_bytes: mem,
        }
    }

    /// Clone out all volumes for snapshotting.
    pub fn snapshot_data(&self) -> Vec<(char, VolumeIndex, Option<JournalPos>)> {
        let g = self.inner.read();
        let mut out = Vec::new();
        for (d, vs) in &g.volumes {
            if vs.phase == VolumePhase::Ready {
                out.push((*d, vs.index.clone(), vs.journal));
            }
        }
        out.sort_by_key(|(d, _, _)| *d);
        out
    }

    pub fn drives(&self) -> Vec<char> {
        let g = self.inner.read();
        let mut v: Vec<char> = g.volumes.keys().copied().collect();
        v.sort_unstable();
        v
    }
}

fn name_key<'a>(g: &'a EngineInner, h: &SearchHit) -> &'a str {
    g.volumes[&h.drive].index.names.get_str(
        g.volumes[&h.drive].index.nodes[h.slot as usize].name_off,
        g.volumes[&h.drive].index.nodes[h.slot as usize].name_len,
    )
}

fn path_key(g: &EngineInner, h: &SearchHit) -> String {
    g.volumes[&h.drive].index.path_of(h.slot)
}

fn push_pending(
    pending: &mut Vec<PendingCreate>,
    frn: u64,
    parent_frn: u64,
    name: String,
    is_dir: bool,
) {
    if pending.len() < 100_000 {
        pending.push(PendingCreate {
            frn,
            parent_frn,
            name,
            is_dir,
            tries: 0,
        });
    }
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{VolumeIndex, ROOT_FRN};

    fn create(frn: u64, parent: u64, name: &str, is_dir: bool) -> IndexEvent {
        IndexEvent::Create {
            frn,
            parent_frn: parent,
            name: name.to_string(),
            is_dir,
        }
    }

    #[test]
    fn incremental_flow() {
        let engine = Engine::new();
        engine.init_volume(
            'C',
            VolumeIndex::new('C'),
            Some(JournalPos {
                journal_id: 1,
                next_usn: 100,
            }),
            VolumePhase::Ready,
        );

        engine.apply(
            'C',
            &[
                create(10, ROOT_FRN, "work", true),
                create(11, 10, "a.txt", false),
                create(12, 10, "b.log", false),
            ],
        );
        let q = Query::parse("txt");
        let out = engine.search(&q, &SearchOptions::default());
        assert_eq!(out.total_matched, 1);

        // rename keeps one result with the new name
        engine.apply(
            'C',
            &[
                IndexEvent::RenameOld { frn: 11 },
                IndexEvent::RenameNew {
                    frn: 11,
                    parent_frn: 10,
                    name: "renamed.txt".into(),
                    is_dir: false,
                },
            ],
        );
        let q = Query::parse("a.txt");
        assert_eq!(
            engine.search(&q, &SearchOptions::default()).total_matched,
            0
        );
        let out = engine.search(&Query::parse("renamed"), &SearchOptions::default());
        assert_eq!(out.total_matched, 1);
        let entries = engine.materialize(&out.hits);
        assert_eq!(entries[0].path, "C:\\work\\renamed.txt");

        // delete dir removes children
        engine.apply('C', &[IndexEvent::Delete { frn: 10 }]);
        assert_eq!(
            engine
                .search(&Query::parse("renamed"), &SearchOptions::default())
                .total_matched,
            0
        );
        assert_eq!(
            engine
                .search(&Query::parse(""), &SearchOptions::default())
                .total_matched,
            1 // root only
        );
    }

    #[test]
    fn rename_new_without_old_acts_as_create() {
        let engine = Engine::new();
        engine.init_volume('C', VolumeIndex::new('C'), None, VolumePhase::Ready);
        engine.apply(
            'C',
            &[IndexEvent::RenameNew {
                frn: 7,
                parent_frn: ROOT_FRN,
                name: "x.txt".into(),
                is_dir: false,
            }],
        );
        assert_eq!(
            engine
                .search(&Query::parse("x.txt"), &SearchOptions::default())
                .total_matched,
            1
        );
    }

    #[test]
    fn orphan_create_resolved_by_later_parent() {
        let engine = Engine::new();
        engine.init_volume('C', VolumeIndex::new('C'), None, VolumePhase::Ready);
        engine.apply('C', &[create(2, 1, "kid.txt", false)]);
        assert_eq!(
            engine
                .search(&Query::parse("kid"), &SearchOptions::default())
                .total_matched,
            0
        );
        engine.apply('C', &[create(1, ROOT_FRN, "dad", true)]);
        assert_eq!(
            engine
                .search(&Query::parse("kid"), &SearchOptions::default())
                .total_matched,
            1
        );
        let out = engine.search(&Query::parse("kid"), &SearchOptions::default());
        assert_eq!(engine.materialize(&out.hits)[0].path, "C:\\dad\\kid.txt");
    }

    #[test]
    fn pagination_and_drive_filter() {
        let mut big = VolumeIndex::new('C');
        for i in 0..300u32 {
            big.insert(1000 + i as u64, ROOT_FRN, &format!("file{i:03}.txt"), false);
        }
        let engine = Engine::new();
        engine.init_volume('C', big, None, VolumePhase::Ready);

        let opts = SearchOptions {
            limit: 10,
            offset: 5,
            sort: SortKind::Name,
            ..Default::default()
        };
        let out = engine.search(&Query::parse("file"), &opts);
        assert_eq!(out.total_matched, 300);
        assert_eq!(out.non_dir_matched, 300, "all matches are files here");
        assert_eq!(out.hits.len(), 10);
        let e = engine.materialize(&out.hits);
        assert_eq!(e[0].name, "file005.txt");
        assert_eq!(e[9].name, "file014.txt");

        // drive filter excluding everything
        let out = engine.search(&Query::parse("d:"), &SearchOptions::default());
        assert_eq!(out.total_matched, 0);
        // drive filter including: 300 files + the root directory
        let out = engine.search(&Query::parse("c:"), &SearchOptions::default());
        assert_eq!(out.total_matched, 301);
        assert_eq!(out.non_dir_matched, 300);
    }

    #[test]
    fn path_terms_and_match_path_flag() {
        let mut idx = VolumeIndex::new('C');
        idx.insert(10, ROOT_FRN, "work", true);
        idx.insert(11, 10, "notes.txt", false);
        idx.insert(12, ROOT_FRN, "notes.md", false);
        let engine = Engine::new();
        engine.init_volume('C', idx, None, VolumePhase::Ready);

        let count = |q: &str, match_path: bool| {
            let opts = SearchOptions {
                match_path,
                ..Default::default()
            };
            engine.search(&Query::parse(q), &opts).total_matched
        };

        // a term with a separator matches the path...
        assert_eq!(count(r"work\notes", false), 1);
        // ...including the directory's own path, which also covers its parent
        assert_eq!(count(r"C:\work", false), 2);
        // ...and does not match the other way round
        assert_eq!(count(r"notes\work", false), 0);

        // name terms are unaffected, and AND with path terms
        assert_eq!(count("notes", false), 2);
        assert_eq!(count(r"txt work\", false), 1);

        // `match_path` pushes every term onto the path: "work" now also finds
        // the file inside the directory, not just the directory itself
        assert_eq!(count("work", false), 1);
        assert_eq!(count("work", true), 2);

        let out = engine.search(
            &Query::parse("notes.txt"),
            &SearchOptions {
                match_path: true,
                ..Default::default()
            },
        );
        assert_eq!(engine.materialize(&out.hits)[0].path, "C:\\work\\notes.txt");
    }
}
