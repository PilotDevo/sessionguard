// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Runtime-loaded AI tool session pattern definitions.
//!
//! Tool definitions describe the session artifacts for each AI coding tool.
//! They are loaded from TOML files at runtime, with built-in defaults compiled
//! into the binary via `include_str!`.
//!
//! Loading order (later overrides earlier by tool name):
//! 1. Built-in patterns (always available)
//! 2. System patterns (`/etc/sessionguard/tools/*.toml`)
//! 3. User patterns (`~/.config/sessionguard/tools/*.toml`)
//! 4. Project patterns (`sessionguard.toml` `[[tools]]` section)

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// How SessionGuard should handle moves for a given tool's artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileStrategy {
    /// Rewrite path references in session files.
    #[default]
    RewritePaths,
    /// Only notify the user; don't modify anything.
    Notify,
    /// Run a custom command.
    Custom(String),
}

/// Specifies a path field inside a session file that needs rewriting on move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathFieldSpec {
    /// Relative path to the file within the session artifact directory.
    pub file: String,
    /// Dot-separated field path within the file (e.g., `project_root` or `cache.dir`).
    pub field: String,
    /// Format of the file (`json`, `toml`, `sqlite`, `text`).
    #[serde(default = "default_format")]
    pub format: String,
}

fn default_format() -> String {
    "text".to_string()
}

/// A tool definition describing one AI coding tool's session artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Unique tool identifier (e.g., `claude_code`, `cursor`).
    pub name: String,
    /// Human-readable display name.
    #[serde(default)]
    pub display_name: String,
    /// Glob patterns for session artifact files/directories.
    pub session_patterns: Vec<String>,
    /// Path fields inside session artifacts that reference the project root.
    #[serde(default)]
    pub path_fields: Vec<PathFieldSpec>,
    /// Strategy for handling moves.
    #[serde(default)]
    pub on_move: ReconcileStrategy,
    /// Version of this tool definition.
    #[serde(default)]
    pub version: Option<String>,
}

/// TOML wrapper for a file containing a single tool definition.
#[derive(Debug, Deserialize)]
struct ToolFile {
    tool: ToolDefinition,
}

/// Registry of all loaded tool definitions, keyed by tool name.
#[derive(Debug, Default)]
pub struct ToolRegistry {
    tools: HashMap<String, ToolDefinition>,
}

// Built-in tool pattern TOML files, compiled into the binary.
const BUILTIN_CLAUDE_CODE: &str = include_str!("builtin/claude_code.toml");
const BUILTIN_CURSOR: &str = include_str!("builtin/cursor.toml");
const BUILTIN_WINDSURF: &str = include_str!("builtin/windsurf.toml");
const BUILTIN_AIDER: &str = include_str!("builtin/aider.toml");
const BUILTIN_GEMINI_CLI: &str = include_str!("builtin/gemini_cli.toml");

impl ToolRegistry {
    /// Create a new registry loaded with built-in defaults only.
    ///
    /// For production use, prefer [`new_with_config`] which loads the full
    /// chain: built-in → system → user → project config.
    pub fn new() -> Result<Self> {
        let mut registry = Self::default();
        registry.load_builtin()?;
        Ok(registry)
    }

    /// Create a registry with the full loading chain:
    /// 1. Built-in patterns (compiled in)
    /// 2. System patterns (`/etc/sessionguard/tools/*.toml`)
    /// 3. User patterns (`~/.config/sessionguard/tools/*.toml`)
    /// 4. Project patterns (from `Config.tools`)
    pub fn new_with_config(config: &crate::config::Config) -> Result<Self> {
        let mut registry = Self::default();
        let config_dir = crate::config::config_dir();
        registry.load_all(&config_dir)?;
        for tool in &config.tools {
            registry.register(tool.clone());
        }
        Ok(registry)
    }

    /// Load all tool definitions from the standard locations.
    pub fn load_all(&mut self, user_config_dir: &Path) -> Result<()> {
        self.load_builtin()?;
        self.load_from_directory(&PathBuf::from("/etc/sessionguard/tools"))?;
        self.load_from_directory(&user_config_dir.join("tools"))?;
        Ok(())
    }

    /// Get a tool definition by name.
    pub fn get(&self, name: &str) -> Option<&ToolDefinition> {
        self.tools.get(name)
    }

    /// Get all registered tool definitions.
    pub fn all(&self) -> impl Iterator<Item = &ToolDefinition> {
        self.tools.values()
    }

    /// Register a tool definition, overriding any existing one with the same name.
    pub fn register(&mut self, tool: ToolDefinition) {
        self.tools.insert(tool.name.clone(), tool);
    }

    /// Load built-in tool definitions compiled into the binary.
    fn load_builtin(&mut self) -> Result<()> {
        for toml_str in [
            BUILTIN_CLAUDE_CODE,
            BUILTIN_CURSOR,
            BUILTIN_WINDSURF,
            BUILTIN_AIDER,
            BUILTIN_GEMINI_CLI,
        ] {
            let tool_file: ToolFile = toml::from_str(toml_str)
                .map_err(|e| Error::ToolDefinition(format!("invalid built-in tool TOML: {e}")))?;
            self.register(tool_file.tool);
        }
        Ok(())
    }

    /// Load tool definitions from a directory of TOML files.
    fn load_from_directory(&mut self, dir: &Path) -> Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }
        let entries = std::fs::read_dir(dir)?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "toml") {
                self.load_from_file(&path)?;
            }
        }
        Ok(())
    }

    /// Load a single tool definition from a TOML file.
    fn load_from_file(&mut self, path: &Path) -> Result<()> {
        let content = std::fs::read_to_string(path)?;
        let tool_file: ToolFile = toml::from_str(&content).map_err(|e| Error::ConfigParse {
            path: path.to_owned(),
            source: e,
        })?;
        self.register(tool_file.tool);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn builtin_tools_load() {
        let registry = ToolRegistry::new().unwrap();
        assert!(registry.get("claude_code").is_some());
        assert!(registry.get("cursor").is_some());
        assert!(registry.get("windsurf").is_some());
        assert!(registry.get("aider").is_some());
        assert!(registry.get("gemini_cli").is_some());
    }

    #[test]
    fn tool_registry_override() {
        let mut registry = ToolRegistry::new().unwrap();
        let custom = ToolDefinition {
            name: "claude_code".to_string(),
            display_name: "Custom Claude".to_string(),
            session_patterns: vec![".custom/".to_string()],
            path_fields: vec![],
            on_move: ReconcileStrategy::Notify,
            version: Some("99.0".to_string()),
        };
        registry.register(custom);
        assert_eq!(
            registry.get("claude_code").unwrap().display_name,
            "Custom Claude"
        );
    }

    #[test]
    fn load_from_directory_picks_up_custom_toml() {
        let dir = TempDir::new().unwrap();
        let tools_dir = dir.path().join("tools");
        fs::create_dir_all(&tools_dir).unwrap();
        fs::write(
            tools_dir.join("windsurf.toml"),
            r#"
[tool]
name = "windsurf"
display_name = "Windsurf"
version = "1.0"
on_move = "rewrite_paths"
session_patterns = [".windsurf/"]

[[tool.path_fields]]
file = ".windsurf/state.json"
field = "project_root"
format = "json"
"#,
        )
        .unwrap();

        let mut registry = ToolRegistry::new().unwrap();
        registry.load_all(dir.path()).unwrap();

        // Builtins still present
        assert!(registry.get("claude_code").is_some());
        assert!(registry.get("cursor").is_some());
        // Custom tool loaded
        let windsurf = registry.get("windsurf").unwrap();
        assert_eq!(windsurf.display_name, "Windsurf");
        assert_eq!(windsurf.session_patterns, vec![".windsurf/"]);
        assert_eq!(windsurf.path_fields.len(), 1);
        assert_eq!(windsurf.path_fields[0].field, "project_root");
    }

    #[test]
    fn custom_toml_overrides_builtin() {
        let dir = TempDir::new().unwrap();
        let tools_dir = dir.path().join("tools");
        fs::create_dir_all(&tools_dir).unwrap();
        // Override the built-in cursor definition
        fs::write(
            tools_dir.join("cursor.toml"),
            r#"
[tool]
name = "cursor"
display_name = "Cursor (Custom)"
version = "99.0"
on_move = "notify"
session_patterns = [".cursor/", ".cursor-custom/"]
"#,
        )
        .unwrap();

        let mut registry = ToolRegistry::new().unwrap();
        registry.load_all(dir.path()).unwrap();

        let cursor = registry.get("cursor").unwrap();
        assert_eq!(cursor.display_name, "Cursor (Custom)");
        assert_eq!(cursor.version, Some("99.0".to_string()));
        assert_eq!(cursor.on_move, ReconcileStrategy::Notify);
        assert_eq!(cursor.session_patterns.len(), 2);
    }

    #[test]
    fn new_with_config_merges_project_tools() {
        let config = crate::config::Config {
            tools: vec![ToolDefinition {
                name: "aider".to_string(),
                display_name: "Aider".to_string(),
                session_patterns: vec![".aider/".to_string()],
                path_fields: vec![],
                on_move: ReconcileStrategy::Notify,
                version: Some("1.0".to_string()),
            }],
            ..Default::default()
        };

        let registry = ToolRegistry::new_with_config(&config).unwrap();

        // Builtins present
        assert!(registry.get("claude_code").is_some());
        assert!(registry.get("cursor").is_some());
        // Project tool merged
        let aider = registry.get("aider").unwrap();
        assert_eq!(aider.display_name, "Aider");
    }

    #[test]
    fn config_tools_override_builtins() {
        let config = crate::config::Config {
            tools: vec![ToolDefinition {
                name: "claude_code".to_string(),
                display_name: "Claude Code (Project Override)".to_string(),
                session_patterns: vec![".claude/".to_string()],
                path_fields: vec![],
                on_move: ReconcileStrategy::RewritePaths,
                version: Some("99.0".to_string()),
            }],
            ..Default::default()
        };

        let registry = ToolRegistry::new_with_config(&config).unwrap();

        let claude = registry.get("claude_code").unwrap();
        assert_eq!(claude.display_name, "Claude Code (Project Override)");
        assert_eq!(claude.version, Some("99.0".to_string()));
    }

    #[test]
    fn non_toml_files_ignored() {
        let dir = TempDir::new().unwrap();
        let tools_dir = dir.path().join("tools");
        fs::create_dir_all(&tools_dir).unwrap();
        fs::write(tools_dir.join("readme.md"), "# Tools").unwrap();
        fs::write(tools_dir.join(".DS_Store"), "junk").unwrap();

        let mut registry = ToolRegistry::new().unwrap();
        registry.load_all(dir.path()).unwrap();

        // Only builtins present, no errors from non-TOML files.
        // Counts all compiled-in built-in tool TOML files.
        assert_eq!(registry.tools.len(), 5);
    }
}
