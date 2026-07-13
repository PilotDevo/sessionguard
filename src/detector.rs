// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! AI tool session artifact detection.
//!
//! Scans a project directory to determine which AI coding tools have
//! session artifacts present, using the patterns from the tool registry.

use std::path::{Path, PathBuf};

use crate::tools::{ToolDefinition, ToolRegistry};

/// Upper bound on directories visited by [`walk_for_projects`], so a
/// pathological tree can't spin forever or blow memory.
const WALK_DIR_CAP: usize = 50_000;

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

/// Recursively discover project directories under `root`, up to `max_depth`
/// levels deep. A directory that contains AI-tool artifacts is a project; we
/// record it and do NOT descend into it (a project's subdirectories are part of
/// that project, not separate ones). Skips version-control/dependency/build
/// dirs and other dot-directories, and stops after [`WALK_DIR_CAP`] directories.
///
/// Returns whether the walk was truncated by the cap, plus the projects found.
pub fn walk_for_projects(
    root: &Path,
    max_depth: usize,
    registry: &ToolRegistry,
) -> (Vec<PathBuf>, bool) {
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut visited = 0usize;

    while let Some((dir, depth)) = stack.pop() {
        visited += 1;
        if visited > WALK_DIR_CAP {
            return (found, true);
        }
        if !detect_tools(&dir, registry).is_empty() {
            found.push(dir);
            continue; // prune: don't descend into a detected project
        }
        if depth >= max_depth {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() && !is_skippable_dir(&p) {
                    stack.push((p, depth + 1));
                }
            }
        }
    }
    (found, false)
}

/// Directories we never descend into during discovery: VCS, dependency, and
/// build trees, plus any dot-directory (session artifacts live *inside* a
/// project dir, which we detect at the project level, so we needn't recurse
/// into hidden dirs to find projects).
fn is_skippable_dir(p: &Path) -> bool {
    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
        return true;
    };
    matches!(
        name,
        "node_modules" | "target" | "vendor" | "__pycache__" | "dist" | "build"
    ) || name.starts_with('.')
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
    // `glob::glob` returns an iterator on success; we only need to know whether
    // any entry matches. An invalid pattern means "no match" — not an error here.
    glob::glob(&full_pattern)
        .ok()
        .is_some_and(|mut entries| entries.next().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;
    use tempfile::TempDir;

    #[test]
    fn walk_for_projects_finds_nested_prunes_and_respects_depth() {
        let root = TempDir::new().unwrap();
        let reg = ToolRegistry::new().unwrap();

        // A project nested 3 deep, with a sub-subdir that also has a .claude
        // (must NOT be reported separately — pruned as part of the project).
        let proj = root.path().join("work/client/proj");
        std::fs::create_dir_all(proj.join(".claude")).unwrap();
        std::fs::write(proj.join(".claude/settings.json"), "{}").unwrap();
        std::fs::create_dir_all(proj.join("sub/.claude")).unwrap();
        std::fs::write(proj.join("sub/.claude/settings.json"), "{}").unwrap();
        // A skippable dir (node_modules) containing a decoy artifact — ignored.
        std::fs::create_dir_all(root.path().join("node_modules/.claude")).unwrap();

        let (found, truncated) = walk_for_projects(root.path(), 5, &reg);
        assert!(!truncated);
        assert_eq!(
            found,
            vec![proj.clone()],
            "should find exactly the one project, pruned"
        );

        // Too-shallow depth can't reach a project 3 levels down.
        let (shallow, _) = walk_for_projects(root.path(), 1, &reg);
        assert!(
            shallow.is_empty(),
            "depth 1 shouldn't reach a depth-3 project"
        );
    }

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
