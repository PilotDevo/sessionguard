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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            watch_roots: default_watch_roots(),
            watch_mode: WatchMode::default(),
            tools: Vec::new(),
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
        toml::from_str(&content).map_err(|e| Error::ConfigParse {
            path: path.to_owned(),
            source: e,
        })
    }

    /// Default config file path: `~/.config/sessionguard/config.toml`.
    pub fn default_path() -> PathBuf {
        config_dir().join("config.toml")
    }

    /// Default data directory: `~/.local/share/sessionguard/`.
    pub fn data_dir() -> PathBuf {
        ProjectDirs::from("dev", "droco", "sessionguard")
            .map(|d| d.data_dir().to_owned())
            .unwrap_or_else(|| PathBuf::from(".sessionguard"))
    }
}

/// SessionGuard config directory.
pub fn config_dir() -> PathBuf {
    ProjectDirs::from("dev", "droco", "sessionguard")
        .map(|d| d.config_dir().to_owned())
        .unwrap_or_else(|| PathBuf::from(".sessionguard"))
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
}
