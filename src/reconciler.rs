// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Path reconciliation engine.
//!
//! When a project directory moves, the reconciler rewrites internal
//! path references in AI tool session artifacts so tools can pick up
//! where they left off.
//!
//! Adapters handle format-specific rewriting (JSON, TOML) with a
//! fallback to plain string replacement for unknown formats.

use std::path::Path;

use tracing::{debug, info, warn};

use crate::error::Result;
use crate::event_log::{EventLog, ReconcileAction};
use crate::tools::{PathFieldSpec, ReconcileStrategy, ToolDefinition};

/// Outcome of a single reconciliation operation.
#[derive(Debug, Clone)]
pub struct ReconcileResult {
    pub tool_name: String,
    pub actions_taken: Vec<ReconcileAction>,
    pub success: bool,
    pub error: Option<String>,
}

/// Reconcile session artifacts for a project that has moved.
pub fn reconcile(
    tool: &ToolDefinition,
    old_root: &Path,
    new_root: &Path,
    event_log: &EventLog,
) -> ReconcileResult {
    info!(
        tool = %tool.name,
        from = %old_root.display(),
        to = %new_root.display(),
        "reconciling session artifacts"
    );

    match &tool.on_move {
        ReconcileStrategy::RewritePaths => rewrite_paths(tool, old_root, new_root, event_log),
        ReconcileStrategy::Notify => {
            info!(tool = %tool.name, "notify-only strategy, no paths rewritten");
            ReconcileResult {
                tool_name: tool.name.clone(),
                actions_taken: vec![],
                success: true,
                error: None,
            }
        }
        ReconcileStrategy::Custom(cmd) => {
            warn!(tool = %tool.name, cmd = %cmd, "custom reconciliation not yet implemented");
            ReconcileResult {
                tool_name: tool.name.clone(),
                actions_taken: vec![],
                success: false,
                error: Some("custom reconciliation not yet implemented".to_string()),
            }
        }
    }
}

fn rewrite_paths(
    tool: &ToolDefinition,
    old_root: &Path,
    new_root: &Path,
    event_log: &EventLog,
) -> ReconcileResult {
    let mut actions = Vec::new();
    let old_root_str = old_root.to_string_lossy();
    let new_root_str = new_root.to_string_lossy();

    for field_spec in &tool.path_fields {
        let artifact_path = new_root.join(&field_spec.file);
        if !artifact_path.exists() {
            debug!(path = %artifact_path.display(), "artifact file not found, skipping");
            continue;
        }

        let result = rewrite_field(&artifact_path, field_spec, &old_root_str, &new_root_str);

        match result {
            Ok(changed) => {
                if changed {
                    let action = ReconcileAction {
                        tool_name: tool.name.clone(),
                        file_path: artifact_path.clone(),
                        field: field_spec.field.clone(),
                        old_value: old_root_str.to_string(),
                        new_value: new_root_str.to_string(),
                    };
                    if let Err(e) = event_log.record(&action) {
                        warn!(error = %e, "failed to record reconciliation action");
                    }
                    actions.push(action);
                }
            }
            Err(e) => {
                return ReconcileResult {
                    tool_name: tool.name.clone(),
                    actions_taken: actions,
                    success: false,
                    error: Some(format!(
                        "failed to rewrite {}: {e}",
                        artifact_path.display()
                    )),
                };
            }
        }
    }

    ReconcileResult {
        tool_name: tool.name.clone(),
        actions_taken: actions,
        success: true,
        error: None,
    }
}

// ── Adapter dispatch ─────────────────────────────────────────────────────────

/// Rewrite a single field in an artifact file, dispatching to the right adapter.
fn rewrite_field(
    path: &Path,
    field_spec: &PathFieldSpec,
    old_str: &str,
    new_str: &str,
) -> Result<bool> {
    match field_spec.format.as_str() {
        "json" => rewrite_json_field(path, &field_spec.field, old_str, new_str),
        "toml" => rewrite_toml_field(path, &field_spec.field, old_str, new_str),
        _ => rewrite_text(path, old_str, new_str),
    }
}

// ── JSON adapter ─────────────────────────────────────────────────────────────

/// Parse JSON, rewrite only the specified field, write back.
fn rewrite_json_field(path: &Path, field: &str, old_str: &str, new_str: &str) -> Result<bool> {
    let content = std::fs::read_to_string(path)?;
    let mut value: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| crate::error::Error::Reconcile {
            path: path.to_owned(),
            detail: format!("invalid JSON: {e}"),
        })?;

    let changed = json_set_field(&mut value, field, old_str, new_str);

    if changed {
        let updated =
            serde_json::to_string_pretty(&value).map_err(|e| crate::error::Error::Reconcile {
                path: path.to_owned(),
                detail: format!("failed to serialize JSON: {e}"),
            })?;
        std::fs::write(path, updated)?;
        debug!(path = %path.display(), field, "JSON field rewritten");
    }

    Ok(changed)
}

/// Walk a dot-separated field path in a JSON value and replace the old string.
fn json_set_field(
    value: &mut serde_json::Value,
    field: &str,
    old_str: &str,
    new_str: &str,
) -> bool {
    let parts: Vec<&str> = field.split('.').collect();
    let mut current = value;

    for part in &parts {
        match current {
            serde_json::Value::Object(map) => {
                if let Some(v) = map.get_mut(*part) {
                    current = v;
                } else {
                    return false;
                }
            }
            _ => return false,
        }
    }

    if let serde_json::Value::String(s) = current {
        if s.contains(old_str) {
            *s = s.replace(old_str, new_str);
            return true;
        }
    }

    false
}

// ── TOML adapter ─────────────────────────────────────────────────────────────

/// Parse TOML, rewrite only the specified field, write back.
fn rewrite_toml_field(path: &Path, field: &str, old_str: &str, new_str: &str) -> Result<bool> {
    let content = std::fs::read_to_string(path)?;
    let mut value: toml::Value =
        toml::from_str(&content).map_err(|e| crate::error::Error::Reconcile {
            path: path.to_owned(),
            detail: format!("invalid TOML: {e}"),
        })?;

    let changed = toml_set_field(&mut value, field, old_str, new_str);

    if changed {
        let updated =
            toml::to_string_pretty(&value).map_err(|e| crate::error::Error::Reconcile {
                path: path.to_owned(),
                detail: format!("failed to serialize TOML: {e}"),
            })?;
        std::fs::write(path, updated)?;
        debug!(path = %path.display(), field, "TOML field rewritten");
    }

    Ok(changed)
}

/// Walk a dot-separated field path in a TOML value and replace the old string.
fn toml_set_field(value: &mut toml::Value, field: &str, old_str: &str, new_str: &str) -> bool {
    let parts: Vec<&str> = field.split('.').collect();
    let mut current = value;

    for part in &parts {
        match current {
            toml::Value::Table(table) => {
                if let Some(v) = table.get_mut(*part) {
                    current = v;
                } else {
                    return false;
                }
            }
            _ => return false,
        }
    }

    if let toml::Value::String(s) = current {
        if s.contains(old_str) {
            *s = s.replace(old_str, new_str);
            return true;
        }
    }

    false
}

// ── Text adapter (fallback) ──────────────────────────────────────────────────

/// Simple string replacement in a file. Returns true if any changes were made.
fn rewrite_text(path: &Path, old_str: &str, new_str: &str) -> Result<bool> {
    let content = std::fs::read_to_string(path)?;
    if content.contains(old_str) {
        let updated = content.replace(old_str, new_str);
        std::fs::write(path, updated)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::EventLog;
    use crate::tools::ToolRegistry;
    use std::fs;
    use tempfile::TempDir;

    // ── Unit tests ───────────────────────────────────────────────────────

    #[test]
    fn rewrite_text_replaces_paths() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.txt");
        fs::write(&file, "path = /old/project").unwrap();

        let changed = rewrite_text(&file, "/old/project", "/new/project").unwrap();
        assert!(changed);

        let content = fs::read_to_string(&file).unwrap();
        assert!(content.contains("/new/project"));
        assert!(!content.contains("/old/project"));
    }

    #[test]
    fn rewrite_text_no_change_returns_false() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.txt");
        fs::write(&file, "path = /some/other/path").unwrap();

        let changed = rewrite_text(&file, "/old/project", "/new/project").unwrap();
        assert!(!changed);
    }

    #[test]
    fn json_adapter_rewrites_only_target_field() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("settings.json");
        fs::write(
            &file,
            r#"{"project_path": "/old/root", "description": "lives at /old/root too"}"#,
        )
        .unwrap();

        let changed = rewrite_json_field(&file, "project_path", "/old/root", "/new/root").unwrap();
        assert!(changed);

        let content = fs::read_to_string(&file).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        // Target field was rewritten
        assert_eq!(v["project_path"], "/new/root");
        // Non-target field with same string was NOT touched
        assert_eq!(v["description"], "lives at /old/root too");
    }

    #[test]
    fn json_adapter_handles_nested_fields() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("config.json");
        fs::write(
            &file,
            r#"{"cache": {"dir": "/old/root/.cache"}, "name": "test"}"#,
        )
        .unwrap();

        let changed = rewrite_json_field(&file, "cache.dir", "/old/root", "/new/root").unwrap();
        assert!(changed);

        let content = fs::read_to_string(&file).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["cache"]["dir"], "/new/root/.cache");
        assert_eq!(v["name"], "test");
    }

    #[test]
    fn json_adapter_missing_field_returns_false() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("settings.json");
        fs::write(&file, r#"{"other_field": "value"}"#).unwrap();

        let changed = rewrite_json_field(&file, "project_path", "/old/root", "/new/root").unwrap();
        assert!(!changed);
    }

    #[test]
    fn toml_adapter_rewrites_only_target_field() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("config.toml");
        fs::write(
            &file,
            "project_root = \"/old/root\"\ndescription = \"lives at /old/root too\"\n",
        )
        .unwrap();

        let changed = rewrite_toml_field(&file, "project_root", "/old/root", "/new/root").unwrap();
        assert!(changed);

        let content = fs::read_to_string(&file).unwrap();
        let v: toml::Value = toml::from_str(&content).unwrap();
        assert_eq!(v["project_root"].as_str().unwrap(), "/new/root");
        assert_eq!(v["description"].as_str().unwrap(), "lives at /old/root too");
    }

    #[test]
    fn toml_adapter_handles_nested_fields() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("config.toml");
        fs::write(
            &file,
            "[cache]\ndir = \"/old/root/.cache\"\n\n[meta]\nname = \"test\"\n",
        )
        .unwrap();

        let changed = rewrite_toml_field(&file, "cache.dir", "/old/root", "/new/root").unwrap();
        assert!(changed);

        let content = fs::read_to_string(&file).unwrap();
        let v: toml::Value = toml::from_str(&content).unwrap();
        assert_eq!(v["cache"]["dir"].as_str().unwrap(), "/new/root/.cache");
        assert_eq!(v["meta"]["name"].as_str().unwrap(), "test");
    }

    // ── End-to-end proof tests ───────────────────────────────────────────

    /// End-to-end proof: moving a Claude Code project rewrites .claude/settings.json
    #[test]
    fn reconcile_claude_code_end_to_end() {
        let sandbox = TempDir::new().unwrap();
        let old_path = sandbox.path().join("alpha-project");
        let new_path = sandbox.path().join("beta-project");

        // Create a realistic Claude Code project at old_path
        fs::create_dir_all(old_path.join(".claude")).unwrap();
        fs::write(old_path.join("CLAUDE.md"), "# Project").unwrap();
        fs::write(old_path.join(".claudeignore"), "target/\n").unwrap();
        fs::write(
            old_path.join(".claude/settings.json"),
            format!(
                r#"{{"project_path": "{}","model": "opus","context": "full"}}"#,
                old_path.display()
            ),
        )
        .unwrap();

        // Physically move the directory (simulates `mv`)
        fs::rename(&old_path, &new_path).unwrap();

        // Get the Claude Code tool definition
        let registry = ToolRegistry::new().unwrap();
        let tool = registry.get("claude_code").unwrap();
        let event_log = EventLog::open_in_memory().unwrap();

        // Reconcile
        let result = reconcile(tool, &old_path, &new_path, &event_log);

        // Assertions
        assert!(result.success, "reconciliation should succeed");
        assert_eq!(result.actions_taken.len(), 1, "should rewrite one file");
        assert_eq!(result.actions_taken[0].field, "project_path");

        // Verify the file was actually rewritten — parse as JSON to check field-level
        let content = fs::read_to_string(new_path.join(".claude/settings.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(
            v["project_path"].as_str().unwrap(),
            new_path.to_string_lossy()
        );
        // Non-path fields intact
        assert_eq!(v["model"], "opus");
        assert_eq!(v["context"], "full");

        // Verify event log recorded the action
        let entries = event_log.recent(10).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tool_name, "claude_code");
    }

    /// End-to-end proof: moving a Cursor project rewrites .cursor/state.json
    #[test]
    fn reconcile_cursor_end_to_end() {
        let sandbox = TempDir::new().unwrap();
        let old_path = sandbox.path().join("cursor-original");
        let new_path = sandbox.path().join("cursor-relocated");

        // Create a realistic Cursor project at old_path
        fs::create_dir_all(old_path.join(".cursor/rules")).unwrap();
        fs::write(old_path.join(".cursorignore"), "node_modules/\n").unwrap();
        fs::write(
            old_path.join(".cursor/state.json"),
            format!(
                r#"{{"project_root": "{}","workspace_id": "abc123"}}"#,
                old_path.display()
            ),
        )
        .unwrap();
        fs::write(
            old_path.join(".cursor/rules/style.md"),
            "Use TypeScript strict mode.",
        )
        .unwrap();

        // Physically move the directory
        fs::rename(&old_path, &new_path).unwrap();

        // Get the Cursor tool definition
        let registry = ToolRegistry::new().unwrap();
        let tool = registry.get("cursor").unwrap();
        let event_log = EventLog::open_in_memory().unwrap();

        // Reconcile
        let result = reconcile(tool, &old_path, &new_path, &event_log);

        // Assertions
        assert!(result.success, "reconciliation should succeed");
        assert_eq!(result.actions_taken.len(), 1);
        assert_eq!(result.actions_taken[0].field, "project_root");

        // Verify field-level rewrite via JSON parsing
        let content = fs::read_to_string(new_path.join(".cursor/state.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(
            v["project_root"].as_str().unwrap(),
            new_path.to_string_lossy()
        );
        assert_eq!(v["workspace_id"], "abc123");

        // Verify event log
        let entries = event_log.recent(10).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tool_name, "cursor");
    }

    /// Proof: multi-tool project gets all artifacts reconciled
    #[test]
    fn reconcile_multi_tool_end_to_end() {
        let sandbox = TempDir::new().unwrap();
        let old_path = sandbox.path().join("original-multi");
        let new_path = sandbox.path().join("relocated-multi");

        // Create a project with both Claude and Cursor artifacts
        fs::create_dir_all(old_path.join(".claude")).unwrap();
        fs::write(
            old_path.join(".claude/settings.json"),
            format!(r#"{{"project_path": "{}"}}"#, old_path.display()),
        )
        .unwrap();
        fs::create_dir_all(old_path.join(".cursor")).unwrap();
        fs::write(
            old_path.join(".cursor/state.json"),
            format!(r#"{{"project_root": "{}"}}"#, old_path.display()),
        )
        .unwrap();

        // Move
        fs::rename(&old_path, &new_path).unwrap();

        let registry = ToolRegistry::new().unwrap();
        let event_log = EventLog::open_in_memory().unwrap();

        // Reconcile both tools
        for tool_name in ["claude_code", "cursor"] {
            let tool = registry.get(tool_name).unwrap();
            let result = reconcile(tool, &old_path, &new_path, &event_log);
            assert!(result.success, "{tool_name} reconciliation should succeed");
            assert_eq!(result.actions_taken.len(), 1);
        }

        // Verify both files rewritten
        let claude_content = fs::read_to_string(new_path.join(".claude/settings.json")).unwrap();
        let cursor_content = fs::read_to_string(new_path.join(".cursor/state.json")).unwrap();

        assert!(claude_content.contains(&new_path.to_string_lossy().to_string()));
        assert!(cursor_content.contains(&new_path.to_string_lossy().to_string()));

        // Event log should have 2 entries
        assert_eq!(event_log.count().unwrap(), 2);
    }

    /// Proof: JSON adapter doesn't corrupt sibling fields containing the same path
    #[test]
    fn json_adapter_surgical_not_global() {
        let sandbox = TempDir::new().unwrap();
        let old_path = sandbox.path().join("orig-project");
        let new_path = sandbox.path().join("dest-project");

        fs::create_dir_all(old_path.join(".claude")).unwrap();
        // Both fields contain the old path — only project_path should be rewritten
        fs::write(
            old_path.join(".claude/settings.json"),
            format!(
                r#"{{"project_path": "{0}","notes": "project was cloned from {0}"}}"#,
                old_path.display()
            ),
        )
        .unwrap();

        fs::rename(&old_path, &new_path).unwrap();

        let registry = ToolRegistry::new().unwrap();
        let tool = registry.get("claude_code").unwrap();
        let event_log = EventLog::open_in_memory().unwrap();

        let result = reconcile(tool, &old_path, &new_path, &event_log);
        assert!(result.success);

        let content = fs::read_to_string(new_path.join(".claude/settings.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();

        // Target field rewritten
        assert_eq!(
            v["project_path"].as_str().unwrap(),
            new_path.to_string_lossy()
        );
        // Non-target field with same string was NOT touched — this is the key proof
        assert!(
            v["notes"]
                .as_str()
                .unwrap()
                .contains(&old_path.to_string_lossy().to_string()),
            "notes field should still contain the OLD path (surgical rewrite)"
        );
    }
}
