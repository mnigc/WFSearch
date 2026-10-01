//! Request dispatch shared by the named pipe and HTTP transports.

use std::collections::BTreeMap;
use std::time::Instant;
use wfs_core::{Query, SearchOptions};
use wfs_proto::{
    ErrorPayload, FileResult, Request, Response, SearchReq, SearchResp, StatusResp, VolumeStatus,
    ERR_BAD_REQUEST, PROTOCOL_VERSION,
};

use crate::content;
use crate::state::AppState;

pub fn handle_request(state: &AppState, req: Request) -> Response {
    match req {
        Request::Ping => Response::Pong,
        Request::Status => Response::Status(status_resp(state)),
        Request::Search(sr) => match search_resp(state, sr) {
            Ok(r) => Response::Search(r),
            Err((code, msg)) => Response::err(code, msg),
        },
    }
}

pub fn search_resp(state: &AppState, sr: SearchReq) -> Result<SearchResp, (u32, String)> {
    if sr.q.trim().is_empty() {
        return Err((ERR_BAD_REQUEST, "query must not be empty".into()));
    }
    let query = Query::parse(&sr.q);
    // `content:` terms route to the query-time document scan; everything else
    // is the plain name/path path below.
    if !query.content_terms.is_empty() {
        return content::search(state, &query, &sr);
    }
    let opts = SearchOptions {
        limit: sr.limit.clamp(1, state.config.max_limit),
        offset: sr.offset,
        sort: sr.sort,
        match_path: sr.match_path,
    };
    let t0 = Instant::now();
    let out = state.engine.search(&query, &opts);
    let entries = state.engine.materialize(&out.hits);
    let results = entries
        .into_iter()
        .map(|e| FileResult {
            name: e.name,
            path: e.path,
            is_dir: e.is_dir,
            snippet: None,
            content_matches: None,
        })
        .collect();
    Ok(SearchResp {
        total_matched: out.total_matched,
        query_ms: t0.elapsed().as_millis() as u64,
        limit: opts.limit,
        offset: opts.offset,
        results,
        content: None,
    })
}

pub fn status_resp(state: &AppState) -> StatusResp {
    let est = state.engine.status();
    let mut volumes: Vec<VolumeStatus> = Vec::new();
    let mut building: BTreeMap<char, u64> = state.build_progress.lock().unwrap().clone();
    for v in est.volumes {
        building.remove(&v.drive);
        volumes.push(VolumeStatus {
            drive: v.drive.to_string(),
            phase: match v.phase {
                wfs_core::VolumePhase::Building => "building",
                wfs_core::VolumePhase::Ready => "ready",
                wfs_core::VolumePhase::Failed => "failed",
            }
            .into(),
            files: v.files,
            deleted: v.deleted,
            journal: v.journal,
            last_update_ms_ago: v.last_update_ms_ago,
        });
    }
    for (d, n) in building {
        volumes.push(VolumeStatus {
            drive: d.to_string(),
            phase: "building".into(),
            files: n,
            deleted: 0,
            journal: false,
            last_update_ms_ago: None,
        });
    }
    volumes.sort_by(|a, b| a.drive.cmp(&b.drive));
    StatusResp {
        version: wfs_core::VERSION.into(),
        protocol: PROTOCOL_VERSION,
        uptime_ms: state.started.elapsed().as_millis() as u64,
        approx_memory_bytes: est.approx_memory_bytes,
        volumes,
    }
}

/// Convenience for the console REPL / CLI docs.
pub fn error_payload(code: u32, msg: impl Into<String>) -> ErrorPayload {
    ErrorPayload {
        code,
        message: msg.into(),
    }
}
