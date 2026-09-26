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
    /// pipe DACL: `open` (any local user, default) or `restricted` (SYSTEM +
    /// Administrators only)
    pub pipe_acl: String,
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
            pipe_acl: "open".into(),
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
        match std::fs::read_to_string(&p) {
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
        }
    }

    pub fn data_dir_path(&self) -> PathBuf {
        match &self.data_dir {
            Some(d) => PathBuf::from(d),
            None => default_data_dir(),
        }
    }

    /// Unknown values fall back to `open` rather than failing the whole config:
    /// a typo should not silently downgrade a locked-down deployment, so it is
    /// reported loudly on stderr.
    pub fn pipe_acl_restricted(&self) -> bool {
        match self.pipe_acl.trim().to_ascii_lowercase().as_str() {
            "restricted" => true,
            "open" => false,
            other => {
                eprintln!(
                    "wfs-server: unknown pipe_acl '{other}' (expected open|restricted); using open"
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
