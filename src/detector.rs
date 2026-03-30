// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! AI tool session artifact detection.
//!
//! Scans a project directory to determine which AI coding tools have
//! session artifacts present, using the patterns from the tool registry.

use std::path::Path;

use glob::Pattern;

use crate::tools::{ToolDefinition, ToolRegistry};

/// Result of detecting AI tool artifacts in a project directory.
#[derive(Debug, Clone)]
pub struct DetectionResult {
    pub tool_name: String,
    pub display_name: String,
    pub matched_patterns: Vec<String>,
    /// Resolved paths to artifact files that contain rewritable path fields.
    pub artifact_files: Vec<std::path::PathBuf>,
}

/// Scan a project directory for AI tool session artifacts.
pub fn detect_tools(project_root: &Path, registry: &ToolRegistry) -> Vec<DetectionResult> {
    registry
        .all()
        .filter_map(|tool| detect_single_tool(project_root, tool))
        .collect()
}

/// Check one tool's patterns against a project directory.
///
/// Uses a two-phase match: first checks for literal path existence
/// (e.g., `.claude/` directory), then falls back to glob expansion
/// for wildcard patterns.
fn detect_single_tool(project_root: &Path, tool: &ToolDefinition) -> Option<DetectionResult> {
    let matched: Vec<String> = tool
        .session_patterns
        .iter()
        .filter(|pattern| {
            let candidate = project_root.join(pattern.trim_end_matches('/'));
            candidate.exists() || glob_matches_any(project_root, pattern)
        })
        .cloned()
        .collect();

    if matched.is_empty() {
        None
    } else {
        // Collect actual artifact file paths from path_fields
        let artifact_files: Vec<std::path::PathBuf> = tool
            .path_fields
            .iter()
            .map(|pf| project_root.join(&pf.file))
            .filter(|p| p.exists())
            .collect();

        Some(DetectionResult {
            tool_name: tool.name.clone(),
            display_name: tool.display_name.clone(),
            matched_patterns: matched,
            artifact_files,
        })
    }
}

fn glob_matches_any(root: &Path, pattern: &str) -> bool {
    let full_pattern = root.join(pattern).to_string_lossy().to_string();
    Pattern::new(&full_pattern)
        .ok()
        .and_then(|_| glob::glob(&full_pattern).ok())
        .is_some_and(|mut entries| entries.next().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;
    use tempfile::TempDir;

    #[test]
    fn detects_claude_code_artifacts() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".claude")).unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "# test").unwrap();

        let registry = ToolRegistry::new().unwrap();
        let results = detect_tools(dir.path(), &registry);

        assert!(!results.is_empty());
        assert!(results.iter().any(|r| r.tool_name == "claude_code"));
    }

    #[test]
    fn returns_empty_for_no_artifacts() {
        let dir = TempDir::new().unwrap();
        let registry = ToolRegistry::new().unwrap();
        let results = detect_tools(dir.path(), &registry);
        assert!(results.is_empty());
    }
}
