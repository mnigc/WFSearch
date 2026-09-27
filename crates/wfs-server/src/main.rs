//! wfs-server — WFSearch engine service.
//!
//! Modes: interactive console (default) and the Windows service entry
//! (`run`, driven by the SCM). Registering the service is the deployer's job —
//! see docs/deploy.md for the `sc create` form.

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

#[derive(Subcommand, Clone, Default)]
enum Command {
    /// run interactively in this console (default; needs elevation)
    #[default]
    Console,
    /// run as the Windows service entry point (used by the SCM)
    Run,
    /// probe volume access step by step and print every ioctl's result —
    /// run this when a volume is reported as failed
    Doctor {
        /// drive letters to probe (default: the configured drives)
        drives: Vec<String>,
    },
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
        Command::Doctor { drives } => doctor(&config, drives),
    }
}

/// Walk the volume access path of `run_volume` one ioctl at a time and print
/// each result. Indexing failures otherwise only surface as a generic
/// "volume marked failed" line in the log.
fn doctor(config: &Config, drives: Vec<String>) -> anyhow::Result<()> {
    let list: Vec<char> = if drives.is_empty() {
        crate::app::resolve_drives(config)
    } else {
        let mut c = config.clone();
        c.drives = drives;
        crate::app::resolve_drives(&c)
    };
    if list.is_empty() {
        println!("no drive to probe (pass one: wfs-server doctor C)");
        return Ok(());
    }
    let mut failed = false;
    for d in list {
        println!("volume {d}:");
        let vol = match wfs_fs::VolumeHandle::open(d) {
            Ok(v) => v,
            Err(e) => {
                println!("  open \\\\.\\{d}:            FAILED - {e}");
                println!("  (MFT and USN access need an elevated process: LocalSystem or an Administrator console)");
                failed = true;
                continue;
            }
        };
        println!("  open \\\\.\\{d}:            ok");
        match vol.journal_info() {
            Ok(j) => println!(
                "  FSCTL_QUERY_USN_JOURNAL: ok - id {:#x}, next_usn {}, lowest_valid {}, max {} MB, record versions {:?}",
                j.journal_id,
                j.next_usn,
                j.lowest_valid_usn,
                j.maximum_size >> 20,
                j.record_versions
            ),
            Err(e) => {
                println!("  FSCTL_QUERY_USN_JOURNAL: FAILED - {e}");
                failed = true;
            }
        }
        match vol.ensure_journal() {
            Ok(j) => println!("  ensure USN journal      : ok - id {:#x}", j.journal_id),
            Err(e) => {
                println!("  ensure USN journal      : FAILED - {e}");
                failed = true;
            }
        }
        let mut records = 0u64;
        let mut first = String::new();
        match vol.enumerate_mft(|e| {
            if records == 0 {
                first = e.name;
            }
            records += 1;
        }) {
            Ok(()) => println!(
                "  FSCTL_ENUM_USN_DATA     : ok - {records} records ({}-byte input, first entry {:?})",
                vol.enum_input_len(),
                first
            ),
            Err(e) => {
                println!("  FSCTL_ENUM_USN_DATA     : FAILED - {e}");
                failed = true;
            }
        }
        if let Ok(j) = vol.journal_info() {
            let pos = wfs_core::JournalPos {
                journal_id: j.journal_id,
                next_usn: j.next_usn as u64,
            };
            match vol.read_journal(pos, 1000) {
                Ok((scan, next)) => println!(
                    "  FSCTL_READ_USN_JOURNAL  : ok - {} pending record(s), {} event(s), \
                     {} unparsable, next_usn {}",
                    scan.records,
                    scan.events.len(),
                    scan.other_versions,
                    next.next_usn
                ),
                Err(e) => {
                    println!("  FSCTL_READ_USN_JOURNAL  : FAILED - {e}");
                    failed = true;
                }
            }
            // Replay from the oldest record the journal still holds. A watch
            // loop that sees no events is only diagnosable with the raw record
            // count next to the event count: "0 == 0" is an idle journal, while
            // "records > 0, events == 0" means the records are not V2. Purely
            // informative — it never affects the exit code.
            let oldest = wfs_core::JournalPos {
                journal_id: j.journal_id,
                next_usn: j.first_usn.max(j.lowest_valid_usn).max(0) as u64,
            };
            match vol.read_journal(oldest, 200_000) {
                Ok((scan, next)) => println!(
                    "  journal history         : usn {}..{} - {} record(s), {} event(s), {} \
                     unparsable [{}]",
                    oldest.next_usn,
                    next.next_usn,
                    scan.records,
                    scan.events.len(),
                    scan.other_versions,
                    event_breakdown(&scan.events)
                ),
                Err(e) => println!("  journal history         : (not replayable - {e})"),
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

/// `create` / `delete` / `rename` counts of a journal replay, so an idle
/// journal can be told apart from a volume whose changes never arrive.
fn event_breakdown(events: &[wfs_core::IndexEvent]) -> String {
    let (mut creates, mut deletes, mut renames) = (0u64, 0u64, 0u64);
    for e in events {
        match e {
            wfs_core::IndexEvent::Create { .. } => creates += 1,
            wfs_core::IndexEvent::Delete { .. } => deletes += 1,
            wfs_core::IndexEvent::RenameOld { .. } | wfs_core::IndexEvent::RenameNew { .. } => {
                renames += 1;
            }
        }
    }
    format!("{creates} create, {deletes} delete, {renames} rename")
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
