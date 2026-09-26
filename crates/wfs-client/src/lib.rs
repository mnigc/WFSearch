//! WFSearch client SDK — the reference implementation for embedding
//! applications. Two transports, identical payloads:
//!
//! * named pipe (default, lowest latency): 4-byte LE length prefix + JSON
//! * local HTTP: `GET /api/v1/search?q=...` on 127.0.0.1
//!
//! See `docs/clients.md` for Python / C# equivalents.

use std::io::{Read, Write};

use wfs_proto::{
    Request, Response, SearchReq, SearchResp, SortKind, StatusResp, DEFAULT_HTTP_PORT,
};

/// Error from either transport or a server-side error payload.
#[derive(Debug)]
pub struct ClientError(pub String);

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ClientError {}

type Result<T> = std::result::Result<T, ClientError>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(ClientError(msg.into()))
}

pub struct Client {
    stream: std::fs::File,
}

impl Client {
    /// Open a persistent pipe connection; reuse it for many queries.
    pub fn connect(pipe_name: &str) -> std::io::Result<Client> {
        let stream = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(pipe_name)?;
        Ok(Client { stream })
    }

    pub fn connect_default() -> std::io::Result<Client> {
        Client::connect(DEFAULT_PIPE_NAME)
    }

    pub fn call(&mut self, req: &Request) -> Result<Response> {
        let body = serde_json::to_vec(req).map_err(|e| ClientError(e.to_string()))?;
        self.stream
            .write_all(&(body.len() as u32).to_le_bytes())
            .map_err(io)?;
        self.stream.write_all(&body).map_err(io)?;
        self.stream.flush().map_err(io)?;

        let mut lenb = [0u8; 4];
        self.stream.read_exact(&mut lenb).map_err(io)?;
        let len = u32::from_le_bytes(lenb) as usize;
        if len > 256 << 20 {
            return err("oversized response frame");
        }
        let mut buf = vec![0u8; len];
        self.stream.read_exact(&mut buf).map_err(io)?;
        match serde_json::from_slice(&buf) {
            Ok(r) => Ok(r),
            Err(e) => err(e.to_string()),
        }
    }

    pub fn search(&mut self, req: &SearchReq) -> Result<SearchResp> {
        match self.call(&Request::Search(req.clone()))? {
            Response::Search(r) => Ok(r),
            Response::Err(e) => err(format!("[{}] {}", e.code, e.message)),
            _ => err("unexpected response kind"),
        }
    }

    pub fn status(&mut self) -> Result<StatusResp> {
        match self.call(&Request::Status)? {
            Response::Status(r) => Ok(r),
            Response::Err(e) => err(format!("[{}] {}", e.code, e.message)),
            _ => err("unexpected response kind"),
        }
    }
}

fn io(e: std::io::Error) -> ClientError {
    ClientError(e.to_string())
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn http_get(port: u16, path: &str) -> Result<Vec<u8>> {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).map_err(io)?;
    // HTTP/1.0: no chunked encoding, body ends with the connection
    write!(s, "GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n").map_err(io)?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).map_err(io)?;
    let pos = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| ClientError("malformed HTTP response".into()))?
        + 4;
    Ok(raw[pos..].to_vec())
}

pub fn search_http(req: &SearchReq, port: u16) -> Result<SearchResp> {
    let sort = match req.sort {
        SortKind::None => "none",
        SortKind::Name => "name",
        SortKind::Path => "path",
    };
    let path = format!(
        "/api/v1/search?q={}&limit={}&offset={}&sort={sort}{}",
        urlencode(&req.q),
        req.limit,
        req.offset,
        if req.match_path {
            "&match_path=true"
        } else {
            ""
        }
    );
    let body = http_get(port, &path)?;
    match serde_json::from_slice(&body) {
        Ok(r) => Ok(r),
        Err(e) => err(e.to_string()),
    }
}

pub fn status_http(port: u16) -> Result<StatusResp> {
    let body = http_get(port, "/api/v1/status")?;
    match serde_json::from_slice(&body) {
        Ok(r) => Ok(r),
        Err(e) => err(e.to_string()),
    }
}

pub const DEFAULT_PORT: u16 = DEFAULT_HTTP_PORT;

pub use wfs_proto::DEFAULT_PIPE_NAME;

#[cfg(test)]
mod tests {
    use super::urlencode;

    #[test]
    fn urlencoding() {
        assert_eq!(urlencode("abc-1.txt"), "abc-1.txt");
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("中文"), "%E4%B8%AD%E6%96%87");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
    }
}
