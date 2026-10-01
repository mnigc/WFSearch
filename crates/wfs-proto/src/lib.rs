//! WFSearch wire protocol: JSON payloads shared by the Named Pipe channel and
//! the local HTTP gateway. Framing (length prefix vs HTTP) is handled by the
//! transports; this crate only defines message types.

use serde::{Deserialize, Serialize};
pub use wfs_core::SortKind;

/// v2: content search (`content:` terms in `q`; optional `content` scan info
/// on `SearchResp`, optional `snippet`/`content_matches` on `FileResult`).
/// All additions are optional fields, so v1 clients keep working.
pub const PROTOCOL_VERSION: u32 = 2;
pub const DEFAULT_PIPE_NAME: &str = r"\\.\pipe\wfs-engine-v1";
pub const DEFAULT_HTTP_PORT: u16 = 15100;

fn default_limit() -> u32 {
    100
}

// ---------------------------------------------------------------- requests

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchReq {
    pub q: String,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
    #[serde(default)]
    pub sort: SortKind,
    /// match every term against the full path instead of the file name
    /// (much slower: paths are materialized per candidate)
    #[serde(default)]
    pub match_path: bool,
}

impl Default for SearchReq {
    fn default() -> Self {
        SearchReq {
            q: String::new(),
            limit: default_limit(),
            offset: 0,
            sort: SortKind::None,
            match_path: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", content = "args", rename_all = "snake_case")]
pub enum Request {
    Search(SearchReq),
    Status,
    Ping,
}

// --------------------------------------------------------------- responses

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileResult {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    /// content-search context around the first hit; absent on name searches
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// content-search hit count in this file; absent on name searches
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_matches: Option<u32>,
}

/// Bookkeeping for a `content:` query's scan phase. `total_matched` counts
/// content matches; every candidate the scan saw is accounted for in one of
/// the counters below.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentScanInfo {
    /// candidates that were read and matched against
    pub scanned: u32,
    /// candidates larger than the configured per-file cap
    pub skipped_size: u32,
    /// candidates rejected as binary
    pub skipped_binary: u32,
    /// candidates that could not be read or parsed (locked, corrupt…)
    pub errors: u32,
    /// the name-level matches exceeded the candidate window, so not every
    /// matching file was scanned — narrow the name terms for full coverage
    pub truncated: bool,
    /// the time budget ran out before all candidates were scanned
    pub timed_out: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResp {
    pub total_matched: u64,
    pub query_ms: u64,
    pub limit: u32,
    pub offset: u32,
    pub results: Vec<FileResult>,
    /// present only for `content:` queries (`query_ms` includes the scan)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ContentScanInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeStatus {
    pub drive: String,
    /// "building" | "ready" | "pending-rebuild"
    pub phase: String,
    /// live (non-deleted) indexed entries
    pub files: u64,
    pub deleted: u64,
    pub journal: bool,
    /// ms since the last incremental update was applied, if any
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_update_ms_ago: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResp {
    pub version: String,
    pub protocol: u32,
    pub uptime_ms: u64,
    pub approx_memory_bytes: u64,
    pub volumes: Vec<VolumeStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub code: u32,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Response {
    Search(SearchResp),
    Status(StatusResp),
    Pong,
    Err(ErrorPayload),
}

impl Response {
    pub fn err(code: u32, message: impl Into<String>) -> Response {
        Response::Err(ErrorPayload {
            code,
            message: message.into(),
        })
    }
}

pub const ERR_BAD_REQUEST: u32 = 1;
pub const ERR_NOT_READY: u32 = 2;
pub const ERR_INTERNAL: u32 = 3;
/// HTTP only: the bearer token was absent or wrong. The named pipe carries the
/// client's Windows identity instead, so it never returns this.
pub const ERR_UNAUTHORIZED: u32 = 4;

/// The JSON shapes below are a published contract: `docs/protocol.md` and the
/// Python/C# samples in `docs/clients.md` are written against them by hand, so
/// any change here breaks clients that never see this crate.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_frames_match_the_documented_shape() {
        let j = serde_json::to_string(&Request::Search(SearchReq {
            q: "*.rs".into(),
            limit: 10,
            ..Default::default()
        }))
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        assert_eq!(v["op"], "search");
        assert_eq!(v["args"]["q"], "*.rs");
        assert_eq!(v["args"]["limit"], 10);

        assert_eq!(
            serde_json::to_string(&Request::Ping).unwrap(),
            r#"{"op":"ping"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Status).unwrap(),
            r#"{"op":"status"}"#
        );
    }

    #[test]
    fn search_args_are_optional_except_q() {
        let r: Request = serde_json::from_str(r#"{"op":"search","args":{"q":"x"}}"#).unwrap();
        match r {
            Request::Search(sr) => {
                assert_eq!(sr.limit, 100);
                assert_eq!(sr.offset, 0);
                assert_eq!(sr.sort, SortKind::None);
                assert!(!sr.match_path);
            }
            other => panic!("expected search, got {other:?}"),
        }
    }

    #[test]
    fn response_frames_match_the_documented_shape() {
        assert_eq!(
            serde_json::to_string(&Response::Pong).unwrap(),
            r#"{"type":"pong"}"#
        );
        assert_eq!(
            serde_json::to_string(&Response::err(ERR_BAD_REQUEST, "query must not be empty"))
                .unwrap(),
            r#"{"type":"err","data":{"code":1,"message":"query must not be empty"}}"#
        );
    }

    #[test]
    fn sort_kinds_are_lowercase_on_the_wire() {
        for (kind, wire) in [
            (SortKind::None, "none"),
            (SortKind::Name, "name"),
            (SortKind::Path, "path"),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{wire}\""));
        }
        let sr: SearchReq = serde_json::from_str(r#"{"q":"a","sort":"path"}"#).unwrap();
        assert_eq!(sr.sort, SortKind::Path);
    }

    #[test]
    fn volume_status_omits_an_absent_last_update() {
        let v = VolumeStatus {
            drive: "C".into(),
            phase: "ready".into(),
            files: 4,
            deleted: 0,
            journal: true,
            last_update_ms_ago: None,
        };
        let j = serde_json::to_string(&v).unwrap();
        assert!(!j.contains("last_update_ms_ago"), "{j}");

        let with_update = VolumeStatus {
            last_update_ms_ago: Some(7),
            ..v
        };
        assert!(serde_json::to_string(&with_update)
            .unwrap()
            .contains(r#""last_update_ms_ago":7"#));
    }

    #[test]
    fn responses_roundtrip_through_json() {
        let resp = Response::Search(SearchResp {
            total_matched: 2,
            query_ms: 1,
            limit: 100,
            offset: 0,
            results: vec![FileResult {
                name: "notes.txt".into(),
                path: r"C:\work\notes.txt".into(),
                is_dir: false,
                snippet: None,
                content_matches: None,
            }],
            content: None,
        });
        let j = serde_json::to_string(&resp).unwrap();
        // a name search keeps the v1 wire shape exactly: no content keys
        assert!(!j.contains("snippet"), "{j}");
        assert!(!j.contains("content"), "{j}");
        match serde_json::from_str::<Response>(&j).unwrap() {
            Response::Search(r) => {
                assert_eq!(r.total_matched, 2);
                assert_eq!(r.results[0].path, r"C:\work\notes.txt");
                assert!(!r.results[0].is_dir);
            }
            other => panic!("expected search, got {other:?}"),
        }
    }

    #[test]
    fn content_fields_appear_only_when_present() {
        let resp = Response::Search(SearchResp {
            total_matched: 1,
            query_ms: 5,
            limit: 100,
            offset: 0,
            results: vec![FileResult {
                name: "a.docx".into(),
                path: r"C:\work\a.docx".into(),
                is_dir: false,
                snippet: Some("…Q3 预算…".into()),
                content_matches: Some(3),
            }],
            content: Some(ContentScanInfo {
                scanned: 7,
                skipped_size: 1,
                skipped_binary: 2,
                errors: 0,
                truncated: false,
                timed_out: false,
            }),
        });
        let j = serde_json::to_string(&resp).unwrap();
        assert!(j.contains(r#""snippet":"…Q3 预算…""#), "{j}");
        assert!(j.contains(r#""content_matches":3"#), "{j}");
        assert!(j.contains(r#""content":{"scanned":7"#), "{j}");
        let back = match serde_json::from_str::<Response>(&j).unwrap() {
            Response::Search(r) => r,
            other => panic!("expected search, got {other:?}"),
        };
        assert_eq!(back.content.unwrap().scanned, 7);
        assert_eq!(back.results[0].content_matches, Some(3));
    }
}
