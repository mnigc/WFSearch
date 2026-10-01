//! Query-time document-content search: candidates from the name index, bytes
//! from the filesystem. Everything here runs strictly *outside* the Engine
//! lock (the caller has already `search`ed and `materialize`d), so a slow
//! scan never delays USN application or ordinary name queries.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use wfs_content::{ContentMatcher, Scan};
use wfs_core::{FileEntry, Query, SearchOptions, SortKind};
use wfs_proto::{ContentScanInfo, FileResult, SearchReq, SearchResp, ERR_BAD_REQUEST};

use crate::state::AppState;

/// Execute a `content:` query end to end. `query.content_terms` must be
/// non-empty (the dispatcher routes on it).
pub fn search(
    state: &AppState,
    query: &Query,
    sr: &SearchReq,
) -> Result<SearchResp, (u32, String)> {
    let cfg = &state.config.content;
    if !cfg.enabled {
        return Err((
            ERR_BAD_REQUEST,
            "content search is disabled on this server ([content] enabled = false)".into(),
        ));
    }
    let t0 = Instant::now();
    let matcher = ContentMatcher::new(&query.content_terms);
    if matcher.is_empty() {
        return Err((ERR_BAD_REQUEST, "content: term is empty".into()));
    }

    // Candidate window: the name-level query decides what gets scanned, and
    // `limit`/`offset` apply to the *content* matches afterwards. Fixed index
    // order (sort = none): ordering the IO by name would buy no scan speed.
    let opts = SearchOptions {
        limit: cfg.max_candidates,
        offset: 0,
        sort: SortKind::None,
        match_path: sr.match_path,
    };
    let out = state.engine.search(query, &opts);
    let entries = state.engine.materialize(&out.hits);
    // Directories never get scanned, so the truncation verdict must compare
    // like with like: non-directory matches overall vs the non-directory
    // entries the window actually holds. (`total_matched` counts directories,
    // which could otherwise report `truncated` for an all-directories
    // overflow that cost nothing to cover.)
    let in_window_files = entries.iter().filter(|e| !e.is_dir).count() as u64;
    let truncated = out.non_dir_matched > in_window_files;

    // The engine's own data directory is never scanned: nothing there is a
    // document, and a SYSTEM-read token must not echo back to clients.
    let exclude = data_dir_prefix(state);
    let cand: Vec<&FileEntry> = entries
        .iter()
        .filter(|e| !e.is_dir && !under_dir(&e.path, exclude.as_deref()))
        .collect();

    let counters = Counters::default();
    let hits: Mutex<Vec<(usize, FileResult)>> = Mutex::new(Vec::new());
    let deadline = t0 + Duration::from_millis(cfg.timeout_ms);
    let cursor = AtomicUsize::new(0);
    let timed_out = AtomicBool::new(false);

    // Bounded threads with a shared cursor: rayon stays free for the name
    // scans every other query is running.
    let workers = (cfg.max_concurrency as usize).min(cand.len()).max(1);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                if timed_out.load(Ordering::Relaxed) {
                    return;
                }
                let i = cursor.fetch_add(1, Ordering::Relaxed);
                if i >= cand.len() {
                    return;
                }
                if Instant::now() >= deadline {
                    timed_out.store(true, Ordering::Relaxed);
                    return;
                }
                scan_one(cand[i], &matcher, cfg.max_file_bytes, &counters, &hits, i);
            });
        }
    });

    // Candidate order is the stable identity of the result set; concurrent
    // completion order is not.
    let mut hits = hits.into_inner().unwrap();
    hits.sort_by_key(|(i, _)| *i);
    let total = hits.len() as u64;
    let results = hits
        .into_iter()
        .skip(sr.offset as usize)
        .take(sr.limit.clamp(1, state.config.max_limit) as usize)
        .map(|(_, r)| r)
        .collect();

    Ok(SearchResp {
        total_matched: total,
        query_ms: t0.elapsed().as_millis() as u64,
        limit: sr.limit.clamp(1, state.config.max_limit),
        offset: sr.offset,
        results,
        content: Some(ContentScanInfo {
            scanned: counters.scanned.load(Ordering::Relaxed),
            skipped_size: counters.skipped_size.load(Ordering::Relaxed),
            skipped_binary: counters.skipped_binary.load(Ordering::Relaxed),
            errors: counters.errors.load(Ordering::Relaxed),
            truncated,
            timed_out: timed_out.load(Ordering::Relaxed),
        }),
    })
}

#[derive(Default)]
struct Counters {
    scanned: AtomicU32,
    skipped_size: AtomicU32,
    skipped_binary: AtomicU32,
    errors: AtomicU32,
}

fn scan_one(
    e: &FileEntry,
    matcher: &ContentMatcher,
    max_file_bytes: u64,
    counters: &Counters,
    hits: &Mutex<Vec<(usize, FileResult)>>,
    idx: usize,
) {
    // Open first, then stat the *handle*: stat-by-path followed by read would
    // race a swap/rename in between, and `fs::read` would swallow the
    // replacement at any size. `take` also caps a file that grows after the
    // stat.
    let Ok(mut f) = std::fs::File::open(&e.path) else {
        // deleted/renamed between materialize and open — routine, count it
        counters.errors.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let Ok(meta) = f.metadata() else {
        counters.errors.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if !meta.is_file() {
        counters.errors.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if meta.len() > max_file_bytes {
        counters.skipped_size.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let mut bytes = Vec::with_capacity(meta.len().min(1 << 20) as usize);
    use std::io::Read;
    if f.by_ref()
        .take(max_file_bytes)
        .read_to_end(&mut bytes)
        .is_err()
    {
        // locked or permission-denied even for the service account
        counters.errors.fetch_add(1, Ordering::Relaxed);
        return;
    }
    counters.scanned.fetch_add(1, Ordering::Relaxed);
    if let Scan::Match { snippet, count } = matcher.scan(&bytes, extension_of(&e.name)) {
        hits.lock().unwrap().push((
            idx,
            FileResult {
                name: e.name.clone(),
                path: e.path.clone(),
                is_dir: e.is_dir,
                snippet: Some(snippet),
                content_matches: Some(count),
            },
        ));
    }
}

fn extension_of(name: &str) -> &str {
    name.rsplit_once('.').map_or("", |(_, ext)| ext)
}

/// Canonicalized data dir in plain `C:\...` form. `canonicalize` emits the
/// `\\?\` prefix form; the index stores plain paths.
fn data_dir_prefix(state: &AppState) -> Option<String> {
    let canon = std::fs::canonicalize(state.config.data_dir_path()).ok()?;
    Some(plain_path(canon))
}

fn plain_path(p: PathBuf) -> String {
    let s = p.to_string_lossy().into_owned();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s
    }
}

/// Case-insensitive "path is strictly inside dir" (Windows paths fold case;
/// the index keeps the original casing of every component).
fn under_dir(path: &str, dir: Option<&str>) -> bool {
    let Some(dir) = dir else {
        return false;
    };
    let (p, d) = (path.as_bytes(), dir.as_bytes());
    if p.len() <= d.len() {
        return false;
    }
    let prefix_folded = p[..d.len()]
        .iter()
        .zip(d)
        .all(|(a, b)| a.eq_ignore_ascii_case(b));
    prefix_folded && (d.last() == Some(&b'\\') || p[d.len()] == b'\\')
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Component, Path};
    use wfs_core::{VolumeIndex, VolumePhase, ROOT_FRN};

    #[test]
    fn under_dir_needs_a_component_boundary() {
        assert!(under_dir(r"C:\data\file.txt", Some(r"C:\data")));
        assert!(under_dir(r"c:\DATA\sub\f", Some(r"C:\data")));
        assert!(under_dir(r"C:\data\file.txt", Some(r"C:\data\")));
        // C:\database is a sibling, not a child
        assert!(!under_dir(r"C:\database\f", Some(r"C:\data")));
        // the directory itself is not "inside" it
        assert!(!under_dir(r"C:\data", Some(r"C:\data")));
        assert!(!under_dir(r"D:\data\f", Some(r"C:\data")));
        assert!(!under_dir(r"C:\elsewhere\f", None));
    }

    #[test]
    fn extension_is_passed_through_without_the_dot() {
        // the matcher folds case itself; the container check is case-insensitive
        assert_eq!(extension_of("Report.DOCX"), "DOCX");
        assert_eq!(extension_of("noext"), "");
        assert_eq!(extension_of("a.b.c"), "c");
    }

    /// Index entries must carry the *real* temp-dir path for the scan to open
    /// actual files, so build the directory chain from the path's components.
    fn index_under(dir: &Path, files: &[(&str, bool)]) -> VolumeIndex {
        let mut idx = VolumeIndex::new('C');
        let mut frn: u64 = 1000;
        let mut parent: u64 = ROOT_FRN;
        for comp in dir.components().filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        }) {
            idx.insert(frn, parent, &comp, true);
            parent = frn;
            frn += 1;
        }
        for (name, is_dir) in files {
            idx.insert(frn, parent, name, *is_dir);
            frn += 1;
        }
        idx
    }

    fn zip_docx(text: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts: zip::write::SimpleFileOptions = Default::default();
            w.start_file("word/document.xml", opts).unwrap();
            use std::io::Write;
            w.write_all(format!(r#"<w:p><w:t>{text}</w:t></w:p>"#).as_bytes())
                .unwrap();
            w.finish().unwrap();
        }
        buf
    }

    /// Real files on disk, real dispatch — the full scan pipeline minus the
    /// transports (which are covered by the pipe/HTTP tests for name search).
    #[test]
    fn content_query_scans_real_files() {
        let scan_dir = crate::testutil::temp_dir("content");
        std::fs::write(scan_dir.join("report.txt"), "the Q3 budget fox").unwrap();
        std::fs::write(scan_dir.join("other.txt"), "nothing to see").unwrap();
        std::fs::write(scan_dir.join("doc.docx"), zip_docx("预算在 PPT 里")).unwrap();

        // the engine's data dir is a sibling: its own files must stay out
        let data_dir = crate::testutil::temp_dir("content-data");
        std::fs::write(data_dir.join("leak.txt"), "budget inside the data dir").unwrap();

        let cfg = crate::testutil::config_in(data_dir);
        let st = crate::state::AppState::try_new(cfg).expect("state");
        st.engine.init_volume(
            'C',
            index_under(
                &scan_dir,
                &[
                    ("report.txt", false),
                    ("other.txt", false),
                    ("doc.docx", false),
                ],
            ),
            None,
            VolumePhase::Ready,
        );

        let sr = SearchReq {
            q: "content:budget txt".into(),
            ..Default::default()
        };
        let query = Query::parse(&sr.q);
        let resp = search(&st, &query, &sr).expect("content search ok");

        assert_eq!(resp.total_matched, 1);
        assert_eq!(resp.results[0].name, "report.txt");
        let sn = resp.results[0].snippet.as_deref().expect("snippet");
        assert_eq!(sn, "the Q3 budget fox");
        assert_eq!(resp.results[0].content_matches, Some(1));

        let scan = resp.content.as_ref().expect("scan info");
        assert_eq!(scan.scanned, 2, "the two .txt files");
        assert_eq!(scan.errors, 0);
        assert!(!scan.truncated);

        // the docx container is found through its own query
        let sr = SearchReq {
            q: "content:预算 *.docx".into(),
            ..Default::default()
        };
        let resp = search(&st, &Query::parse(&sr.q), &sr).expect("docx search ok");
        assert_eq!(resp.total_matched, 1);
        assert!(resp.results[0].snippet.as_deref().unwrap().contains("预算"));

        // the engine's own data dir never echoes back
        let sr = SearchReq {
            q: "content:budget leak".into(),
            ..Default::default()
        };
        let resp = search(&st, &Query::parse(&sr.q), &sr).expect("leak probe ok");
        assert_eq!(resp.total_matched, 0, "data_dir files are not scanned");
    }

    #[test]
    fn disabled_content_search_is_a_clean_error() {
        let data_dir = crate::testutil::temp_dir("content-off");
        let mut cfg = crate::testutil::config_in(data_dir);
        cfg.content.enabled = false;
        let st = crate::state::AppState::try_new(cfg).expect("state");
        let sr = SearchReq {
            q: "content:x".into(),
            ..Default::default()
        };
        let query = Query::parse(&sr.q);
        let err = search(&st, &query, &sr).unwrap_err();
        assert_eq!(err.0, ERR_BAD_REQUEST);
        assert!(err.1.contains("disabled"));
    }

    #[test]
    fn pagination_applies_after_content_matching() {
        let scan_dir = crate::testutil::temp_dir("content-page");
        for i in 0..5 {
            std::fs::write(scan_dir.join(format!("f{i}.txt")), "hit".as_bytes()).unwrap();
        }
        let cfg = crate::testutil::config_in(crate::testutil::temp_dir("content-page-data"));
        let st = crate::state::AppState::try_new(cfg).expect("state");
        let files: Vec<(String, bool)> = (0..5).map(|i| (format!("f{i}.txt"), false)).collect();
        let refs: Vec<(&str, bool)> = files.iter().map(|(n, d)| (n.as_str(), *d)).collect();
        st.engine
            .init_volume('C', index_under(&scan_dir, &refs), None, VolumePhase::Ready);

        let sr = SearchReq {
            q: "content:hit".into(),
            limit: 2,
            offset: 1,
            ..Default::default()
        };
        let resp = search(&st, &Query::parse(&sr.q), &sr).expect("ok");
        assert_eq!(resp.total_matched, 5, "matches counted before paging");
        assert_eq!(
            resp.results.len(),
            2,
            "page size applies to content matches"
        );
    }

    /// A window that overflows with *directories* only must not report
    /// `truncated`: the files that matter were all scanned. Regression for
    /// judging truncation against `total_matched` (which counts directories).
    #[test]
    fn directory_overflow_does_not_report_truncated() {
        let scan_dir = crate::testutil::temp_dir("content-dirwin");
        for i in 0..2 {
            std::fs::write(scan_dir.join(format!("zx{i}.txt")), "hit".as_bytes()).unwrap();
        }
        let mut cfg = crate::testutil::config_in(crate::testutil::temp_dir("content-dirwin-data"));
        cfg.content.max_candidates = 2; // window holds exactly the two files
        let st = crate::state::AppState::try_new(cfg).expect("state");

        // index order: the two files first, then two matching directories —
        // the window (2 slots) takes the files, the directories overflow
        let mut idx = index_under(&scan_dir, &[("zx0.txt", false), ("zx1.txt", false)]);
        let base = 5000u64;
        idx.insert(base, ROOT_FRN, "zxd", true);
        idx.insert(base + 1, ROOT_FRN, "zxd", true);
        st.engine.init_volume('C', idx, None, VolumePhase::Ready);

        let sr = SearchReq {
            q: "content:hit zx".into(),
            ..Default::default()
        };
        let resp = search(&st, &Query::parse(&sr.q), &sr).expect("ok");
        assert_eq!(resp.total_matched, 2, "both files match on content");
        let scan = resp.content.as_ref().expect("scan info");
        assert!(!scan.truncated, "the overflow is directories only");
        assert_eq!(scan.scanned, 2);
    }
}
