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
    let mut errors: Vec<String> = Vec::new();
    let pairs: Vec<(String, String)> = path_pair_candidates(old_root, new_root);

    for field_spec in &tool.path_fields {
        let artifact_path = new_root.join(&field_spec.file);
        if !artifact_path.exists() {
            debug!(path = %artifact_path.display(), "artifact file not found, skipping");
            continue;
        }

        let result = rewrite_field(&artifact_path, field_spec, &pairs);

        match result {
            Ok(Some(idx)) => {
                // Record the pair that ACTUALLY matched (not pairs[0]) so
                // `undo` reverses the strings that were really written — on
                // macOS the applied pair may be the `/private`-prefixed
                // variant, and logging pairs[0] made undo a silent no-op.
                let action = ReconcileAction {
                    tool_name: tool.name.clone(),
                    file_path: artifact_path.clone(),
                    field: field_spec.field.clone(),
                    format: field_spec.format.clone(),
                    old_value: pairs[idx].0.clone(),
                    new_value: pairs[idx].1.clone(),
                };
                if let Err(e) = event_log.record(&action) {
                    // The file is already rewritten. Failing to log it means
                    // `undo` can never reverse this change, so surface it as
                    // a hard failure instead of a silent `warn!` — the
                    // operator needs to know. (H1's WAL + busy_timeout make
                    // this rare; a swallowed error here was a data-loss gap.)
                    actions.push(action);
                    return ReconcileResult {
                        tool_name: tool.name.clone(),
                        actions_taken: actions,
                        success: false,
                        error: Some(format!(
                            "rewrote {} but failed to record its undo entry \
                             (undo unavailable for this change): {e}",
                            artifact_path.display()
                        )),
                    };
                }
                actions.push(action);
            }
            Ok(None) => {}
            Err(e) => {
                // One bad artifact must not silently strand the tool's OTHER
                // fields un-reconciled — keep going, then report failure with
                // everything that did succeed recorded.
                warn!(
                    path = %artifact_path.display(),
                    error = %e,
                    "failed to rewrite artifact; continuing with remaining fields"
                );
                errors.push(format!("{}: {e}", artifact_path.display()));
            }
        }
    }

    if !errors.is_empty() {
        return ReconcileResult {
            tool_name: tool.name.clone(),
            actions_taken: actions,
            success: false,
            error: Some(format!("failed to rewrite: {}", errors.join("; "))),
        };
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
    Ok(rewrite_field(&entry.file_path, &field_spec, &pairs)?.is_some())
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
///
/// Exposed `pub(crate)` so the v0.4 migrate driver can reuse the same
/// adapter dispatch for `HomeDirDiscovery::Config` (rewriting a tool's
/// `~/.config/<tool>/config.toml`-style data-dir field). The migrate
/// driver constructs a synthetic `PathFieldSpec` from `HomeDirConfigFile`
/// and a single `(old_data_dir, new_data_dir)` pair.
/// Returns the index of the pair that was applied (so the caller can log the
/// ACTUAL (old, new) strings written, not just `pairs[0]` — undo depends on it).
pub(crate) fn rewrite_field(
    path: &Path,
    field_spec: &PathFieldSpec,
    pairs: &[(String, String)],
) -> Result<Option<usize>> {
    match field_spec.format.as_str() {
        "json" => rewrite_json_field(path, &field_spec.field, pairs),
        "toml" => rewrite_toml_field(path, &field_spec.field, pairs),
        _ => rewrite_text(path, pairs),
    }
}

/// Largest artifact we'll load into memory for rewriting. Chat-history files
/// (e.g. aider's) grow without bound; slurping hundreds of MB to rewrite a
/// path is worse than skipping with a warning the operator can act on.
const MAX_ARTIFACT_BYTES: u64 = 32 * 1024 * 1024;

/// Read an artifact for rewriting, with guardrails: files over
/// [`MAX_ARTIFACT_BYTES`] and non-UTF-8 files are skipped (with a warning)
/// rather than erroring — one odd file must not abort a whole tool's
/// reconciliation.
fn read_artifact(path: &Path) -> Result<Option<String>> {
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > MAX_ARTIFACT_BYTES {
            warn!(
                path = %path.display(),
                size = meta.len(),
                "artifact exceeds the rewrite size cap; skipping (paths inside it were NOT rewritten)"
            );
            return Ok(None);
        }
    }
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            warn!(
                path = %path.display(),
                "artifact is not valid UTF-8; skipping (paths inside it were NOT rewritten)"
            );
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

/// Rewrite the `old` prefix → `new` in `value` for the first matching pair.
///
/// `pairs` is an ordered list of (old_root, new_root) strings. For each pair,
/// `value` matches if it equals `old_root` exactly or begins with
/// `old_root` followed by a path separator (`/` or `\`). When a pair matches,
/// `value` is rewritten using THAT pair's `new_root` — preserving whichever
/// form the stored path used (e.g. `/var/...` stays `/var/...`, not
/// `/private/var/...`). Returns the rewritten value plus the index of the pair
/// that matched.
///
/// Protects against substring collisions like `/home/me/code` being
/// rewritten inside `/home/me/code-backup/foo`.
fn replace_path_prefix(value: &str, pairs: &[(String, String)]) -> Option<(String, usize)> {
    for (i, (old_root, new_root)) in pairs.iter().enumerate() {
        if value == old_root {
            return Some((new_root.clone(), i));
        }
        for sep in ['/', '\\'] {
            let prefix = format!("{old_root}{sep}");
            if let Some(rest) = value.strip_prefix(&prefix) {
                return Some((format!("{new_root}{sep}{rest}"), i));
            }
        }
    }
    None
}

/// Boundary-guarded global replace for the text adapter: replaces every
/// occurrence of `old` that is followed by a path boundary (end of file, `/`,
/// `\`, or any character that can't continue a path segment). This gives the
/// text fallback the same substring-collision protection the structured
/// adapters get from [`replace_path_prefix`] — `/home/me/code` never rewrites
/// inside `/home/me/code-backup` mentioned in e.g. a chat-history body.
fn replace_all_with_boundary(content: &str, old: &str, new: &str) -> Option<String> {
    fn continues_path_segment(c: char) -> bool {
        c.is_alphanumeric() || matches!(c, '-' | '_' | '.')
    }
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    let mut replaced = false;
    while let Some(idx) = rest.find(old) {
        let after = &rest[idx + old.len()..];
        let boundary = after
            .chars()
            .next()
            .is_none_or(|c| !continues_path_segment(c));
        out.push_str(&rest[..idx]);
        if boundary {
            out.push_str(new);
            replaced = true;
        } else {
            out.push_str(old);
        }
        rest = after;
    }
    if !replaced {
        return None;
    }
    out.push_str(rest);
    Some(out)
}

/// Write `contents` to `path` **atomically**: write a temp sibling in the same
/// directory, fsync it, copy the original file's permissions, then `rename` over
/// the original. Same-filesystem rename is atomic on POSIX, so a crash, power
/// loss, or `ENOSPC` leaves either the old file or the new file fully intact —
/// never a truncated one. This is the difference between preserving a user's
/// session artifact and destroying it; a bare `fs::write` truncates first.
fn atomic_write(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("artifact");
    // Temp sibling in the same directory (same filesystem → atomic rename).
    let tmp = dir.join(format!(".{file_name}.sg-tmp-{}", std::process::id()));

    // Preserve the original's permissions on the replacement.
    let perms = std::fs::metadata(path).map(|m| m.permissions()).ok();

    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        drop(f);
        if let Some(p) = perms {
            let _ = std::fs::set_permissions(&tmp, p);
        }
        std::fs::rename(&tmp, path)
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp); // don't litter a partial temp
    }
    result
}

// ── JSON adapter ─────────────────────────────────────────────────────────────

/// Parse JSON, rewrite only the specified field, write back. Returns the
/// index of the (old, new) pair that was applied, if any.
fn rewrite_json_field(
    path: &Path,
    field: &str,
    pairs: &[(String, String)],
) -> Result<Option<usize>> {
    let Some(content) = read_artifact(path)? else {
        return Ok(None);
    };
    let mut value: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| crate::error::Error::Reconcile {
            path: path.to_owned(),
            detail: format!("invalid JSON: {e}"),
        })?;

    let matched = json_set_field(&mut value, field, pairs);

    if matched.is_some() {
        let updated =
            serde_json::to_string_pretty(&value).map_err(|e| crate::error::Error::Reconcile {
                path: path.to_owned(),
                detail: format!("failed to serialize JSON: {e}"),
            })?;
        atomic_write(path, &updated)?;
        debug!(path = %path.display(), field, "JSON field rewritten");
    }

    Ok(matched)
}

/// Walk a dot-separated field path in a JSON value and replace the old path
/// prefix. Returns the index of the pair that matched.
fn json_set_field(
    value: &mut serde_json::Value,
    field: &str,
    pairs: &[(String, String)],
) -> Option<usize> {
    let parts: Vec<&str> = field.split('.').collect();
    let mut current = value;

    for part in &parts {
        match current {
            serde_json::Value::Object(map) => {
                if let Some(v) = map.get_mut(*part) {
                    current = v;
                } else {
                    return None;
                }
            }
            _ => return None,
        }
    }

    if let serde_json::Value::String(s) = current {
        if let Some((rewritten, idx)) = replace_path_prefix(s, pairs) {
            *s = rewritten;
            return Some(idx);
        }
    }

    None
}

// ── TOML adapter ─────────────────────────────────────────────────────────────

/// Parse TOML, rewrite only the specified field, write back. Returns the
/// index of the (old, new) pair that was applied, if any.
fn rewrite_toml_field(
    path: &Path,
    field: &str,
    pairs: &[(String, String)],
) -> Result<Option<usize>> {
    let Some(content) = read_artifact(path)? else {
        return Ok(None);
    };
    let mut value: toml::Value =
        toml::from_str(&content).map_err(|e| crate::error::Error::Reconcile {
            path: path.to_owned(),
            detail: format!("invalid TOML: {e}"),
        })?;

    let matched = toml_set_field(&mut value, field, pairs);

    if matched.is_some() {
        let updated =
            toml::to_string_pretty(&value).map_err(|e| crate::error::Error::Reconcile {
                path: path.to_owned(),
                detail: format!("failed to serialize TOML: {e}"),
            })?;
        atomic_write(path, &updated)?;
        debug!(path = %path.display(), field, "TOML field rewritten");
    }

    Ok(matched)
}

/// Walk a dot-separated field path in a TOML value and replace the old path
/// prefix. Returns the index of the pair that matched.
fn toml_set_field(
    value: &mut toml::Value,
    field: &str,
    pairs: &[(String, String)],
) -> Option<usize> {
    let parts: Vec<&str> = field.split('.').collect();
    let mut current = value;

    for part in &parts {
        match current {
            toml::Value::Table(table) => {
                if let Some(v) = table.get_mut(*part) {
                    current = v;
                } else {
                    return None;
                }
            }
            _ => return None,
        }
    }

    if let toml::Value::String(s) = current {
        if let Some((rewritten, idx)) = replace_path_prefix(s, pairs) {
            *s = rewritten;
            return Some(idx);
        }
    }

    None
}

// ── Text adapter (fallback) ──────────────────────────────────────────────────

/// Boundary-guarded string replacement in a file. Tries each candidate pair in
/// order; the first whose `old` string occurs (at a path boundary) is applied
/// to every boundary-safe occurrence. Returns the index of the pair applied.
///
/// Unlike the structured adapters we have no field to scope to, but the
/// boundary guard gives the same substring-collision protection: an incidental
/// `/home/me/code-backup` in a chat-history body is never corrupted by a
/// `/home/me/code` rewrite.
fn rewrite_text(path: &Path, pairs: &[(String, String)]) -> Result<Option<usize>> {
    let Some(content) = read_artifact(path)? else {
        return Ok(None);
    };
    for (i, (old_str, new_str)) in pairs.iter().enumerate() {
        if let Some(updated) = replace_all_with_boundary(&content, old_str, new_str) {
            atomic_write(path, &updated)?;
            return Ok(Some(i));
        }
    }
    Ok(None)
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
    fn atomic_write_replaces_content_and_leaves_no_temp() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("settings.json");
        fs::write(&file, "OLD CONTENT").unwrap();

        atomic_write(&file, "NEW CONTENT").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "NEW CONTENT");

        // No `.sg-tmp-*` sibling left behind after the rename.
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("sg-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file was not cleaned up");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("cfg.toml");
        fs::write(&file, "a").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();

        atomic_write(&file, "b").unwrap();

        let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o640,
            "atomic_write should preserve the original mode"
        );
    }

    #[test]
    fn rewrite_text_replaces_paths() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.txt");
        fs::write(&file, "path = /old/project").unwrap();

        let changed = rewrite_text(&file, &one_pair("/old/project", "/new/project")).unwrap();
        assert!(changed.is_some());

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
        assert!(changed.is_none());
    }

    #[test]
    fn text_adapter_boundary_guard_protects_sibling_paths() {
        // The aider chat-history case: prose mentions both the project path
        // and a sibling that shares it as a prefix. Only the true path (and
        // its sub-paths) may be rewritten.
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("history.md");
        fs::write(
            &file,
            "worked in /home/me/code today\n\
             backup lives at /home/me/code-backup/notes\n\
             edited /home/me/code/src/main.rs\n",
        )
        .unwrap();

        let changed = rewrite_text(&file, &one_pair("/home/me/code", "/new/root")).unwrap();
        assert!(changed.is_some());

        let content = fs::read_to_string(&file).unwrap();
        assert!(content.contains("worked in /new/root today"));
        assert!(content.contains("edited /new/root/src/main.rs"));
        assert!(
            content.contains("/home/me/code-backup/notes"),
            "sibling prefix must be untouched, got: {content}"
        );
    }

    #[test]
    fn non_utf8_artifact_is_skipped_not_errored() {
        // One binary-ish file must not abort a tool's reconciliation.
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("blob.txt");
        std::fs::write(&file, [0xFF, 0xFE, 0x2F, 0x6F, 0x6C, 0x64]).unwrap();

        let changed = rewrite_text(&file, &one_pair("/old", "/new")).unwrap();
        assert!(changed.is_none(), "non-UTF8 should skip, not rewrite");
    }

    #[test]
    fn rewrite_field_reports_the_matched_pair_index() {
        // The value carries the SHORT (/var) form; pairs[0] is the /private
        // long form (no match), pairs[1] the short form (match). The returned
        // index must be 1 so the event log records what was really written —
        // otherwise undo looks for strings that aren't in the file.
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("settings.json");
        fs::write(&file, r#"{"project_path": "/var/folders/xx/proj"}"#).unwrap();

        let pairs = vec![
            (
                "/private/var/folders/xx/proj".to_string(),
                "/private/var/folders/yy/proj".to_string(),
            ),
            (
                "/var/folders/xx/proj".to_string(),
                "/var/folders/yy/proj".to_string(),
            ),
        ];
        let spec = PathFieldSpec {
            file: String::new(),
            field: "project_path".into(),
            format: "json".into(),
        };
        let matched = rewrite_field(&file, &spec, &pairs).unwrap();
        assert_eq!(matched, Some(1), "must report the pair actually applied");
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
        assert!(changed.is_some());

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
        assert!(changed.is_some());

        let content = fs::read_to_string(&file).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["cache"]["dir"], "/new/root/.cache");
        assert_eq!(v["name"], "test");
    }

    #[test]
    fn replace_path_prefix_exact_match() {
        assert_eq!(
            replace_path_prefix("/home/me/code", &one_pair("/home/me/code", "/new/root")),
            Some(("/new/root".to_string(), 0))
        );
    }

    #[test]
    fn replace_path_prefix_with_trailing_segment() {
        assert_eq!(
            replace_path_prefix(
                "/home/me/code/src/main.rs",
                &one_pair("/home/me/code", "/new/root")
            ),
            Some(("/new/root/src/main.rs".to_string(), 0))
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
            Some((r"D:\new\src".to_string(), 0))
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
            Some(("/var/folders/yy/foo".to_string(), 1))
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
        assert!(
            changed.is_none(),
            "sibling path must not be treated as a prefix"
        );

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
        assert!(changed.is_none());
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
        assert!(changed.is_some());

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
        assert!(changed.is_some());

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
