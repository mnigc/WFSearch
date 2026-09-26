//! Local HTTP gateway (axum) — binds 127.0.0.1 only.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use wfs_core::SortKind;
use wfs_proto::{ErrorPayload, SearchReq, SearchResp, StatusResp, ERR_BAD_REQUEST, ERR_INTERNAL};

use crate::api;
use crate::state::AppState;

pub async fn spawn(state: Arc<AppState>) -> anyhow::Result<std::net::SocketAddr> {
    let port = state.config.http_port;
    let app = Router::new()
        .route("/api/v1/search", get(search_h))
        .route("/api/v1/status", get(status_h))
        .route("/api/v1/snapshot", post(snapshot_h))
        .with_state(state);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    tracing::info!("http: listening on http://{bound}");
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("http server: {e}");
        }
    });
    Ok(bound)
}

fn status_code(code: u32) -> StatusCode {
    match code {
        ERR_BAD_REQUEST => StatusCode::BAD_REQUEST,
        ERR_INTERNAL => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn parse_sort(s: Option<&String>) -> SortKind {
    match s.map(|x| x.as_str()) {
        Some("name") => SortKind::Name,
        Some("path") => SortKind::Path,
        _ => SortKind::None,
    }
}

async fn search_h(
    State(st): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<SearchResp>, (StatusCode, Json<ErrorPayload>)> {
    let sr = SearchReq {
        q: params.get("q").cloned().unwrap_or_default(),
        limit: params
            .get("limit")
            .and_then(|v| v.parse().ok())
            .unwrap_or(100),
        offset: params
            .get("offset")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        sort: parse_sort(params.get("sort")),
        match_path: params
            .get("match_path")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false),
    };
    // Off the reactor: a query is a full parallel scan (and `sort` may collect
    // every hit), which would otherwise stall the other connections.
    match tokio::task::spawn_blocking(move || api::search_resp(&st, sr)).await {
        Ok(Ok(r)) => Ok(Json(r)),
        Ok(Err((code, msg))) => Err((status_code(code), Json(api::error_payload(code, msg)))),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(api::error_payload(
                ERR_INTERNAL,
                format!("query task failed: {e}"),
            )),
        )),
    }
}

async fn status_h(State(st): State<Arc<AppState>>) -> Json<StatusResp> {
    Json(api::status_resp(&st))
}

async fn snapshot_h(
    State(st): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorPayload>)> {
    // Serializing every volume is heavy IO — keep it off the reactor too.
    match tokio::task::spawn_blocking(move || crate::snapshot::save(&st)).await {
        Ok(Ok(p)) => Ok(Json(serde_json::json!({"saved": p.display().to_string()}))),
        Ok(Err(e)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(api::error_payload(ERR_INTERNAL, e.to_string())),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(api::error_payload(
                ERR_INTERNAL,
                format!("snapshot task failed: {e}"),
            )),
        )),
    }
}

/// End-to-end over a real socket on an OS-assigned port, driving the gateway
/// through the published client (`wfs-client`), which is what practitioners
/// copy from `docs/clients.md`.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    async fn serve() -> (Arc<AppState>, u16) {
        let st = testutil::state();
        let addr = spawn(st.clone()).await.unwrap();
        (st, addr.port())
    }

    /// The client is blocking — keep it off the test's async worker.
    async fn on_blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        tokio::task::spawn_blocking(f).await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn search_and_status_over_http() {
        let (_, port) = serve().await;

        let resp = on_blocking(move || {
            wfs_client::search_http(
                &wfs_proto::SearchReq {
                    q: "notes*".into(),
                    limit: 20,
                    ..Default::default()
                },
                port,
            )
        })
        .await
        .expect("search over http");
        assert_eq!(resp.total_matched, 2);
        assert_eq!(resp.results.len(), 2);
        assert!(resp.results.iter().any(|r| r.path == r"C:\work\notes.txt"));
        assert!(resp.results.iter().any(|r| r.path == r"C:\notes.md"));

        let st = on_blocking(move || wfs_client::status_http(port))
            .await
            .unwrap();
        assert_eq!(st.protocol, wfs_proto::PROTOCOL_VERSION);
        assert_eq!(st.volumes.len(), 1);
        assert_eq!(st.volumes[0].drive, "C");
        assert_eq!(st.volumes[0].phase, "ready");
        assert_eq!(st.volumes[0].files, 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn match_path_query_param_is_honoured() {
        let (_, port) = serve().await;

        let by_name = on_blocking(move || {
            wfs_client::search_http(
                &wfs_proto::SearchReq {
                    q: "notes".into(),
                    limit: 20,
                    ..Default::default()
                },
                port,
            )
        })
        .await
        .unwrap();
        assert_eq!(by_name.total_matched, 2, "both names contain 'notes'");

        // With match_path the same term must also hold for the whole path.
        let with_path = on_blocking(move || {
            wfs_client::search_http(
                &wfs_proto::SearchReq {
                    q: r"work\notes".into(),
                    limit: 20,
                    match_path: true,
                    ..Default::default()
                },
                port,
            )
        })
        .await
        .unwrap();
        assert_eq!(with_path.total_matched, 1);
        assert_eq!(with_path.results[0].path, r"C:\work\notes.txt");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_query_is_a_400_not_a_500() {
        let (_, port) = serve().await;
        let body = on_blocking(move || {
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            use std::io::{Read, Write};
            write!(s, "GET /api/v1/search?q=%20 HTTP/1.0\r\nHost: x\r\n\r\n").unwrap();
            let mut raw = Vec::new();
            s.read_to_end(&mut raw).unwrap();
            String::from_utf8_lossy(&raw).to_string()
        })
        .await;
        assert!(body.contains("400 Bad Request"), "{body}");
        assert!(body.contains("query must not be empty"), "{body}");
    }
}
