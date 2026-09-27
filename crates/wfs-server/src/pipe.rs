//! Named pipe front: `\\.\pipe\wfs-engine-v1`, 4-byte LE length prefix +
//! JSON, one connection per client (persistent, multiple sequential requests).

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use wfs_proto::{Request, Response, ERR_BAD_REQUEST, ERR_INTERNAL};

use crate::api;
use crate::state::AppState;

const FRAME_LIMIT: u32 = 4 << 20; // 4MB request cap

pub async fn spawn(state: Arc<AppState>) -> anyhow::Result<()> {
    let pipe_name = state.config.pipe_name.clone();
    let restricted = state.config.acl_restricted();
    let mut server = create_instance(&pipe_name, true, restricted)?;
    if restricted {
        tracing::info!("pipe: DACL restricted to SYSTEM + Administrators");
    }
    tracing::info!("pipe: listening on {}", pipe_name);
    tokio::spawn(async move {
        loop {
            // connect() wires THIS instance; the instance itself is the client
            match server.connect().await {
                Ok(()) => {
                    let st = state.clone();
                    let client = server;
                    tokio::spawn(async move {
                        if let Err(e) = serve_client(st, client).await {
                            tracing::debug!("pipe client ended: {e}");
                        }
                    });
                    // queue the next instance for new clients
                    match create_instance(&pipe_name, false, restricted) {
                        Ok(s) => server = s,
                        Err(e) => {
                            tracing::error!("pipe: cannot create next instance: {e}");
                            break;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("pipe connect: {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    });
    Ok(())
}

/// Create one pipe instance. `first` sets FILE_FLAG_FIRST_PIPE_INSTANCE so a
/// second engine cannot take over the name.
///
/// The DACL is always explicit, including the `open` case: a pipe created
/// without one inherits the *creating token's* default DACL, which serves the
/// interactive user in console mode but not when the engine runs as a
/// LocalSystem service. Passing `SDDL_OPEN` makes `acl = "open"` mean the same
/// thing in both. The handle is created with the descriptor, so no separate
/// `SetSecurityInfo` right is needed.
///
/// Synchronous on purpose: `SecurityDescriptor` holds a raw pointer and would
/// make any future that awaits across it non-`Send`.
fn create_instance(name: &str, first: bool, restricted: bool) -> std::io::Result<NamedPipeServer> {
    let mut opts = ServerOptions::new();
    opts.first_pipe_instance(first);
    let sddl = if restricted {
        wfs_fs::sec::SDDL_ADMINS_ONLY
    } else {
        wfs_fs::sec::SDDL_OPEN
    };
    let mut sec =
        wfs_fs::sec::SecurityDescriptor::from_sddl(sddl).map_err(std::io::Error::other)?;
    unsafe { opts.create_with_security_attributes_raw(name, sec.as_raw()) }
}

async fn serve_client(
    state: Arc<AppState>,
    mut pipe: NamedPipeServer,
) -> Result<(), std::io::Error> {
    loop {
        let mut lenb = [0u8; 4];
        pipe.read_exact(&mut lenb).await?;
        let len = u32::from_le_bytes(lenb);
        if len == 0 || len > FRAME_LIMIT {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("frame of {len} bytes out of range"),
            ));
        }
        let mut buf = vec![0u8; len as usize];
        pipe.read_exact(&mut buf).await?;

        // Dispatch off the reactor: a query is a full parallel scan and can
        // take tens of ms, which must not stall other connections.
        let st = state.clone();
        let resp = match serde_json::from_slice::<Request>(&buf) {
            Ok(req) => {
                match tokio::task::spawn_blocking(move || api::handle_request(&st, req)).await {
                    Ok(r) => r,
                    Err(e) => Response::err(ERR_INTERNAL, format!("request task failed: {e}")),
                }
            }
            Err(e) => Response::err(ERR_BAD_REQUEST, format!("bad request json: {e}")),
        };
        let body = serde_json::to_vec(&resp).unwrap_or_else(|_| {
            br#"{"type":"err","data":{"code":3,"message":"serialize failed"}}"#.to_vec()
        });

        pipe.write_all(&(body.len() as u32).to_le_bytes()).await?;
        pipe.write_all(&body).await?;
        pipe.flush().await?;
    }
}

/// End-to-end through a real pipe instance, using the published client
/// (`wfs-client`), which is the reference for the Python/C# samples.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;
    use wfs_proto::Request;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ping_search_and_status_through_the_pipe() {
        let st = testutil::state();
        let name = st.config.pipe_name.clone();
        assert_ne!(
            name,
            wfs_proto::DEFAULT_PIPE_NAME,
            "tests must not fight over the production pipe name"
        );
        spawn(st).await.unwrap();

        tokio::task::spawn_blocking(move || {
            let mut c = wfs_client::Client::connect(&name).expect("connect to pipe");

            assert!(matches!(
                c.call(&Request::Ping).unwrap(),
                wfs_proto::Response::Pong
            ));

            let sr = wfs_proto::SearchReq {
                q: "*.txt".into(),
                limit: 10,
                ..Default::default()
            };
            let resp = c.search(&sr).unwrap();
            assert_eq!(resp.total_matched, 1);
            assert_eq!(resp.results[0].path, r"C:\work\notes.txt");
            assert!(!resp.results[0].is_dir);

            // one connection, several sequential frames
            let status = c.status().unwrap();
            assert_eq!(status.volumes[0].drive, "C");
            assert_eq!(status.volumes[0].files, 4);
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn malformed_json_gets_a_bad_request_reply() {
        let st = testutil::state();
        let name = st.config.pipe_name.clone();
        spawn(st).await.unwrap();

        let reply = tokio::task::spawn_blocking(move || {
            use std::io::{Read, Write};
            let mut f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&name)
                .expect("connect");
            let garbage = b"{\"op\":\"nope\"}";
            f.write_all(&(garbage.len() as u32).to_le_bytes()).unwrap();
            f.write_all(garbage).unwrap();
            f.flush().unwrap();

            let mut lenb = [0u8; 4];
            f.read_exact(&mut lenb).unwrap();
            let mut body = vec![0u8; u32::from_le_bytes(lenb) as usize];
            f.read_exact(&mut body).unwrap();
            String::from_utf8(body).unwrap()
        })
        .await
        .unwrap();

        let v: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(v["type"], "err");
        assert_eq!(v["data"]["code"], ERR_BAD_REQUEST);
    }

    /// The regression this guards: `acl = "open"` used to inherit the creating
    /// token's default DACL, which served the interactive user in console mode
    /// but locked every client out of a LocalSystem service. Since the tests run
    /// as that same interactive user, inheritance was invisible to them — so
    /// the DACL itself is read back and asserted.
    #[tokio::test]
    async fn open_dacl_grants_interactive_users() {
        let name = testutil::unique_pipe();
        // tokio's ServerOptions registers with the reactor, hence the async test.
        let inst = create_instance(&name, true, false).expect("open pipe instance");
        let sddl = wfs_fs::sec::pipe_sddl(&name).expect("read the pipe DACL back");
        assert!(sddl.contains("(A;;"), "an allow ACE is expected: {sddl}");
        assert!(sddl.contains("IU"), "open must grant IU: {sddl}");
        drop(inst);
    }

    #[tokio::test]
    async fn restricted_dacl_withholds_interactive_users() {
        let name = testutil::unique_pipe();
        let inst = create_instance(&name, true, true).expect("restricted pipe instance");
        match wfs_fs::sec::pipe_sddl(&name) {
            Ok(sddl) => assert!(!sddl.contains("IU"), "restricted must not grant IU: {sddl}"),
            // The read-back is itself gated by the DACL: a UAC-filtered token has
            // `BA` marked deny-only, so denial here is the restriction working.
            // An elevated runner (CI) takes the `Ok` branch instead.
            Err(e) => assert!(
                e.to_string().ends_with("win32 error 5"),
                "only access denied is acceptable: {e}"
            ),
        }
        drop(inst);
    }
}
