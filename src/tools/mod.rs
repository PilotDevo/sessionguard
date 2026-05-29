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

/// How a tool discovers its data directory at startup.
///
/// Drives the rewrite stage of `sessionguard migrate`: depending on
/// how the tool finds its data, we either edit a config file, set an
/// env var (typically via systemd unit override), or just leave a
/// symlink in place. See `docs/design/migrate.md` §"Per-tool
/// `home_dir_layout` schema".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HomeDirDiscovery {
    /// Path is set via an environment variable named in `env_var`.
    Env,
    /// Path is embedded in one or more files declared in `config_files`.
    Config,
    /// Migration replaces `default_path` with a symlink to the new
    /// location; the tool follows the symlink transparently. Works
    /// for tools that don't resolve symlinks; verify per-tool.
    #[default]
    Symlink,
    /// Path is baked into the binary; migration impossible without
    /// reinstalling. SessionGuard refuses to migrate such tools.
    Compile,
}

/// Reference to a config file that names the tool's data dir. Reuses
/// the same JSON/TOML/text adapter dispatch as in-project `path_fields`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeDirConfigFile {
    /// Path to the config file, with `~` expanded to the user's home.
    pub file: String,
    /// Dot-separated field within the file (e.g. `data_dir`, `storage.path`).
    pub field: String,
    /// File format: `json`, `toml`, or anything else (text fallback).
    #[serde(default = "default_format")]
    pub format: String,
}

/// systemd integration for quiescing a tool before migrating its data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HomeDirQuiesce {
    /// Stop this user-scope unit (`systemctl --user stop <name>`)
    /// before the copy stage; restart it after rewrite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub systemd_user_unit: Option<String>,
    /// System-scope unit equivalent. Mutually exclusive with
    /// `systemd_user_unit` per-invocation; declaring both means
    /// "try user first, fall back to system" at migrate time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub systemd_system_unit: Option<String>,
}

/// Optional post-rewrite validation step run during migrate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HomeDirValidate {
    /// Argv to invoke after restart. Migrate considers the migration
    /// successful only if this exits zero within `timeout_seconds`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Validation timeout. Defaults to 10s if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
}

/// Per-tool layout description used by `sessionguard migrate` (v0.4+).
///
/// Tools without this block are *in-project only* — they reconcile on
/// project moves (existing v0.3 behaviour) but cannot be migrated by
/// `sessionguard migrate`. The schema is intentionally permissive on
/// load (any combination of optional fields parses) so future tool
/// definitions can mix and match without bumping the schema version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeDirLayout {
    /// Canonical default location the tool reads/writes from.
    /// May contain `~` for the user's home; expanded at runtime, not
    /// at parse time, so the same TOML works for any user.
    pub default_path: String,
    /// How the tool finds the data dir at startup.
    #[serde(default)]
    pub discovery: HomeDirDiscovery,
    /// For `discovery = "env"`: the environment variable name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_var: Option<String>,
    /// For `discovery = "config"`: one or more config files that
    /// name the data dir.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_files: Vec<HomeDirConfigFile>,
    /// Optional service-quiesce instructions (stop before, start after).
    #[serde(default, skip_serializing_if = "is_default_quiesce")]
    pub quiesce: HomeDirQuiesce,
    /// Optional post-migrate validation.
    #[serde(default, skip_serializing_if = "is_default_validate")]
    pub validate: HomeDirValidate,
}

fn is_default_quiesce(q: &HomeDirQuiesce) -> bool {
    q.systemd_user_unit.is_none() && q.systemd_system_unit.is_none()
}

fn is_default_validate(v: &HomeDirValidate) -> bool {
    v.command.is_empty() && v.timeout_seconds.is_none()
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
    /// Name of the launcher binary on PATH (e.g. `claude`, `codex`). Optional —
    /// some "tools" are IDEs without a CLI; leave unset for those. Used by the
    /// `health` module and `sessionguard doctor` to warn when session data
    /// exists but the binary that wrote it is no longer reachable (typical
    /// after a Node/Python runtime upgrade).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// How the tool stores user-scoped data and how to rewrite its self-
    /// references during `sessionguard migrate` (v0.4+). Tools without this
    /// block are skipped by migrate with a clear "no home-dir layout
    /// declared" message — they still reconcile on project moves as in
    /// v0.3.x. See `docs/design/migrate.md` for the full schema rationale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_dir_layout: Option<HomeDirLayout>,
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
const BUILTIN_CODEX: &str = include_str!("builtin/codex.toml");
const BUILTIN_OPENCODE: &str = include_str!("builtin/opencode.toml");

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
            BUILTIN_CODEX,
            BUILTIN_OPENCODE,
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
        assert!(registry.get("codex").is_some());
        assert!(registry.get("opencode").is_some());
    }

    #[test]
    fn home_dir_layout_parses_full_block() {
        // Exercise every optional sub-field of the v0.4 home_dir_layout
        // schema and confirm round-tripping doesn't lose information.
        let toml_str = r#"
[tool]
name = "example"
display_name = "Example"
version = "1.0"
on_move = "rewrite_paths"
binary = "example"
session_patterns = []

[tool.home_dir_layout]
default_path = "~/.local/share/example"
discovery = "config"

[[tool.home_dir_layout.config_files]]
file = "~/.config/example/config.json"
field = "data_dir"
format = "json"

[tool.home_dir_layout.quiesce]
systemd_user_unit = "example.service"

[tool.home_dir_layout.validate]
command = ["example", "--health"]
timeout_seconds = 5
"#;
        let parsed: ToolFile = toml::from_str(toml_str).unwrap();
        let h = parsed
            .tool
            .home_dir_layout
            .as_ref()
            .expect("layout present");
        assert_eq!(h.default_path, "~/.local/share/example");
        assert_eq!(h.discovery, HomeDirDiscovery::Config);
        assert_eq!(h.config_files.len(), 1);
        assert_eq!(h.config_files[0].field, "data_dir");
        assert_eq!(h.config_files[0].format, "json");
        assert_eq!(
            h.quiesce.systemd_user_unit.as_deref(),
            Some("example.service")
        );
        assert_eq!(h.validate.command, vec!["example", "--health"]);
        assert_eq!(h.validate.timeout_seconds, Some(5));
    }

    #[test]
    fn home_dir_layout_minimal_block_parses() {
        // The schema is intentionally permissive — only `default_path`
        // is required. Everything else falls back to defaults.
        let toml_str = r#"
[tool]
name = "minimal"
display_name = "Minimal"
session_patterns = []

[tool.home_dir_layout]
default_path = "~/.minimal"
"#;
        let parsed: ToolFile = toml::from_str(toml_str).unwrap();
        let h = parsed
            .tool
            .home_dir_layout
            .as_ref()
            .expect("layout present");
        assert_eq!(h.default_path, "~/.minimal");
        assert_eq!(h.discovery, HomeDirDiscovery::Symlink); // default
        assert!(h.config_files.is_empty());
        assert!(h.quiesce.systemd_user_unit.is_none());
        assert!(h.validate.command.is_empty());
    }

    #[test]
    fn builtin_codex_declares_env_discovery() {
        // The Codex builtin should declare CODEX_HOME as its env discovery
        // mechanism. This test will fail loudly if anyone removes or
        // mis-spells the field, which would silently break v0.4 migrate
        // for Codex.
        let registry = ToolRegistry::new().unwrap();
        let codex = registry.get("codex").unwrap();
        let h = codex
            .home_dir_layout
            .as_ref()
            .expect("codex must declare home_dir_layout for v0.4 migrate");
        assert_eq!(h.default_path, "~/.codex");
        assert_eq!(h.discovery, HomeDirDiscovery::Env);
        assert_eq!(h.env_var.as_deref(), Some("CODEX_HOME"));
        // env discovery needs a unit to drop the CODEX_HOME override
        // into; the builtin declares the conventional user unit.
        assert_eq!(
            h.quiesce.systemd_user_unit.as_deref(),
            Some("codex.service"),
            "codex builtin must declare a quiesce unit for the env-rewrite drop-in"
        );
    }

    #[test]
    fn builtin_opencode_declares_symlink_discovery() {
        let registry = ToolRegistry::new().unwrap();
        let opencode = registry.get("opencode").unwrap();
        let h = opencode
            .home_dir_layout
            .as_ref()
            .expect("opencode must declare home_dir_layout for v0.4 migrate");
        assert_eq!(h.default_path, "~/.local/share/opencode");
        assert_eq!(h.discovery, HomeDirDiscovery::Symlink);
        // WAL-safe copy: the builtin declares the conventional user
        // unit so migrate quiesces OpenCode when it runs under systemd.
        assert_eq!(
            h.quiesce.systemd_user_unit.as_deref(),
            Some("opencode.service"),
            "opencode builtin must declare a quiesce unit for WAL-safe copy"
        );
    }

    #[test]
    fn home_dir_layout_omitted_means_in_project_only() {
        // Tools that don't declare home_dir_layout (the existing v0.3.x
        // shape) parse exactly as before. v0.4 migrate will skip them.
        let toml_str = r#"
[tool]
name = "in_project_only"
display_name = "In-Project Only"
session_patterns = [".foo/"]
"#;
        let parsed: ToolFile = toml::from_str(toml_str).unwrap();
        assert!(parsed.tool.home_dir_layout.is_none());
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
            binary: None,
            home_dir_layout: None,
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
                binary: None,
                home_dir_layout: None,
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
                binary: None,
                home_dir_layout: None,
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
        assert_eq!(registry.tools.len(), 7);
    }
}
