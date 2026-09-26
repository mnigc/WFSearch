//! WFSearch wire protocol: JSON payloads shared by the Named Pipe channel and
//! the local HTTP gateway. Framing (length prefix vs HTTP) is handled by the
//! transports; this crate only defines message types.

use serde::{Deserialize, Serialize};
pub use wfs_core::SortKind;

pub const PROTOCOL_VERSION: u32 = 1;
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResp {
    pub total_matched: u64,
    pub query_ms: u64,
    pub limit: u32,
    pub offset: u32,
    pub results: Vec<FileResult>,
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
            }],
        });
        let j = serde_json::to_string(&resp).unwrap();
        match serde_json::from_str::<Response>(&j).unwrap() {
            Response::Search(r) => {
                assert_eq!(r.total_matched, 2);
                assert_eq!(r.results[0].path, r"C:\work\notes.txt");
                assert!(!r.results[0].is_dir);
            }
            other => panic!("expected search, got {other:?}"),
        }
    }
}
