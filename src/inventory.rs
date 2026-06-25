// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Tool data-dir inventory.
//!
//! Step 2 of the v0.4 `migrate` implementation order (see
//! `docs/history/migrate.md`). For every tool with a `home_dir_layout`
//! declared, report:
//!
//! - the **resolved location** on the current machine (after
//!   `~`-expansion and any platform overrides),
//! - the **total size on disk** of that location,
//! - the **last-modified timestamp** anywhere inside it.
//!
//! Pure read-only. No subprocess, no SQLite, no filesystem mutations.
//! Cap walks at [`WALK_FILE_CAP`] files to keep `inventory` fast on
//! pathologically large stores (e.g. a multi-GB Codex history with
//! tens of thousands of small JSONL files).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::tools::ToolDefinition;

/// Soft cap on number of files walked per tool. Beyond this we stop
/// recursing and mark `truncated = true` so the operator knows the
/// numbers are a floor, not exact.
pub const WALK_FILE_CAP: usize = 200_000;

/// Inventory entry for one tool with a `home_dir_layout` declared.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryEntry {
    /// Tool's machine name (e.g. `codex`, `opencode`).
    pub tool_name: String,
    /// Tool's display name.
    pub display_name: String,
    /// Resolved on-disk location (after `~` expansion). Even when the
    /// path does not currently exist on this machine — operators sometimes
    /// inventory before they've installed a tool.
    pub path: PathBuf,
    /// Whether the path currently exists on disk.
    pub exists: bool,
    /// Total bytes summed across every regular file under `path`.
    /// Zero if the path does not exist or is unreadable.
    pub size_bytes: u64,
    /// Total number of regular files walked. Capped at [`WALK_FILE_CAP`].
    pub file_count: usize,
    /// Most-recent `mtime` across every regular file (Unix epoch seconds).
    /// `None` if no files were walked.
    pub last_modified: Option<u64>,
    /// `true` when the walk hit [`WALK_FILE_CAP`] before completing —
    /// reported numbers are a floor.
    pub truncated: bool,
    /// Anything that prevented a clean inventory (permissions, broken
    /// symlinks, etc.). Empty when the walk completed cleanly.
    pub notes: Vec<String>,
}

/// Inventory every tool in `tools` that declares a `home_dir_layout`.
/// Tools without one are skipped silently — they're "in-project only"
/// in the v0.3.x sense and `migrate` doesn't apply.
pub fn inventory_tools(
    tools: impl IntoIterator<Item = &'static ToolDefinition>,
) -> Vec<InventoryEntry>
where
    // We don't actually need 'static — making this generic so callers
    // can pass either `&[T]` or a registry iterator without lifetime
    // dances. Kept fn-style for parameter clarity.
{
    inventory_tools_impl(tools)
}

/// Generic-lifetime version for callers that pass borrowed iterators.
pub fn inventory_tools_impl<'a, I>(tools: I) -> Vec<InventoryEntry>
where
    I: IntoIterator<Item = &'a ToolDefinition>,
{
    let mut out = Vec::new();
    for tool in tools {
        let Some(layout) = tool.home_dir_layout.as_ref() else {
            continue;
        };
        let path = expand_home(&layout.default_path);
        let mut entry = InventoryEntry {
            tool_name: tool.name.clone(),
            display_name: tool.display_name.clone(),
            path: path.clone(),
            exists: path.exists(),
            size_bytes: 0,
            file_count: 0,
            last_modified: None,
            truncated: false,
            notes: Vec::new(),
        };
        if entry.exists {
            walk_into(&path, &mut entry);
        }
        out.push(entry);
    }
    out
}

/// Replace a leading `~` with the user's home directory. Other `~`
/// occurrences inside the string are left as-is. Returns the input
/// unchanged on unusual platforms with no resolvable home.
pub fn expand_home(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    } else if s == "~" {
        if let Some(home) = home_dir() {
            return home;
        }
    }
    PathBuf::from(s)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Walk `path`, populating size / count / mtime fields on `entry`.
///
/// Bounded by [`WALK_FILE_CAP`] to keep inventory snappy on huge
/// stores. The walk is iterative (a manual stack) rather than
/// recursive to avoid blowing the call stack on deep trees.
fn walk_into(path: &Path, entry: &mut InventoryEntry) {
    let mut stack: Vec<PathBuf> = vec![path.to_path_buf()];
    let mut max_mtime: Option<SystemTime> = None;

    while let Some(dir) = stack.pop() {
        if entry.file_count >= WALK_FILE_CAP {
            entry.truncated = true;
            break;
        }
        let read = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                entry
                    .notes
                    .push(format!("read_dir failed on {}: {e}", dir.display()));
                continue;
            }
        };
        for child in read {
            let child = match child {
                Ok(c) => c,
                Err(e) => {
                    entry.notes.push(format!("entry error: {e}"));
                    continue;
                }
            };
            let p = child.path();
            let meta = match child.metadata() {
                Ok(m) => m,
                Err(e) => {
                    entry
                        .notes
                        .push(format!("stat failed on {}: {e}", p.display()));
                    continue;
                }
            };
            if meta.is_dir() {
                stack.push(p);
            } else if meta.is_file() {
                entry.size_bytes = entry.size_bytes.saturating_add(meta.len());
                entry.file_count = entry.file_count.saturating_add(1);
                if let Ok(mtime) = meta.modified() {
                    max_mtime = Some(match max_mtime {
                        None => mtime,
                        Some(prev) if mtime > prev => mtime,
                        Some(prev) => prev,
                    });
                }
                if entry.file_count >= WALK_FILE_CAP {
                    entry.truncated = true;
                    break;
                }
            }
            // symlinks: skipped silently — could be cycles, could
            // be pointers outside the data tree. Don't follow.
        }
    }

    if let Some(t) = max_mtime {
        entry.last_modified = t
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{
        HomeDirDiscovery, HomeDirLayout, HomeDirQuiesce, HomeDirValidate, ReconcileStrategy,
        ToolDefinition,
    };
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    fn tool_with_layout(name: &str, default_path: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            display_name: name.to_string(),
            session_patterns: vec![],
            path_fields: vec![],
            on_move: ReconcileStrategy::Notify,
            version: None,
            binary: None,
            home_dir_layout: Some(HomeDirLayout {
                default_path: default_path.to_string(),
                discovery: HomeDirDiscovery::Symlink,
                env_var: None,
                config_files: vec![],
                quiesce: HomeDirQuiesce::default(),
                validate: HomeDirValidate::default(),
            }),
        }
    }

    #[test]
    fn skips_tools_without_layout() {
        let t = ToolDefinition {
            name: "no_layout".into(),
            display_name: "No Layout".into(),
            session_patterns: vec![],
            path_fields: vec![],
            on_move: ReconcileStrategy::Notify,
            version: None,
            binary: None,
            home_dir_layout: None,
        };
        let inv = inventory_tools_impl([&t]);
        assert!(inv.is_empty());
    }

    #[test]
    fn reports_missing_path_with_exists_false() {
        let t = tool_with_layout("ghost", "/definitely/does/not/exist/xyz");
        let inv = inventory_tools_impl([&t]);
        assert_eq!(inv.len(), 1);
        assert!(!inv[0].exists);
        assert_eq!(inv[0].size_bytes, 0);
        assert_eq!(inv[0].file_count, 0);
        assert!(inv[0].last_modified.is_none());
    }

    #[test]
    fn walks_real_dir_and_sums_sizes() {
        let dir = TempDir::new().unwrap();
        // Two files at the top level
        for (name, len) in [("a.txt", 100), ("b.bin", 250)] {
            let mut f = fs::File::create(dir.path().join(name)).unwrap();
            f.write_all(&vec![0u8; len]).unwrap();
        }
        // One nested file
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        let mut f = fs::File::create(dir.path().join("sub/c.dat")).unwrap();
        f.write_all(&[0u8; 50]).unwrap();

        let t = tool_with_layout("dirwalk", dir.path().to_str().unwrap());
        let inv = inventory_tools_impl([&t]);
        assert_eq!(inv.len(), 1);
        let e = &inv[0];
        assert!(e.exists);
        assert_eq!(e.file_count, 3);
        assert_eq!(e.size_bytes, 100 + 250 + 50);
        assert!(e.last_modified.is_some());
        assert!(!e.truncated);
        assert!(e.notes.is_empty(), "expected clean walk, got {:?}", e.notes);
    }

    #[test]
    fn expand_home_handles_tilde_prefix() {
        let p = expand_home("~/.foo");
        if let Some(home) = home_dir() {
            assert_eq!(p, home.join(".foo"));
        }
    }

    #[test]
    fn expand_home_passes_through_non_tilde() {
        assert_eq!(expand_home("/abs/path"), PathBuf::from("/abs/path"));
        assert_eq!(expand_home("rel/path"), PathBuf::from("rel/path"));
    }
}
