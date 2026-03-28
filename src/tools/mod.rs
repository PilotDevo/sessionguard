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

impl ToolRegistry {
    /// Create a new registry loaded with built-in defaults.
    pub fn new() -> Result<Self> {
        let mut registry = Self::default();
        registry.load_builtin()?;
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
        for toml_str in [BUILTIN_CLAUDE_CODE, BUILTIN_CURSOR] {
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

    #[test]
    fn builtin_tools_load() {
        let registry = ToolRegistry::new().unwrap();
        assert!(registry.get("claude_code").is_some());
        assert!(registry.get("cursor").is_some());
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
        registry.register(custom.clone());
        assert_eq!(
            registry.get("claude_code").unwrap().display_name,
            "Custom Claude"
        );
    }
}
