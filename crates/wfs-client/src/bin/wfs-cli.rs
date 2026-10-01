//! wfs-cli — command-line debug client for the WFSearch engine.

use anyhow::bail;
use clap::{Parser, Subcommand};
use wfs_client::{Client, DEFAULT_PIPE_NAME, DEFAULT_PORT};
use wfs_proto::{SearchReq, SortKind};

#[derive(Parser)]
#[command(name = "wfs-cli", version, about = "Query the WFSearch engine")]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
    /// named pipe endpoint
    #[arg(long, global = true, default_value = DEFAULT_PIPE_NAME)]
    pipe: String,
    /// HTTP port (used with --http)
    #[arg(long, global = true, default_value_t = DEFAULT_PORT)]
    port: u16,
    /// use the HTTP transport instead of the named pipe
    #[arg(long, global = true)]
    http: bool,
    /// HTTP bearer token (default: read <ProgramData>\WFSearch\http.token)
    #[arg(long, global = true)]
    token: Option<String>,
}

/// The token is only needed by the HTTP transport; the pipe is authenticated by
/// its DACL, so there is nothing to present there.
fn http_token(cli: &Cli) -> anyhow::Result<String> {
    match &cli.token {
        Some(t) => Ok(t.clone()),
        None => wfs_client::load_token().map_err(|e| anyhow::anyhow!("{e}")),
    }
}

#[derive(Subcommand)]
enum Command {
    /// search files by name (terms AND'ed; supports * and ? wildcards; "c:" filters a drive;
    /// terms containing \ or / match the full path; "content:term" scans document contents)
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long, default_value_t = 0)]
        offset: u32,
        /// none | name | path
        #[arg(long, default_value = "none")]
        sort: String,
        /// match every term against the full path instead of the file name
        #[arg(long)]
        match_path: bool,
    },
    /// show engine status
    Status,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let sort = cli_sort(&cli)?;
    match &cli.cmd {
        Command::Search {
            query,
            limit,
            offset,
            match_path,
            ..
        } => {
            let req = SearchReq {
                q: query.clone(),
                limit: *limit,
                offset: *offset,
                sort,
                match_path: *match_path,
            };
            let resp = if cli.http {
                wfs_client::search_http(&req, cli.port, &http_token(&cli)?)?
            } else {
                Client::connect(&cli.pipe)
                    .map_err(|e| anyhow::anyhow!("connect {}: {e}", cli.pipe))?
                    .search(&req)?
            };
            println!(
                "{} match(es), {} ms server-side (showing {})",
                resp.total_matched,
                resp.query_ms,
                resp.results.len()
            );
            if let Some(c) = &resp.content {
                if c.truncated {
                    println!(
                        "  note: candidate window exhausted — narrow the name terms for full coverage"
                    );
                }
                if c.timed_out {
                    println!("  note: scan budget ran out — results are partial");
                }
            }
            for f in &resp.results {
                let tag = if f.is_dir { "  [DIR]" } else { "" };
                println!("  {}{tag}", f.path);
                if let Some(sn) = &f.snippet {
                    println!("      {sn}  ({} in file)", f.content_matches.unwrap_or(0));
                }
            }
        }
        Command::Status => {
            let resp = if cli.http {
                wfs_client::status_http(cli.port, &http_token(&cli)?)?
            } else {
                Client::connect(&cli.pipe)
                    .map_err(|e| anyhow::anyhow!("connect {}: {e}", cli.pipe))?
                    .status()?
            };
            println!("{}", serde_json::to_string_pretty(&resp)?);
        }
    }
    Ok(())
}

fn cli_sort(cli: &Cli) -> anyhow::Result<SortKind> {
    let Some(Command::Search { sort, .. }) = Some(&cli.cmd) else {
        return Ok(SortKind::None);
    };
    match sort.as_str() {
        "none" => Ok(SortKind::None),
        "name" => Ok(SortKind::Name),
        "path" => Ok(SortKind::Path),
        other => bail!("unknown sort '{other}' (none|name|path)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The README/deploy docs spell the transport flags after the subcommand
    /// (`wfs-cli search "*.md" --http`), which only parses if they are global.
    #[test]
    fn transport_flags_parse_after_subcommand() {
        let cli = Cli::try_parse_from(["wfs-cli", "search", "*.md", "--http"]).unwrap();
        assert!(cli.http);

        let cli = Cli::try_parse_from(["wfs-cli", "status", "--http", "--port", "15200"]).unwrap();
        assert!(cli.http);
        assert_eq!(cli.port, 15200);

        let cli = Cli::try_parse_from(["wfs-cli", "--pipe", r"\\.\pipe\other", "status"]).unwrap();
        assert_eq!(cli.pipe, r"\\.\pipe\other");
    }

    #[test]
    fn sort_flag_is_validated() {
        assert!(cli_sort(
            &Cli::try_parse_from(["wfs-cli", "search", "x", "--sort", "name"]).unwrap()
        )
        .is_ok());
        assert!(cli_sort(
            &Cli::try_parse_from(["wfs-cli", "search", "x", "--sort", "bogus"]).unwrap()
        )
        .is_err());
    }
}
