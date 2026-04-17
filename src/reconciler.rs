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
use crate::event_log::{EventLog, LogEntry, ReconcileAction};
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
    let pairs: Vec<(String, String)> = path_pair_candidates(old_root, new_root);

    for field_spec in &tool.path_fields {
        let artifact_path = new_root.join(&field_spec.file);
        if !artifact_path.exists() {
            debug!(path = %artifact_path.display(), "artifact file not found, skipping");
            continue;
        }

        let result = rewrite_field(&artifact_path, field_spec, &pairs);

        match result {
            Ok(changed) => {
                if changed {
                    let action = ReconcileAction {
                        tool_name: tool.name.clone(),
                        file_path: artifact_path.clone(),
                        field: field_spec.field.clone(),
                        format: field_spec.format.clone(),
                        old_value: pairs[0].0.clone(),
                        new_value: pairs[0].1.clone(),
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

// ── Undo ─────────────────────────────────────────────────────────────────────

/// Reverse a previously-recorded reconciliation event.
///
/// Routes to the same adapter used during reconciliation (based on the
/// `format` stored in the event log) but with `old_value` / `new_value`
/// swapped — so `new_value` gets rewritten back to `old_value`.
///
/// Returns `Ok(true)` if the file was modified, `Ok(false)` if the expected
/// value wasn't found (file may have been edited since, or the event was
/// already undone manually). `dry_run = true` only checks that the file
/// exists and reports what would happen without writing.
pub fn undo_event(entry: &LogEntry, dry_run: bool) -> Result<bool> {
    use crate::tools::PathFieldSpec;

    if !entry.file_path.exists() {
        return Err(crate::error::Error::Reconcile {
            path: entry.file_path.clone(),
            detail: "target file no longer exists".to_string(),
        });
    }

    if dry_run {
        return Ok(true);
    }

    // Synthesise a PathFieldSpec from the event log. The adapters consume
    // only `field` and `format`; `file` is irrelevant here because we already
    // have the concrete artifact path.
    let field_spec = PathFieldSpec {
        file: String::new(),
        field: entry.field.clone(),
        format: entry.format.clone(),
    };

    // Swap old ↔ new: rewrite new_value back to old_value.
    let pairs = vec![(entry.new_value.clone(), entry.old_value.clone())];
    rewrite_field(&entry.file_path, &field_spec, &pairs)
}

/// Produce paired (old, new) path-string candidates to try in order.
///
/// On macOS, `/tmp` is a symlink to `/private/tmp` and `/var/folders/...`
/// lives under `/private/var/folders/...`. notify reports the canonical
/// (`/private`-prefixed) form, but user-facing paths in session files are
/// usually the shorter form. We try both; each candidate rewrites the OLD
/// form to the matching NEW form, preserving the style the tool originally
/// used. Try short form first — it's what user-level tooling tends to store.
fn path_pair_candidates(old: &Path, new: &Path) -> Vec<(String, String)> {
    let old_raw = old.to_string_lossy().to_string();
    let new_raw = new.to_string_lossy().to_string();
    let mut out = Vec::with_capacity(2);

    #[cfg(target_os = "macos")]
    {
        // Prefer short form first (what users typically see in their session files).
        if let (Some(old_short), Some(new_short)) = (
            old_raw.strip_prefix("/private"),
            new_raw.strip_prefix("/private"),
        ) {
            if !old_short.is_empty() && !new_short.is_empty() {
                out.push((old_short.to_string(), new_short.to_string()));
            }
        }
    }

    // Always include the raw form.
    out.push((old_raw, new_raw));

    // Also include the /private-prefixed form if old started with /var or /tmp.
    #[cfg(target_os = "macos")]
    {
        let (raw_old, raw_new) = (&out[out.len() - 1].0, &out[out.len() - 1].1);
        if raw_old.starts_with("/var/") || raw_old.starts_with("/tmp/") {
            out.push((format!("/private{raw_old}"), format!("/private{raw_new}")));
        }
    }

    out
}

// ── Adapter dispatch ─────────────────────────────────────────────────────────

/// Rewrite a single field in an artifact file, dispatching to the right adapter.
fn rewrite_field(
    path: &Path,
    field_spec: &PathFieldSpec,
    pairs: &[(String, String)],
) -> Result<bool> {
    match field_spec.format.as_str() {
        "json" => rewrite_json_field(path, &field_spec.field, pairs),
        "toml" => rewrite_toml_field(path, &field_spec.field, pairs),
        _ => rewrite_text(path, pairs),
    }
}

/// Rewrite the `old` prefix → `new` in `value` for the first matching pair.
///
/// `pairs` is an ordered list of (old_root, new_root) strings. For each pair,
/// `value` matches if it equals `old_root` exactly or begins with
/// `old_root` followed by a path separator (`/` or `\`). When a pair matches,
/// `value` is rewritten using THAT pair's `new_root` — preserving whichever
/// form the stored path used (e.g. `/var/...` stays `/var/...`, not
/// `/private/var/...`).
///
/// Protects against substring collisions like `/home/me/code` being
/// rewritten inside `/home/me/code-backup/foo`.
fn replace_path_prefix(value: &str, pairs: &[(String, String)]) -> Option<String> {
    for (old_root, new_root) in pairs {
        if value == old_root {
            return Some(new_root.clone());
        }
        for sep in ['/', '\\'] {
            let prefix = format!("{old_root}{sep}");
            if let Some(rest) = value.strip_prefix(&prefix) {
                return Some(format!("{new_root}{sep}{rest}"));
            }
        }
    }
    None
}

// ── JSON adapter ─────────────────────────────────────────────────────────────

/// Parse JSON, rewrite only the specified field, write back.
fn rewrite_json_field(path: &Path, field: &str, pairs: &[(String, String)]) -> Result<bool> {
    let content = std::fs::read_to_string(path)?;
    let mut value: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| crate::error::Error::Reconcile {
            path: path.to_owned(),
            detail: format!("invalid JSON: {e}"),
        })?;

    let changed = json_set_field(&mut value, field, pairs);

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

/// Walk a dot-separated field path in a JSON value and replace the old path prefix.
fn json_set_field(value: &mut serde_json::Value, field: &str, pairs: &[(String, String)]) -> bool {
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
        if let Some(rewritten) = replace_path_prefix(s, pairs) {
            *s = rewritten;
            return true;
        }
    }

    false
}

// ── TOML adapter ─────────────────────────────────────────────────────────────

/// Parse TOML, rewrite only the specified field, write back.
fn rewrite_toml_field(path: &Path, field: &str, pairs: &[(String, String)]) -> Result<bool> {
    let content = std::fs::read_to_string(path)?;
    let mut value: toml::Value =
        toml::from_str(&content).map_err(|e| crate::error::Error::Reconcile {
            path: path.to_owned(),
            detail: format!("invalid TOML: {e}"),
        })?;

    let changed = toml_set_field(&mut value, field, pairs);

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

/// Walk a dot-separated field path in a TOML value and replace the old path prefix.
fn toml_set_field(value: &mut toml::Value, field: &str, pairs: &[(String, String)]) -> bool {
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
        if let Some(rewritten) = replace_path_prefix(s, pairs) {
            *s = rewritten;
            return true;
        }
    }

    false
}

// ── Text adapter (fallback) ──────────────────────────────────────────────────

/// Simple string replacement in a file. Returns true if any changes were made.
/// Tries each candidate in `old_roots`; first whose literal substring is found
/// is replaced. Unlike the structured JSON/TOML adapters, this is a raw
/// substring replace and doesn't guard against prefix collisions — reserved
/// for formats where we have no semantic understanding.
fn rewrite_text(path: &Path, pairs: &[(String, String)]) -> Result<bool> {
    let content = std::fs::read_to_string(path)?;
    for (old_str, new_str) in pairs {
        if content.contains(old_str.as_str()) {
            let updated = content.replace(old_str.as_str(), new_str);
            std::fs::write(path, updated)?;
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::EventLog;
    use crate::tools::ToolRegistry;
    use std::fs;
    use tempfile::TempDir;

    // ── Unit tests ───────────────────────────────────────────────────────

    /// Test helper: single (old, new) pair as the adapters now expect.
    fn one_pair(old: &str, new: &str) -> Vec<(String, String)> {
        vec![(old.to_string(), new.to_string())]
    }

    #[test]
    fn rewrite_text_replaces_paths() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.txt");
        fs::write(&file, "path = /old/project").unwrap();

        let changed = rewrite_text(&file, &one_pair("/old/project", "/new/project")).unwrap();
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

        let changed = rewrite_text(&file, &one_pair("/old/project", "/new/project")).unwrap();
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

        let changed =
            rewrite_json_field(&file, "project_path", &one_pair("/old/root", "/new/root")).unwrap();
        assert!(changed);

        let content = fs::read_to_string(&file).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["project_path"], "/new/root");
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

        let changed =
            rewrite_json_field(&file, "cache.dir", &one_pair("/old/root", "/new/root")).unwrap();
        assert!(changed);

        let content = fs::read_to_string(&file).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["cache"]["dir"], "/new/root/.cache");
        assert_eq!(v["name"], "test");
    }

    #[test]
    fn replace_path_prefix_exact_match() {
        assert_eq!(
            replace_path_prefix("/home/me/code", &one_pair("/home/me/code", "/new/root")),
            Some("/new/root".to_string())
        );
    }

    #[test]
    fn replace_path_prefix_with_trailing_segment() {
        assert_eq!(
            replace_path_prefix(
                "/home/me/code/src/main.rs",
                &one_pair("/home/me/code", "/new/root")
            ),
            Some("/new/root/src/main.rs".to_string())
        );
    }

    #[test]
    fn replace_path_prefix_rejects_substring_collision() {
        assert_eq!(
            replace_path_prefix(
                "/home/me/code-backup/foo",
                &one_pair("/home/me/code", "/new/root")
            ),
            None
        );
        assert_eq!(
            replace_path_prefix(
                "/home/me/code_archive",
                &one_pair("/home/me/code", "/new/root")
            ),
            None
        );
    }

    #[test]
    fn replace_path_prefix_windows_separator() {
        assert_eq!(
            replace_path_prefix(
                r"C:\old\project\src",
                &one_pair(r"C:\old\project", r"D:\new")
            ),
            Some(r"D:\new\src".to_string())
        );
    }

    #[test]
    fn replace_path_prefix_uses_matching_pairs_new_root() {
        // value uses short form; first candidate is long form (won't match),
        // second is short. We must rewrite with the SHORT new_root — not
        // surprise the user by switching to the long form.
        let pairs = vec![
            (
                "/private/var/folders/xx".to_string(),
                "/private/var/folders/yy".to_string(),
            ),
            ("/var/folders/xx".to_string(), "/var/folders/yy".to_string()),
        ];
        assert_eq!(
            replace_path_prefix("/var/folders/xx/foo", &pairs),
            Some("/var/folders/yy/foo".to_string())
        );
    }

    #[test]
    fn json_adapter_does_not_corrupt_sibling_path() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("settings.json");
        fs::write(&file, r#"{"project_path": "/home/me/code-backup/notes"}"#).unwrap();

        let changed = rewrite_json_field(
            &file,
            "project_path",
            &one_pair("/home/me/code", "/new/root"),
        )
        .unwrap();
        assert!(!changed, "sibling path must not be treated as a prefix");

        let content = fs::read_to_string(&file).unwrap();
        assert!(content.contains("/home/me/code-backup/notes"));
    }

    #[test]
    fn json_adapter_missing_field_returns_false() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("settings.json");
        fs::write(&file, r#"{"other_field": "value"}"#).unwrap();

        let changed =
            rewrite_json_field(&file, "project_path", &one_pair("/old/root", "/new/root")).unwrap();
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

        let changed =
            rewrite_toml_field(&file, "project_root", &one_pair("/old/root", "/new/root")).unwrap();
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

        let changed =
            rewrite_toml_field(&file, "cache.dir", &one_pair("/old/root", "/new/root")).unwrap();
        assert!(changed);

        let content = fs::read_to_string(&file).unwrap();
        let v: toml::Value = toml::from_str(&content).unwrap();
        assert_eq!(v["cache"]["dir"].as_str().unwrap(), "/new/root/.cache");
        assert_eq!(v["meta"]["name"].as_str().unwrap(), "test");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn path_pair_candidates_adds_private_variant_macos() {
        // /var/folders/... → pairs include both short and /private-prefixed.
        let pairs = path_pair_candidates(
            Path::new("/var/folders/xx/abc"),
            Path::new("/var/folders/xx/def"),
        );
        assert!(pairs
            .iter()
            .any(|(o, n)| o == "/var/folders/xx/abc" && n == "/var/folders/xx/def"));
        assert!(
            pairs
                .iter()
                .any(|(o, n)| o == "/private/var/folders/xx/abc"
                    && n == "/private/var/folders/xx/def")
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn path_pair_candidates_strips_private_variant_macos() {
        // /private/tmp/foo → pairs include the short form too.
        let pairs =
            path_pair_candidates(Path::new("/private/tmp/foo"), Path::new("/private/tmp/bar"));
        assert!(pairs
            .iter()
            .any(|(o, n)| o == "/tmp/foo" && n == "/tmp/bar"));
        assert!(pairs
            .iter()
            .any(|(o, n)| o == "/private/tmp/foo" && n == "/private/tmp/bar"));
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

    // ── Undo tests ───────────────────────────────────────────────────────

    /// Round-trip: reconcile then undo → file is back to original state.
    #[test]
    fn undo_restores_json_field() {
        let sandbox = TempDir::new().unwrap();
        let old_path = sandbox.path().join("orig");
        let new_path = sandbox.path().join("dest");
        fs::create_dir_all(old_path.join(".claude")).unwrap();
        let settings = old_path.join(".claude/settings.json");
        let original = format!(
            r#"{{"project_path": "{}","model": "opus"}}"#,
            old_path.display()
        );
        fs::write(&settings, &original).unwrap();
        fs::rename(&old_path, &new_path).unwrap();

        let registry = ToolRegistry::new().unwrap();
        let tool = registry.get("claude_code").unwrap();
        let event_log = EventLog::open_in_memory().unwrap();
        let r = reconcile(tool, &old_path, &new_path, &event_log);
        assert!(r.success);

        // Grab the logged entry and run undo
        let entry = &event_log.recent(1).unwrap()[0];
        let changed = undo_event(entry, false).unwrap();
        assert!(changed, "undo should modify the file");

        // File now contains the OLD path again
        let content = fs::read_to_string(new_path.join(".claude/settings.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(
            v["project_path"].as_str().unwrap(),
            old_path.to_string_lossy()
        );
        assert_eq!(v["model"], "opus");
    }

    /// Dry-run undo doesn't modify the file.
    #[test]
    fn undo_dry_run_leaves_file_alone() {
        let sandbox = TempDir::new().unwrap();
        let old_path = sandbox.path().join("dry-orig");
        let new_path = sandbox.path().join("dry-dest");
        fs::create_dir_all(old_path.join(".claude")).unwrap();
        fs::write(
            old_path.join(".claude/settings.json"),
            format!(r#"{{"project_path": "{}"}}"#, old_path.display()),
        )
        .unwrap();
        fs::rename(&old_path, &new_path).unwrap();

        let registry = ToolRegistry::new().unwrap();
        let event_log = EventLog::open_in_memory().unwrap();
        let _ = reconcile(
            registry.get("claude_code").unwrap(),
            &old_path,
            &new_path,
            &event_log,
        );
        let after_reconcile = fs::read_to_string(new_path.join(".claude/settings.json")).unwrap();

        let entry = &event_log.recent(1).unwrap()[0];
        let would_undo = undo_event(entry, true).unwrap();
        assert!(would_undo, "dry run should report it would undo");

        let after_dry_run = fs::read_to_string(new_path.join(".claude/settings.json")).unwrap();
        assert_eq!(
            after_reconcile, after_dry_run,
            "dry run must not modify the file"
        );
    }

    /// Undoing when the file no longer contains the expected new_value
    /// returns Ok(false) rather than corrupting things.
    #[test]
    fn undo_is_safe_when_file_has_been_modified() {
        let sandbox = TempDir::new().unwrap();
        let artifact = sandbox.path().join("settings.json");
        fs::write(&artifact, r#"{"project_path": "/totally/different"}"#).unwrap();

        let fake_entry = LogEntry {
            id: 1,
            timestamp: "2026-01-01 00:00:00".into(),
            tool_name: "claude_code".into(),
            file_path: artifact.clone(),
            field: "project_path".into(),
            format: "json".into(),
            old_value: "/orig/path".into(),
            new_value: "/expected/new/path".into(),
            undone_at: None,
        };

        let changed = undo_event(&fake_entry, false).unwrap();
        assert!(
            !changed,
            "undo must report no-change when new_value isn't found"
        );

        // File untouched
        let content = fs::read_to_string(&artifact).unwrap();
        assert!(content.contains("/totally/different"));
    }

    /// Undo errors cleanly (doesn't panic) when the target file is gone.
    #[test]
    fn undo_errors_when_file_missing() {
        let sandbox = TempDir::new().unwrap();
        let entry = LogEntry {
            id: 1,
            timestamp: "2026-01-01 00:00:00".into(),
            tool_name: "claude_code".into(),
            file_path: sandbox.path().join("does-not-exist.json"),
            field: "project_path".into(),
            format: "json".into(),
            old_value: "/old".into(),
            new_value: "/new".into(),
            undone_at: None,
        };
        let err = undo_event(&entry, false).unwrap_err();
        assert!(err.to_string().contains("no longer exists"));
    }
}
