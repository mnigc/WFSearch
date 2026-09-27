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
}
