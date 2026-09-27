//! Local HTTP gateway (axum) — binds 127.0.0.1 only.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use wfs_core::SortKind;
use wfs_proto::{
    ErrorPayload, SearchReq, SearchResp, StatusResp, ERR_BAD_REQUEST, ERR_INTERNAL,
    ERR_UNAUTHORIZED,
};

use crate::api;
use crate::state::AppState;

/// Same-origin demo UI served at `/` — no CORS exposure: the endpoint stays
/// loopback-only and other origins cannot read these responses.
const UI: &str = include_str!("search_ui.html");

/// Header the gateway demands on every route, including the UI.
const TOKEN_HEADER: &str = "x-wfs-token";

pub async fn spawn(state: Arc<AppState>) -> anyhow::Result<std::net::SocketAddr> {
    let port = state.config.http_port;
    let token_path = state.config.token_path();
    let app = Router::new()
        .route("/", get(ui_h))
        .route("/api/v1/search", get(search_h))
        .route("/api/v1/status", get(status_h))
        .route("/api/v1/snapshot", post(snapshot_h))
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        .with_state(state);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    tracing::info!(
        "http: listening on http://{bound}; every request must send {TOKEN_HEADER} \
         with the contents of {}",
        token_path.display()
    );
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("http server: {e}");
        }
    });
    Ok(bound)
}

/// Loopback carries no per-user identity, so the token file's DACL is what
/// decides who may query the engine over HTTP.
async fn require_token(State(st): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let presented = req
        .headers()
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| query_token(req.uri().query()));
    match presented {
        Some(t) if token_eq(&t, &st.http_token) => next.run(req).await,
        _ => unauthorized(&req, &st),
    }
}

fn query_token(query: Option<&str>) -> Option<String> {
    // The token is hex, so it never arrives percent-encoded.
    query?
        .split('&')
        .find_map(|kv| kv.strip_prefix("token="))
        .map(str::to_owned)
}

/// Length-checked, then branch-free: short-circuiting on the first mismatch
/// would tell a guesser how many leading bytes it had right.
fn token_eq(given: &str, want: &str) -> bool {
    let (a, b) = (given.as_bytes(), want.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn unauthorized(req: &Request, st: &AppState) -> Response {
    if req.uri().path() == "/" {
        // A person typed this URL; answer in the browser, and never echo the
        // token — that would make the page the thing it guards.
        return (
            StatusCode::UNAUTHORIZED,
            Html(format!(
                "<!doctype html><meta charset=utf-8><title>WFSearch · 需要 token</title>\
                 <body style=\"font:15px/1.7 'Segoe UI',system-ui,sans-serif;margin:40px;max-width:720px\">\
                 <h2>需要访问 token</h2>\
                 <p>HTTP 网关只认 token，不认回环地址（回环不携带用户身份）。</p>\
                 <p>token 内容在：<code>{}</code></p>\
                 <p>用这种方式打开：<code>http://127.0.0.1:{}/?token=&lt;上面文件的内容&gt;</code></p>",
                st.config.token_path().display(),
                st.config.http_port
            )),
        )
            .into_response();
    }
    (
        StatusCode::UNAUTHORIZED,
        Json(api::error_payload(
            ERR_UNAUTHORIZED,
            "missing or invalid x-wfs-token",
        )),
    )
        .into_response()
}

fn status_code(code: u32) -> StatusCode {
    match code {
        ERR_BAD_REQUEST => StatusCode::BAD_REQUEST,
        ERR_UNAUTHORIZED => StatusCode::UNAUTHORIZED,
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

async fn ui_h() -> Html<&'static str> {
    Html(UI)
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
        let port = addr.port();
        (st, port)
    }

    /// The client is blocking — keep it off the test's async worker.
    async fn on_blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        tokio::task::spawn_blocking(f).await.unwrap()
    }

    /// One raw HTTP/1.0 exchange, because these tests must control the headers
    /// and the query string the SDK client fixes.
    async fn raw(port: u16, request: String) -> (String, String) {
        on_blocking(move || {
            use std::io::{Read, Write};
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(s, "{request}").unwrap();
            let mut bytes = Vec::new();
            s.read_to_end(&mut bytes).unwrap();
            let text = String::from_utf8_lossy(&bytes).to_string();
            match text.split_once("\r\n\r\n") {
                Some((h, b)) => (h.to_string(), b.to_string()),
                None => (text, String::new()),
            }
        })
        .await
    }

    fn get(path: &str, token: Option<&str>) -> String {
        match token {
            Some(t) => format!("GET {path} HTTP/1.0\r\nHost: x\r\n{TOKEN_HEADER}: {t}\r\n\r\n"),
            None => format!("GET {path} HTTP/1.0\r\nHost: x\r\n\r\n"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn search_and_status_over_http() {
        let (st, port) = serve().await;
        let token = st.http_token.clone();

        let resp = on_blocking(move || {
            wfs_client::search_http(
                &wfs_proto::SearchReq {
                    q: "notes*".into(),
                    limit: 20,
                    ..Default::default()
                },
                port,
                &token,
            )
        })
        .await
        .expect("search over http");
        assert_eq!(resp.total_matched, 2);
        assert_eq!(resp.results.len(), 2);
        assert!(resp.results.iter().any(|r| r.path == r"C:\work\notes.txt"));
        assert!(resp.results.iter().any(|r| r.path == r"C:\notes.md"));

        let token = st.http_token.clone();
        let st = on_blocking(move || wfs_client::status_http(port, &token))
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
        let (st, port) = serve().await;

        let token = st.http_token.clone();
        let by_name = on_blocking(move || {
            wfs_client::search_http(
                &wfs_proto::SearchReq {
                    q: "notes".into(),
                    limit: 20,
                    ..Default::default()
                },
                port,
                &token,
            )
        })
        .await
        .unwrap();
        assert_eq!(by_name.total_matched, 2, "both names contain 'notes'");

        // With match_path the same term must also hold for the whole path.
        let token = st.http_token.clone();
        let with_path = on_blocking(move || {
            wfs_client::search_http(
                &wfs_proto::SearchReq {
                    q: r"work\notes".into(),
                    limit: 20,
                    match_path: true,
                    ..Default::default()
                },
                port,
                &token,
            )
        })
        .await
        .unwrap();
        assert_eq!(with_path.total_matched, 1);
        assert_eq!(with_path.results[0].path, r"C:\work\notes.txt");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_api_route_requires_the_token() {
        let (_, port) = serve().await;
        for (path, verb) in [
            ("/api/v1/search?q=notes", "GET"),
            ("/api/v1/status", "GET"),
            ("/api/v1/snapshot", "POST"),
        ] {
            let (head, body) =
                raw(port, format!("{verb} {path} HTTP/1.0\r\nHost: x\r\n\r\n")).await;
            assert!(head.contains("401"), "{verb} {path} -> {head}");
            assert!(
                body.contains(&format!("\"code\":{ERR_UNAUTHORIZED}")),
                "{verb} {path} -> {body}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_wrong_token_is_rejected_and_leaks_nothing() {
        let (st, port) = serve().await;
        let wrong = "0".repeat(st.http_token.len());
        assert_ne!(wrong, st.http_token);
        let (head, body) = raw(port, get("/api/v1/status", Some(&wrong))).await;
        assert!(head.contains("401"), "{head}");
        assert!(
            !body.contains(&st.http_token),
            "reply must not echo the token"
        );
        assert!(
            body.contains(&format!("\"code\":{ERR_UNAUTHORIZED}")),
            "{body}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn token_is_accepted_via_query_param_too() {
        // A browser cannot set a header from the address bar, and the UI's
        // fetch replays it as a header — the query form is what loads the page.
        let (st, port) = serve().await;
        let (head, _) = raw(
            port,
            get(&format!("/api/v1/status?token={}", st.http_token), None),
        )
        .await;
        assert!(head.contains("200 OK"), "{head}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn root_serves_the_ui_only_with_a_token() {
        let (st, port) = serve().await;
        let (head, body) = raw(port, get("/", Some(&st.http_token))).await;
        assert!(head.contains("200 OK"), "{head}");
        assert!(head.to_ascii_lowercase().contains("text/html"), "{head}");
        assert!(body.contains("WFSearch"), "ui must carry the brand");
        assert!(body.contains("/api/v1/"), "ui must talk to the api");

        let (head, body) = raw(port, get("/", None)).await;
        assert!(head.contains("401"), "{head}");
        assert!(
            !body.contains(r#"id="q""#),
            "the search box must not reach an unauthenticated browser"
        );
        assert!(
            !body.contains(&st.http_token),
            "401 page must not leak the token"
        );
        assert!(
            body.contains("http.token"),
            "401 page must say where the token is"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_query_is_a_400_not_a_500() {
        let (st, port) = serve().await;
        let (_, body) = raw(port, get("/api/v1/search?q=%20", Some(&st.http_token))).await;
        assert!(body.contains("query must not be empty"), "{body}");
    }
}
