//! wfs-server — WFSearch engine service.
//!
//! Modes: interactive console (default), Windows service entry (`run`),
//! `install` / `uninstall` for the service registration.

mod api;
mod app;
mod config;
mod http;
mod pipe;
mod service;
mod snapshot;
mod state;
#[cfg(test)]
mod testutil;
mod watcher;

use std::io::Write;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;
use wfs_proto::SearchReq;

use crate::config::Config;
use crate::state::AppState;

#[derive(Parser)]
#[command(
    name = "wfs-server",
    version,
    about = "WFSearch: fast NTFS filename search engine service"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// config file path (default: %ProgramData%\WFSearch\config.toml)
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<String>,
    #[arg(long, global = true, value_name = "PORT")]
    http_port: Option<u16>,
    /// comma-separated drive letters, e.g. C,D (overrides config)
    #[arg(long, global = true, value_name = "LIST")]
    drives: Option<String>,
}

#[derive(Subcommand, Clone, Copy, Default)]
enum Command {
    /// run interactively in this console (default; needs elevation)
    #[default]
    Console,
    /// run as the Windows service entry point (used by the SCM)
    Run,
    /// register the Windows service (elevated)
    Install,
    /// remove the Windows service (elevated)
    Uninstall,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let mut config = Config::load(cli.config.as_deref().map(std::path::Path::new));
    if let Some(port) = cli.http_port {
        config.http_port = port;
    }
    if let Some(d) = &cli.drives {
        config.drives = d
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }

    let command = cli.command.unwrap_or_default();
    init_logging(matches!(command, Command::Run), &config);

    match command {
        Command::Console => console(config),
        Command::Run => service::dispatch().map_err(|e| anyhow::anyhow!("service dispatch: {e}")),
        Command::Install => service::install(),
        Command::Uninstall => service::uninstall(),
    }
}

/// stderr in console mode; a service has no console to write to, so it appends
/// to `<data_dir>/wfs.log` — otherwise service-mode logs are lost entirely and
/// deploy.md's "check the SCM output" advice has nothing behind it.
fn init_logging(to_file: bool, config: &Config) {
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let ok = if to_file {
        match FileLog::open(&config.data_dir_path().join("wfs.log")) {
            Ok(writer) => tracing_subscriber::fmt()
                .with_env_filter(filter())
                .with_ansi(false)
                .with_writer(writer)
                .try_init()
                .is_ok(),
            Err(e) => {
                eprintln!("wfs-server: cannot open the service log ({e}); logging to stderr");
                tracing_subscriber::fmt()
                    .with_env_filter(filter())
                    .try_init()
                    .is_ok()
            }
        }
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter())
            .try_init()
            .is_ok()
    };
    let _ = ok;
}

/// Append-only log sink, truncated on startup once it grows past 16 MB (a
/// service cannot rotate files, and an unbounded log is a slow disk leak).
struct FileLog(std::sync::Mutex<std::fs::File>);

impl FileLog {
    const MAX_BYTES: u64 = 16 << 20;

    fn open(path: &std::path::Path) -> std::io::Result<FileLog> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let oversized = std::fs::metadata(path)
            .map(|m| m.len() > Self::MAX_BYTES)
            .unwrap_or(false);
        if oversized {
            let _ = std::fs::rename(path, path.with_extension("log.old"));
        }
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(FileLog(std::sync::Mutex::new(f)))
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for FileLog {
    type Writer = FileLogGuard<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        FileLogGuard(self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

struct FileLogGuard<'a>(std::sync::MutexGuard<'a, std::fs::File>);

impl Write for FileLogGuard<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

fn console(config: Config) -> anyhow::Result<()> {
    warn_if_not_elevated();
    let application = app::boot(config)?;
    let st = application.state.clone();
    println!(
        "WFSearch v{} — pipe: {}   http: http://127.0.0.1:{}",
        wfs_core::VERSION,
        st.config.pipe_name,
        st.config.http_port
    );
    println!("building indexes for fixed drives... (status endpoint shows progress)");
    println!("REPL: type a query, 'status', or 'quit'");

    // ctrl-c: snapshot and exit
    {
        let st = application.state.clone();
        application.rt.spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = snapshot::save(&st);
            std::process::exit(0);
        });
    }

    repl(&st);

    let _ = snapshot::save(&st);
    application.shutdown();
    println!("bye");
    Ok(())
}

fn repl(state: &Arc<AppState>) {
    let stdin = std::io::stdin();
    loop {
        print!("wfs> ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.read_line(&mut line).unwrap_or(0) == 0 {
            break; // EOF
        }
        let t = line.trim();
        match t {
            "" => continue,
            "quit" | "exit" => break,
            "status" => {
                let s = api::status_resp(state);
                if let Ok(j) = serde_json::to_string_pretty(&s) {
                    println!("{j}");
                }
            }
            q => {
                let req = SearchReq {
                    q: q.to_string(),
                    limit: 30,
                    ..Default::default()
                };
                match api::search_resp(state, req) {
                    Ok(r) => {
                        println!(
                            "{} match(es) in {} ms (showing {})",
                            r.total_matched,
                            r.query_ms,
                            r.results.len()
                        );
                        for f in &r.results {
                            let tag = if f.is_dir { " [DIR]" } else { "" };
                            println!("  {}{tag}", f.path);
                        }
                    }
                    Err((_, m)) => println!("error: {m}"),
                }
            }
        }
    }
}

fn warn_if_not_elevated() {
    if let Err(e) = wfs_fs::VolumeHandle::open('C') {
        eprintln!("warning: cannot open volume C: ({e})");
        eprintln!("warning: MFT indexing requires an elevated (Administrator) console.");
    }
}
