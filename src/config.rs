// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Configuration loading and management.
//!
//! Config file: `~/.config/sessionguard/config.toml`
//! Falls back to sensible defaults when no config file exists.

use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::tools::ToolDefinition;

/// Watch aggressiveness mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WatchMode {
    /// Maximum responsiveness, higher resource usage.
    Aggressive,
    /// Default. Good balance of responsiveness and resource usage.
    #[default]
    Balanced,
    /// Minimal resource usage, may miss rapid successive events.
    Passive,
}

/// A machine in the fleet that can be censused over ssh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostSpec {
    /// Short name used with `--host`. `local` is reserved for this machine.
    pub name: String,
    /// ssh destination, e.g. `devo@192.0.2.10`.
    pub ssh: String,
    /// Path of the `sessionguard` binary on that host. Defaults to
    /// `sessionguard`, which the remote NON-interactive shell resolves through
    /// its own PATH — one that commonly lacks Homebrew's or cargo's bin dir,
    /// so the remote reports `command not found` although the host was
    /// reached fine. Set an absolute path (e.g.
    /// `/opt/homebrew/bin/sessionguard`, `~/.cargo/bin/sessionguard`) then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
}

impl HostSpec {
    /// Name reserved for this machine's own census; a configured host may
    /// not claim it, or its rows would be indistinguishable from local ones.
    pub const LOCAL: &'static str = "local";

    /// The remote binary to run (see the `binary` field).
    pub fn binary(&self) -> &str {
        self.binary.as_deref().unwrap_or("sessionguard")
    }
}

/// Top-level SessionGuard configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Directories to watch for project moves.
    #[serde(default = "default_watch_roots")]
    pub watch_roots: Vec<PathBuf>,

    /// Watch aggressiveness mode.
    #[serde(default)]
    pub watch_mode: WatchMode,

    /// Additional tool definitions from the project config.
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,

    /// Fleet hosts this machine can census over ssh (`--host`/`--all-hosts`).
    #[serde(default)]
    pub hosts: Vec<HostSpec>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            watch_roots: default_watch_roots(),
            watch_mode: WatchMode::default(),
            tools: Vec::new(),
            hosts: Vec::new(),
        }
    }
}

fn default_watch_roots() -> Vec<PathBuf> {
    let home = dirs_home().unwrap_or_default();
    ["projects", "repos", "code", "dev"]
        .iter()
        .map(|d| home.join(d))
        .filter(|p| p.is_dir())
        .collect()
}

fn dirs_home() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().to_owned())
}

impl Config {
    /// Load config from the standard location, falling back to defaults.
    pub fn load() -> Result<Self> {
        let path = Self::default_path();
        if path.is_file() {
            Self::load_from(&path)
        } else {
            Ok(Self::default())
        }
    }

    /// Load config from a specific file path.
    pub fn load_from(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&content).map_err(|e| Error::ConfigParse {
            path: path.to_owned(),
            source: e,
        })?;
        config
            .validate()
            .map_err(|msg| Error::Config(format!("{}: {msg}", path.display())))?;
        Ok(config)
    }

    /// Reject a config that parses but cannot mean what it says: a `[[hosts]]`
    /// entry with no name or destination, two hosts sharing a name (`--host`
    /// could only ever reach one), or a host named `local` (its rows would be
    /// indistinguishable from this machine's own).
    pub fn validate(&self) -> std::result::Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for h in &self.hosts {
            if h.name.trim().is_empty() {
                return Err("[[hosts]] entry has an empty `name`".into());
            }
            if h.name == HostSpec::LOCAL {
                return Err(format!(
                    "[[hosts]] name `{}` is reserved for this machine",
                    HostSpec::LOCAL
                ));
            }
            if h.ssh.trim().is_empty() {
                return Err(format!(
                    "[[hosts]] `{}` has an empty `ssh` destination",
                    h.name
                ));
            }
            if !seen.insert(h.name.as_str()) {
                return Err(format!("[[hosts]] name `{}` is declared twice", h.name));
            }
        }
        Ok(())
    }

    /// Default config file path: `~/.config/sessionguard/config.toml`.
    pub fn default_path() -> PathBuf {
        config_dir().join("config.toml")
    }

    /// Default data directory: `~/.local/share/sessionguard/`.
    ///
    /// Overridable via `SESSIONGUARD_DATA_DIR` environment variable (used in tests).
    pub fn data_dir() -> PathBuf {
        if let Ok(dir) = std::env::var("SESSIONGUARD_DATA_DIR") {
            return PathBuf::from(dir);
        }
        ProjectDirs::from("dev", "droco", "sessionguard")
            .map(|d| d.data_dir().to_owned())
            .unwrap_or_else(fallback_state_dir)
    }
}

/// Stable, ABSOLUTE fallback when the platform dirs can't be resolved (HOME
/// unset — containers, hardened systemd units). A cwd-relative fallback here
/// meant the PID file and registry changed with the working directory: two
/// daemons could start, and `stop`/`status` from another cwd saw nothing.
fn fallback_state_dir() -> PathBuf {
    #[cfg(unix)]
    let uid = unsafe { libc::getuid() };
    #[cfg(not(unix))]
    let uid = 0;
    std::env::temp_dir().join(format!("sessionguard-{uid}"))
}

/// SessionGuard config directory.
///
/// Overridable via the `SESSIONGUARD_CONFIG_DIR` environment variable, mirroring
/// [`Config::data_dir`]'s `SESSIONGUARD_DATA_DIR` knob. Tests and the dogfood
/// scripts set it so `config show` / `inventory` never read the operator's real
/// `~/.config/sessionguard`.
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SESSIONGUARD_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    ProjectDirs::from("dev", "droco", "sessionguard")
        .map(|d| d.config_dir().to_owned())
        .unwrap_or_else(fallback_state_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        let config = Config::default();
        assert_eq!(config.watch_mode, WatchMode::Balanced);
        assert!(config.tools.is_empty());
    }

    #[test]
    fn config_round_trips_toml() {
        let config = Config::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let _parsed: Config = toml::from_str(&toml_str).unwrap();
    }

    #[test]
    fn config_parses_hosts() {
        let c: Config = toml::from_str(
            r#"
            watch_roots = []
            [[hosts]]
            name = "fedora"
            ssh = "devo@192.0.2.10"
            "#,
        )
        .unwrap();
        assert_eq!(c.hosts.len(), 1);
        assert_eq!(c.hosts[0].name, "fedora");
        assert_eq!(c.hosts[0].binary(), "sessionguard", "default remote binary");
        assert!(c.validate().is_ok());
    }

    #[test]
    fn config_parses_per_host_binary() {
        let c: Config = toml::from_str(
            r#"
            watch_roots = []
            [[hosts]]
            name = "mac"
            ssh = "devo@macbook"
            binary = "/opt/homebrew/bin/sessionguard"
            "#,
        )
        .unwrap();
        assert_eq!(c.hosts[0].binary(), "/opt/homebrew/bin/sessionguard");
    }

    #[test]
    fn config_validation_rejects_reserved_duplicate_and_empty_host_names() {
        let parse = |hosts: &str| -> Config {
            toml::from_str(&format!("watch_roots = []\n{hosts}")).unwrap()
        };
        let reserved = parse("[[hosts]]\nname = \"local\"\nssh = \"x@y\"\n");
        assert!(reserved.validate().unwrap_err().contains("reserved"));

        let dup = parse(
            "[[hosts]]\nname = \"a\"\nssh = \"x@y\"\n[[hosts]]\nname = \"a\"\nssh = \"x@z\"\n",
        );
        assert!(dup.validate().unwrap_err().contains("twice"));

        let empty = parse("[[hosts]]\nname = \"\"\nssh = \"x@y\"\n");
        assert!(empty.validate().unwrap_err().contains("empty `name`"));

        let no_ssh = parse("[[hosts]]\nname = \"a\"\nssh = \"\"\n");
        assert!(no_ssh.validate().unwrap_err().contains("ssh"));

        // And load_from surfaces it as a config error naming the file.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[[hosts]]\nname = \"local\"\nssh = \"x@y\"\n").unwrap();
        let err = Config::load_from(&path).unwrap_err().to_string();
        assert!(
            err.contains("config.toml") && err.contains("reserved"),
            "{err}"
        );
    }
}
