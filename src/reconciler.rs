//! Path reconciliation engine.
//!
//! When a project directory moves, the reconciler rewrites internal
//! path references in AI tool session artifacts so tools can pick up
//! where they left off.

use std::path::Path;

use tracing::{debug, info, warn};

use crate::error::Result;
use crate::event_log::{EventLog, ReconcileAction};
use crate::tools::{ReconcileStrategy, ToolDefinition};

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

        match rewrite_file(&artifact_path, &old_root_str, &new_root_str) {
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

/// Simple string replacement in a file. Returns true if any changes were made.
fn rewrite_file(path: &Path, old_str: &str, new_str: &str) -> Result<bool> {
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
    use tempfile::TempDir;

    #[test]
    fn rewrite_file_replaces_paths() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.json");
        std::fs::write(&file, r#"{"root": "/old/project"}"#).unwrap();

        let changed = rewrite_file(&file, "/old/project", "/new/project").unwrap();
        assert!(changed);

        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.contains("/new/project"));
        assert!(!content.contains("/old/project"));
    }

    #[test]
    fn rewrite_file_no_change_returns_false() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.json");
        std::fs::write(&file, r#"{"root": "/some/other/path"}"#).unwrap();

        let changed = rewrite_file(&file, "/old/project", "/new/project").unwrap();
        assert!(!changed);
    }
}
