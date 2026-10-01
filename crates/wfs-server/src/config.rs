use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use wfs_proto::{DEFAULT_HTTP_PORT, DEFAULT_PIPE_NAME};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// "auto" or drive letters like ["C", "D"]
    pub drives: Vec<String>,
    pub http_port: u16,
    pub pipe_name: String,
    /// overrides %ProgramData%\WFSearch when set
    pub data_dir: Option<String>,
    /// USN journal poll interval (ms)
    pub poll_ms: u64,
    /// hard cap for a single search's limit
    pub max_limit: u32,
    /// Who may reach the engine, on **both** front-ends: `open` (any locally
    /// signed-in user, default) or `restricted` (SYSTEM + Administrators only).
    /// The `pipe_acl` key name is accepted as an alias for releases before the
    /// HTTP gateway shared the setting.
    #[serde(alias = "pipe_acl")]
    pub acl: String,
    /// document-content search (`content:` terms) — see `ContentConfig`
    pub content: ContentConfig,
}

/// Query-time document-content scan budgets. Nothing here is persisted state:
/// the scan reads candidate files per request, so these values only bound
/// latency and IO, and every field can be tuned without touching the index.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContentConfig {
    /// `content:` queries return an error when false
    pub enabled: bool,
    /// candidate window: at most this many name-matched files are scanned;
    /// beyond it `truncated` is reported instead of scanning unbounded
    pub max_candidates: u32,
    /// files larger than this are skipped (`skipped_size`)
    pub max_file_bytes: u64,
    /// wall-clock budget for one scan; `timed_out` reports partial coverage
    pub timeout_ms: u64,
    /// concurrent file readers (kept small so scans never starve the rayon
    /// pool the name search shares)
    pub max_concurrency: u32,
}

impl Default for ContentConfig {
    fn default() -> Self {
        ContentConfig {
            enabled: true,
            max_candidates: 2_000,
            max_file_bytes: 8 << 20,
            timeout_ms: 10_000,
            max_concurrency: 4,
        }
    }
}

impl ContentConfig {
    /// Clamp out-of-range values (a hand-edited TOML must not produce a
    /// zero-budget or unbounded scan); the loud-warning style of
    /// `normalize_acl` is not needed for plain numeric ranges.
    pub fn normalized(mut self) -> ContentConfig {
        self.max_candidates = self.max_candidates.clamp(1, 1_000_000);
        self.max_file_bytes = self.max_file_bytes.clamp(1, 1 << 30);
        self.timeout_ms = self.timeout_ms.clamp(100, 600_000);
        self.max_concurrency = self.max_concurrency.clamp(1, 16);
        self
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            drives: vec!["auto".into()],
            http_port: DEFAULT_HTTP_PORT,
            pipe_name: DEFAULT_PIPE_NAME.into(),
            data_dir: None,
            poll_ms: 100,
            max_limit: 1000,
            acl: "open".into(),
            content: ContentConfig::default(),
        }
    }
}

impl Config {
    /// Missing file or bad TOML falls back to defaults (with a warning).
    pub fn load(path: Option<&Path>) -> Config {
        let p = match path {
            Some(p) => p.to_path_buf(),
            None => default_data_dir().join("config.toml"),
        };
        let mut cfg = match std::fs::read_to_string(&p) {
            Ok(s) => match toml::from_str(&s) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!(
                        "wfs-server: bad config {}: {e}; using defaults",
                        p.display()
                    );
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        };
        cfg.normalize_acl();
        cfg.content = cfg.content.clone().normalized();
        cfg
    }

    pub fn data_dir_path(&self) -> PathBuf {
        match &self.data_dir {
            Some(d) => PathBuf::from(d),
            None => default_data_dir(),
        }
    }

    /// Where the HTTP bearer token lives. Its DACL is the access policy for the
    /// HTTP gateway, so whoever can read this file can query the engine.
    pub fn token_path(&self) -> PathBuf {
        self.data_dir_path().join("http.token")
    }

    /// Unknown values fall back to `open` rather than failing the whole config:
    /// a typo should not silently downgrade a locked-down deployment, so it is
    /// reported loudly on stderr. Normalized once at load so the warning cannot
    /// repeat for each consumer of the setting.
    fn normalize_acl(&mut self) {
        self.acl = if self.acl_restricted() {
            "restricted".into()
        } else {
            "open".into()
        };
    }

    pub fn acl_restricted(&self) -> bool {
        match self.acl.trim().to_ascii_lowercase().as_str() {
            "restricted" => true,
            "open" => false,
            other => {
                eprintln!(
                    "wfs-server: unknown acl '{other}' (expected open|restricted); using open"
                );
                false
            }
        }
    }
}

pub fn default_data_dir() -> PathBuf {
    std::env::var("ProgramData")
        .map(|p| PathBuf::from(p).join("WFSearch"))
        .unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded(name: &str, toml_body: &str) -> Config {
        let dir = std::env::temp_dir().join(format!("wfs-cfg-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, toml_body).unwrap();
        Config::load(Some(&path))
    }

    /// Deploys written against the old key must keep locking the same way: a
    /// silently ignored `restricted` would leave the engine wide open.
    #[test]
    fn pipe_acl_is_still_accepted_as_the_old_name() {
        assert!(loaded("old", "pipe_acl = \"restricted\"\n").acl_restricted());
        assert!(!loaded("new", "acl = \"open\"\n").acl_restricted());
    }

    #[test]
    fn an_unknown_acl_falls_back_to_open() {
        assert!(!loaded("typo", "acl = \"loose\"\n").acl_restricted());
    }

    #[test]
    fn token_path_follows_the_data_dir() {
        let mut c = Config::default();
        assert_eq!(
            c.token_path(),
            c.data_dir_path().join("http.token"),
            "default data dir"
        );
        c.data_dir = Some("D:\\wfsdata".into());
        assert_eq!(c.token_path(), PathBuf::from("D:\\wfsdata\\http.token"));
    }

    #[test]
    fn content_section_defaults_when_absent() {
        let c = loaded("noc", "drives = [\"C\"]\n");
        assert!(c.content.enabled);
        assert_eq!(c.content.max_candidates, 2_000);
        assert_eq!(c.content.max_file_bytes, 8 << 20);
        assert_eq!(c.content.timeout_ms, 10_000);
        assert_eq!(c.content.max_concurrency, 4);
    }

    #[test]
    fn content_section_parses_and_normalizes() {
        let c = loaded(
            "content",
            "[content]\nenabled = false\nmax_candidates = 0\nmax_concurrency = 99\ntimeout_ms = 1\n",
        );
        assert!(!c.content.enabled);
        // out-of-range values are clamped into sane bounds
        assert_eq!(c.content.max_candidates, 1);
        assert_eq!(c.content.max_concurrency, 16);
        assert_eq!(c.content.timeout_ms, 100);
    }
}
