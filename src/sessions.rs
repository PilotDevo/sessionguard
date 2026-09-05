// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Per-project session census across the tools' home-directory stores.
//!
//! Where `inventory` reports store-level totals ("codex: 2.5 GB"), this module
//! flips the axis to the question operators actually ask: *"for each PROJECT,
//! which assistants have sessions, how many, and how fresh?"* — including
//! sessions whose project directory no longer exists (**orphans**), which is
//! the first signal for cleaning up or archiving stale session data.
//!
//! Store layouts are *declarations*, not hardcoded readers: each tool's
//! `[tool.session_store]` (see [`crate::tools::SessionStore`]) names one of
//! three data-bound shapes, and [`census`] dispatches to the matching reader
//! with that declaration's bindings. Adding or repointing a store is a TOML
//! change, not a recompile:
//! - **`encoded_dir`** (Claude Code): one directory per project; the
//!   directory name is the project path with every non-alphanumeric
//!   character collapsed into `separator` — lossy, so the declaration can
//!   name a *key hint* (a file inside each directory that records the literal
//!   path) and the name is decoded, encoding-aware, against the real
//!   filesystem only as a fallback.
//! - **`jsonl_field`** (Codex): a tree of JSONL files; the project path is a
//!   field (optionally dotted, e.g. `payload.cwd`) on the first line.
//! - **`sqlite_column`** (OpenCode): rows in a SQLite table, opened
//!   read-only; a column carries the project dir and an optional column
//!   flags archived rows.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::tools::{safe_ident, HomeDirDiscovery, SessionStore, TimeUnit, ToolDefinition};

/// Cap on directory entries visited per store walk, so a pathological store
/// can't spin. Counted against entries *visited*, not matches *yielded* — a
/// cap on matches bounds nothing when the walk itself is what runs away.
const SESSION_WALK_CAP: usize = 50_000;

/// How confidently a store entry was resolved to a real project path.
/// Ordered most-confident first so `min` keeps the best decode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecodeConfidence {
    Exact,
    Inferred,
    /// Also the `Default`: a payload that omits `confidence` entirely has told
    /// us nothing about how it resolved the path, and "unknown" must not
    /// masquerade as `Exact`.
    #[default]
    Unresolved,
}

/// One tool's sessions for one project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSessions {
    /// Number of session files (Claude/Codex) or session rows (OpenCode).
    pub count: usize,
    /// Total bytes of the session files (0 for OpenCode — its sessions share
    /// one database file, so per-session sizes aren't attributable).
    pub bytes: u64,
    /// Most recent activity, unix seconds.
    pub last_active_unix: u64,
}

/// All known sessions for one project directory, across tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGroup {
    /// The project directory the sessions belong to. At `Inferred`
    /// confidence this is a best-effort naive decode rather than a
    /// filesystem-validated path; at `Unresolved` it is the raw encoded
    /// store name (still listed rather than hidden). See
    /// [`DecodeConfidence`].
    pub project_path: String,
    /// Whether `project_path` was confidently resolved to a real path form.
    ///
    /// Retained for one release for pre-`confidence` consumers and scheduled
    /// for removal; `#[serde(default)]` (like `confidence` and `host`) so an
    /// OLDER local binary can still deserialize a NEWER remote host's payload
    /// once the field is dropped — the fleet compat shim only upgrades
    /// payloads in the older-remote direction.
    #[serde(default)]
    pub decoded: bool,
    /// How confidently `project_path` was resolved (see [`DecodeConfidence`]).
    #[serde(default)]
    pub confidence: DecodeConfidence,
    /// True when the project directory no longer exists on disk — the
    /// sessions are orphaned (candidates for archive/cleanup). Always `false`
    /// for a census of a foreign root (see [`census`]): the project
    /// directories belong to another machine's filesystem, so their existence
    /// is not decidable here.
    pub orphaned: bool,
    /// The host this census was taken on: `"local"` for this machine, or a
    /// configured [`crate::config::HostSpec`] name when populated by
    /// [`crate::fleet::remote_census`].
    #[serde(default)]
    pub host: String,
    /// Per-tool session summaries, keyed by tool name.
    pub tools: BTreeMap<String, ToolSessions>,
}

/// Expand a leading `~` against `home`. Declarations use `~` so the same TOML
/// works for any user and any census root (including a mounted remote home).
fn expand(home: &Path, raw: &str) -> PathBuf {
    match raw.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if raw == "~" => home.to_path_buf(),
        None => PathBuf::from(raw),
    }
}

/// An environment lookup (`std::env::var(...).ok()` in production, a closure
/// over a fixed map in tests) — see [`resolve_stores`].
pub type EnvLookup<'e> = &'e dyn Fn(&str) -> Option<String>;

/// Build the `(tool, store)` list [`census`] consumes from loaded tool
/// definitions, resolving each store path the way the TOOL itself would.
///
/// A tool whose `home_dir_layout` is discovered through an environment
/// variable (Codex: `CODEX_HOME`) keeps its sessions under THAT directory
/// whenever the variable is set — typically after `sessionguard migrate`
/// relocated the store — so a declared `~/.codex/sessions` is re-rooted to
/// `$CODEX_HOME/sessions`. Without this the census silently read the old,
/// now-empty location while `inventory` (which honours env discovery) found
/// the real one.
///
/// `env` is injected rather than read from `std::env` so tests never touch
/// the real environment. Pass `None` for a foreign root: this machine's
/// variables say nothing about another machine's layout. Output is sorted by
/// tool name so a census is deterministic regardless of registry order.
pub fn resolve_stores<'a>(
    tools: impl Iterator<Item = &'a ToolDefinition>,
    env: Option<EnvLookup<'_>>,
) -> Vec<(String, SessionStore)> {
    let mut out = Vec::new();
    for tool in tools {
        let Some(mut store) = tool.session_store.clone() else {
            continue;
        };
        if let (Some(env), Some(layout)) = (env, tool.home_dir_layout.as_ref()) {
            if layout.discovery == HomeDirDiscovery::Env {
                let root = layout
                    .env_var
                    .as_deref()
                    .and_then(env)
                    .filter(|v| !v.trim().is_empty());
                if let Some(root) = root {
                    if let Some(rest) = store.path().strip_prefix(layout.default_path.as_str()) {
                        if rest.is_empty() || rest.starts_with('/') {
                            let rerooted = format!("{}{}", root.trim_end_matches('/'), rest);
                            tracing::debug!(
                                tool = %tool.name,
                                from = store.path(),
                                to = %rerooted,
                                "session_store re-rooted via env discovery"
                            );
                            store.set_path(rerooted);
                        }
                    }
                }
            }
        }
        out.push((tool.name.clone(), store));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Census every declared session store under `home`, grouped by project
/// directory. Read-only. `stores` pairs each tool name with the store
/// declaration to read it with (see [`resolve_stores`]).
///
/// `foreign_root` says whether `home` is something OTHER than this machine's
/// real `$HOME` — a mounted or rsync'd home from another machine
/// (`sessions --home`). Two things change for a foreign root:
///
/// - Orphan detection asks "does this project directory exist?", and the
///   only filesystem available to answer that is the local one, which is the
///   wrong filesystem for a foreign root: every path would look gone (false
///   orphans), and a path that happens to exist locally under the same name
///   would look live while actually being a *different* project on this
///   machine. So no orphan verdict is asserted at all: `orphaned` is `false`
///   everywhere and callers are expected to say so (see `main.rs`).
///   Cross-machine orphan status is what `sessions --host` / `--all-hosts`
///   exist for — there the verdict is computed on the host the sessions
///   actually live on.
/// - Encoded directory names are not validated against the local filesystem
///   either (see [`decode_encoded_name`]); a key hint still yields the exact
///   path, everything else is a naive `Inferred` decode.
pub fn census(
    home: &Path,
    stores: &[(String, SessionStore)],
    foreign_root: bool,
) -> Vec<SessionGroup> {
    let mut groups: BTreeMap<String, (DecodeConfidence, BTreeMap<String, ToolSessions>)> =
        BTreeMap::new();

    let mut absorb = |path: String, conf: DecodeConfidence, tool: &str, add: ToolSessions| {
        let entry = groups
            .entry(path)
            .or_insert_with(|| (conf, BTreeMap::new()));
        // Keep the most confident decode any source produced.
        if conf < entry.0 {
            entry.0 = conf;
        }
        let t = entry.1.entry(tool.to_string()).or_default();
        t.count += add.count;
        t.bytes += add.bytes;
        t.last_active_unix = t.last_active_unix.max(add.last_active_unix);
    };

    for (tool, store) in stores {
        match store {
            SessionStore::EncodedDir {
                path,
                separator,
                key_glob,
                key_field,
            } => read_encoded_dir(
                &expand(home, path),
                separator,
                key_glob.as_deref(),
                key_field.as_deref(),
                !foreign_root,
                tool,
                &mut absorb,
            ),
            SessionStore::JsonlField {
                path,
                glob,
                key_field,
                fallback_field,
            } => read_jsonl_field(
                &expand(home, path),
                glob,
                key_field,
                fallback_field.as_deref(),
                tool,
                &mut absorb,
            ),
            SessionStore::SqliteColumn {
                path,
                table,
                path_column,
                updated_column,
                updated_unit,
                archived_column,
            } => read_sqlite_column(
                &expand(home, path),
                table,
                path_column,
                updated_column.as_deref(),
                *updated_unit,
                archived_column.as_deref(),
                tool,
                &mut absorb,
            ),
        }
    }

    groups
        .into_iter()
        .map(|(project_path, (confidence, tools))| {
            // Only meaningful for a census of the local machine's own home:
            // for a foreign root the local filesystem cannot answer the
            // question, so no verdict is asserted.
            let exists = foreign_root || Path::new(&project_path).exists();
            SessionGroup {
                orphaned: matches!(
                    confidence,
                    DecodeConfidence::Exact | DecodeConfidence::Inferred
                ) && !exists,
                decoded: confidence == DecodeConfidence::Exact,
                project_path,
                confidence,
                host: "local".to_string(),
                tools,
            }
        })
        .collect()
}

/// Claude Code's store-name encoding: every character outside `[A-Za-z0-9]`
/// becomes `separator` (one per UTF-16 unit, matching the JS
/// `replace(/[^a-zA-Z0-9]/g, "-")` the tool applies). Used both to validate
/// a key-hint path against the directory it was read from and to recognise,
/// during name decoding, a real directory whose name only *encodes* to the
/// next segment (`my_app` → `my-app`).
pub fn encode_project_path(path: &str, separator: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            for _ in 0..c.len_utf16() {
                out.push_str(separator);
            }
        }
    }
    out
}

/// Decode an encoded project directory name (e.g.
/// `-Users-devo-Droco-side-projects-ai-session-track`) into a filesystem
/// path, validating against this machine's filesystem. Convenience wrapper
/// over [`decode_encoded_name`].
pub fn decode_claude_project_dir(name: &str, separator: &str) -> (String, DecodeConfidence) {
    decode_encoded_name(name, separator, true)
}

/// Decode an encoded store-directory name into a project path.
///
/// The encoding collapses `/` AND every other non-alphanumeric character into
/// `separator`, so a name alone is ambiguous: `-work-my-app` is `/work/my-app`
/// or `/work/my_app` or `/work/my/app`. When `local_fs` is true the name is
/// decoded against the real filesystem, in three steps:
///
/// 1. **DFS with encoding-aware matching.** At each level the candidates for
///    the next segment are the literal join of the first `k` remaining parts
///    (`my-app`) *and* any real child directory whose own encoding equals
///    such a join (`my_app`, `app.v2`, `LARS Docs`, `.config` → `-config`).
///    Fewer parts per segment (more path separators) are tried first. If
///    exactly one split consumes the whole name the path is `Exact`; if more
///    than one does (`my_app` and `my-app` both live under `work/`) the first
///    is proposed at `Inferred` — it exists, so it is not an orphan, but the
///    name cannot say which one the sessions belong to.
/// 2. **Fold onto the deepest surviving ancestor.** If no split consumes the
///    whole name the project directory itself is most likely gone: find the
///    deepest ancestor the DFS did validate and fold everything after it into
///    ONE final segment rejoined with `separator` (ancestor
///    `/home/devo/projects` + leftover `["amarillo", "project"]` →
///    `/home/devo/projects/amarillo-project`). Reported `Inferred`: exactly
///    right for the common shape (one deleted leaf under a surviving parent)
///    but a best guess when an ancestor is also gone, when the leaf's own
///    name had a `_`/`.`/space (unrecoverable without the directory), or when
///    a live *decoy sibling* whose name is a prefix of the deleted one's
///    (`…/amarillo` beside deleted `…/amarillo-project`) consumes a part.
/// 3. **Naive split.** If not even the first part anchors to a real
///    directory, propose the plain `separator` → `/` decode, `Inferred`.
///
/// When `local_fs` is false (a foreign root: the store came from another
/// machine) steps 1–2 would validate against the WRONG filesystem, so only
/// step 3 runs. A name not starting with `separator` at all is `Unresolved`:
/// nothing path-shaped to propose, so the raw name is returned unchanged.
///
/// The key hint on [`SessionStore::EncodedDir`] avoids all of this whenever a
/// file inside the directory records the literal path; this is the fallback.
pub fn decode_encoded_name(
    name: &str,
    separator: &str,
    local_fs: bool,
) -> (String, DecodeConfidence) {
    if separator.is_empty() {
        // Rejected at load time by `SessionStore::validate`; never split on it.
        return (name.to_string(), DecodeConfidence::Unresolved);
    }
    let Some(rest) = name.strip_prefix(separator) else {
        return (name.to_string(), DecodeConfidence::Unresolved);
    };
    let parts: Vec<&str> = rest.split(separator).collect();
    let naive = || format!("/{}", parts.join("/"));
    if !local_fs {
        return (naive(), DecodeConfidence::Inferred);
    }

    let mut full = Vec::new();
    walk(Path::new("/"), &parts, separator, &mut full);
    if let Some(first) = full.first() {
        let confidence = if full.len() == 1 {
            DecodeConfidence::Exact
        } else {
            DecodeConfidence::Inferred
        };
        return (first.display().to_string(), confidence);
    }

    let (ancestor, consumed) = deepest_match(Path::new("/"), &parts, separator);
    if consumed == 0 {
        return (naive(), DecodeConfidence::Inferred);
    }
    let leaf = parts[consumed..].join(separator);
    (
        ancestor.join(leaf).display().to_string(),
        DecodeConfidence::Inferred,
    )
}

/// Candidate children of `base` for the next segment(s) of `remaining`, each
/// with the number of parts it consumes, fewest first — see
/// [`decode_encoded_name`] step 1.
fn children_matching(base: &Path, remaining: &[&str], sep: &str) -> Vec<(PathBuf, usize)> {
    let mut out: Vec<(PathBuf, usize)> = Vec::new();
    for k in 1..=remaining.len() {
        let segment = remaining[..k].join(sep);
        if segment.is_empty() {
            // A leading empty part (from a doubled separator, e.g. `--config`)
            // is never a segment on its own — `base.join("")` is `base`.
            continue;
        }
        let literal = base.join(&segment);
        if literal.is_dir() {
            out.push((literal, k));
        }
    }
    if let Ok(entries) = std::fs::read_dir(base) {
        for entry in entries.flatten() {
            let child = entry.file_name();
            let child = child.to_string_lossy();
            let enc = encode_project_path(&child, sep);
            if enc == child {
                continue; // covered by the literal join above
            }
            let enc_parts: Vec<&str> = enc.split(sep).collect();
            let k = enc_parts.len();
            if k <= remaining.len() && enc_parts[..] == remaining[..k] && entry.path().is_dir() {
                out.push((entry.path(), k));
            }
        }
    }
    out.sort_by_key(|(_, k)| *k);
    out.dedup();
    out
}

/// Collect splits that consume ALL of `remaining` — at most two, since one is
/// enough for `Exact` and a second is enough to know the name is ambiguous.
fn walk(base: &Path, remaining: &[&str], sep: &str, found: &mut Vec<PathBuf>) {
    if found.len() >= 2 {
        return;
    }
    if remaining.is_empty() {
        found.push(base.to_path_buf());
        return;
    }
    for (candidate, k) in children_matching(base, remaining, sep) {
        walk(&candidate, &remaining[k..], sep, found);
        if found.len() >= 2 {
            return;
        }
    }
}

/// Deepest ancestor any explored branch validated, with the number of parts
/// consumed to reach it — see [`decode_encoded_name`] step 2.
fn deepest_match(base: &Path, remaining: &[&str], sep: &str) -> (PathBuf, usize) {
    let mut best = (base.to_path_buf(), 0usize);
    for (candidate, k) in children_matching(base, remaining, sep) {
        let (deeper_base, deeper_count) = deepest_match(&candidate, &remaining[k..], sep);
        let total = k + deeper_count;
        if total > best.1 {
            best = (deeper_base, total);
        }
    }
    best
}

/// Files consulted per directory for the key hint, newest first. One is the
/// norm (every transcript in a directory records the same `cwd`); the extra
/// budget covers a truncated or hint-less newest file.
const HINT_FILES: usize = 4;
/// Lines / bytes scanned per hinted file. Measured on real Claude Code
/// stores: `cwd` appears within the first 22 lines and 200 KiB (usually
/// line 3–7, under 10 KiB) — the lines before it are summaries or file
/// snapshots.
const HINT_LINES: usize = 64;
const HINT_BYTES: u64 = 512 * 1024;
/// A `jsonl_field` store keys on its FIRST line (Codex `session_meta`); the
/// read is bounded so a pathological first line cannot buffer a whole
/// transcript into memory.
const FIRST_LINE_BYTES: u64 = 64 * 1024;

/// `encoded_dir` layout: one subdir per project under `base`; each file
/// inside is session state (transcripts, todos, …). With a key hint, the
/// project path is read from the newest matching file that records it — and
/// cross-checked: its encoding must reproduce the directory name, so a stray
/// file cannot re-key a directory. Otherwise, or when no file carries the
/// field, the directory name is decoded per [`decode_encoded_name`].
#[allow(clippy::too_many_arguments)]
fn read_encoded_dir(
    base: &Path,
    separator: &str,
    key_glob: Option<&str>,
    key_field: Option<&str>,
    local_fs: bool,
    tool: &str,
    absorb: &mut impl FnMut(String, DecodeConfidence, &str, ToolSessions),
) {
    let hint = match (key_glob, key_field) {
        (Some(g), Some(f)) => match glob::Pattern::new(g) {
            Ok(p) => Some((p, f)),
            Err(e) => {
                tracing::warn!(
                    tool,
                    key_glob = g,
                    %e,
                    "invalid session_store key_glob; decoding directory names only"
                );
                None
            }
        },
        _ => None,
    };
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();

        let mut count = 0usize;
        let mut bytes = 0u64;
        let mut last = 0u64;
        let mut hint_files: Vec<(u64, PathBuf)> = Vec::new();
        if let Ok(files) = std::fs::read_dir(&dir) {
            for f in files.flatten() {
                let Ok(meta) = f.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                count += 1;
                bytes += meta.len();
                let mtime = unix_mtime(&meta);
                last = last.max(mtime);
                if let Some((pat, _)) = &hint {
                    if pat.matches(&f.file_name().to_string_lossy()) {
                        hint_files.push((mtime, f.path()));
                    }
                }
            }
        }
        if count == 0 {
            continue;
        }

        let mut hinted = None;
        if let Some((_, field)) = &hint {
            hint_files.sort_by(|a, b| b.0.cmp(&a.0));
            for (_, p) in hint_files.iter().take(HINT_FILES) {
                if let Some(cwd) = jsonl_find_field(p, &[field], HINT_LINES, HINT_BYTES) {
                    if hint_consistent(&cwd, &name, separator) {
                        hinted = Some(cwd);
                        break;
                    }
                    tracing::debug!(
                        tool,
                        dir = %name,
                        cwd = %cwd,
                        "key hint does not encode to its directory name; ignoring"
                    );
                }
            }
        }
        let (path, confidence) = match hinted {
            Some(cwd) => (cwd, DecodeConfidence::Exact),
            None => decode_encoded_name(&name, separator, local_fs),
        };
        absorb(
            path,
            confidence,
            tool,
            ToolSessions {
                count,
                bytes,
                last_active_unix: last,
            },
        );
    }
}

/// A hinted path is accepted only if it could have produced the directory it
/// was read from — under the full encoding or the older `/`-only one.
fn hint_consistent(cwd: &str, dir_name: &str, separator: &str) -> bool {
    !cwd.is_empty()
        && (encode_project_path(cwd, separator) == dir_name
            || cwd.replace('/', separator) == dir_name)
}

/// `jsonl_field` layout: a tree of JSONL files under `base` matching
/// `pattern`; `key_field` (optionally dotted, e.g. `payload.cwd`) on the
/// first line of each file carries the project path, with `fallback_field`
/// tried when `key_field` is absent (older/newer schema variants).
///
/// The tree is walked here rather than by `glob::glob` — `glob`'s own
/// recursion stats candidate directories with `fs::metadata`, which FOLLOWS
/// symlinks, so a symlink cycle under a store recurses without bound. This
/// walk uses [`std::fs::DirEntry::metadata`], which does NOT follow symlinks:
/// a symlinked entry is neither `is_dir` nor `is_file`, so it is skipped and
/// a cycle is structurally impossible. `pattern` is then applied to each
/// visited file's path RELATIVE to the store root as a [`glob::Pattern`],
/// preserving the declaration's glob semantics: splicing the root into the
/// pattern instead would let `[`, `*`, `?` in `$HOME`, `--home` or a declared
/// path act as metacharacters (or fail to parse), and a trailing `/` on the
/// declaration would produce `//` and match nothing — every case a silent
/// zero. Entries visited are counted against [`SESSION_WALK_CAP`] — the bound
/// has to be on the walk, since a runaway walk yields no matches to count —
/// and hitting it is reported, since an incomplete census must not look like
/// a small one.
fn read_jsonl_field(
    base: &Path,
    pattern: &str,
    key_field: &str,
    fallback_field: Option<&str>,
    tool: &str,
    absorb: &mut impl FnMut(String, DecodeConfidence, &str, ToolSessions),
) {
    if !base.is_dir() {
        return;
    }
    let matcher = match glob::Pattern::new(pattern) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(tool, glob = pattern, %e, "invalid session_store glob; skipping store");
            return;
        }
    };
    let mut fields = vec![key_field];
    fields.extend(fallback_field);

    let mut visited = 0usize;
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > SESSION_WALK_CAP {
                tracing::warn!(
                    tool,
                    store = %base.display(),
                    cap = SESSION_WALK_CAP,
                    "session store walk hit its entry cap; this store's census is incomplete"
                );
                return;
            }
            // Does not follow symlinks — see the note above.
            let Ok(meta) = entry.metadata() else { continue };
            let p = entry.path();
            if meta.is_dir() {
                stack.push(p);
                continue;
            }
            let rel = p.strip_prefix(base).unwrap_or(&p);
            if !meta.is_file() || !matcher.matches_path(rel) {
                continue;
            }
            let Some(cwd) = jsonl_find_field(&p, &fields, 1, FIRST_LINE_BYTES) else {
                continue;
            };
            absorb(
                cwd,
                DecodeConfidence::Exact,
                tool,
                ToolSessions {
                    count: 1,
                    bytes: meta.len(),
                    last_active_unix: unix_mtime(&meta),
                },
            );
        }
    }
}

/// Scan up to `max_lines` leading lines of a JSONL file — reading at most
/// `max_bytes` in total — for the first JSON object carrying one of `fields`
/// (each optionally dotted, tried in order per line) as a string. Lines that
/// are not JSON objects (or were cut by the byte budget) are skipped.
fn jsonl_find_field(
    path: &Path,
    fields: &[&str],
    max_lines: usize,
    max_bytes: u64,
) -> Option<String> {
    use std::io::{BufRead, BufReader, Read};
    let f = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(f.take(max_bytes));
    let mut line = String::new();
    for _ in 0..max_lines {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if let Some(found) = fields.iter().find_map(|f| json_field(&v, f)) {
            return Some(found);
        }
    }
    None
}

/// Look up a dotted field path (e.g. `payload.cwd`) inside a JSON value.
fn json_field(v: &serde_json::Value, field: &str) -> Option<String> {
    let mut cur = v;
    for part in field.split('.') {
        cur = cur.get(part)?;
    }
    cur.as_str().map(str::to_string)
}

/// Any epoch above this (year ~5138 in seconds) can only be milliseconds.
const EPOCH_MS_FLOOR: u64 = 100_000_000_000;

/// Read a "last updated" cell as an epoch number whatever its declared
/// affinity: SQLite columns are dynamically typed, so an `INTEGER` column can
/// hold `REAL` or `TEXT` rows ("1752610000", "1.75e12") that a strict `i64`
/// read would reject — and rejecting the row drops the SESSION, not just its
/// timestamp.
fn epoch_cell(v: &rusqlite::types::Value) -> u64 {
    use rusqlite::types::Value;
    let raw = match v {
        Value::Integer(i) => *i as f64,
        Value::Real(f) => *f,
        Value::Text(s) => s.trim().parse::<f64>().unwrap_or(0.0),
        Value::Null | Value::Blob(_) => 0.0,
    };
    if raw.is_finite() && raw > 0.0 {
        raw as u64
    } else {
        0
    }
}

/// `sqlite_column` layout: rows in a SQLite table at `db`, opened read-only;
/// `path_column` names the project dir, `updated_column` (in `updated_unit`)
/// tracks recency, and rows with a non-null `archived_column` are excluded.
/// Table/column names come from a TOML declaration, so they are validated
/// with [`safe_ident`] before ever being interpolated into a query — an
/// unsafe identifier is never built into SQL; when the table or the path
/// column itself is unsafe there is no safe query to run, so the whole store
/// is skipped. A seconds-declared column whose values can only be
/// milliseconds is read as milliseconds (and said so once): the alternative
/// is every session "0s ago" forever.
#[allow(clippy::too_many_arguments)]
fn read_sqlite_column(
    db: &Path,
    table: &str,
    path_column: &str,
    updated_column: Option<&str>,
    updated_unit: TimeUnit,
    archived_column: Option<&str>,
    tool: &str,
    absorb: &mut impl FnMut(String, DecodeConfidence, &str, ToolSessions),
) {
    if !db.is_file() {
        return;
    }
    if !safe_ident(table) || !safe_ident(path_column) {
        tracing::warn!(
            tool,
            "session_store declares unsafe SQL identifiers; skipping"
        );
        return;
    }
    let mut sql = format!("SELECT {path_column}");
    match updated_column {
        Some(c) if safe_ident(c) => sql.push_str(&format!(", {c}")),
        _ => sql.push_str(", NULL"),
    }
    sql.push_str(&format!(" FROM {table}"));
    if let Some(a) = archived_column.filter(|a| safe_ident(a)) {
        sql.push_str(&format!(" WHERE {a} IS NULL"));
    }

    let Ok(conn) =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return;
    };
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return;
    };
    let rows = stmt.query_map([], |row| {
        let dir: Option<String> = row.get(0)?;
        let updated: rusqlite::types::Value = row.get(1)?;
        Ok((dir, updated))
    });
    let Ok(rows) = rows else { return };
    let mut warned_ms = false;
    for row in rows.flatten() {
        let (Some(dir), updated) = row else {
            continue;
        };
        if dir.is_empty() {
            continue;
        }
        let raw = epoch_cell(&updated);
        let last_active_unix = match updated_unit {
            TimeUnit::Ms => raw / 1000,
            TimeUnit::S if raw > EPOCH_MS_FLOOR => {
                if !warned_ms {
                    tracing::warn!(
                        tool,
                        column = updated_column.unwrap_or("?"),
                        "updated_column values can only be milliseconds but updated_unit is \
                         \"s\" (the default); reading as ms — declare updated_unit = \"ms\""
                    );
                    warned_ms = true;
                }
                raw / 1000
            }
            TimeUnit::S => raw,
        };
        absorb(
            dir,
            DecodeConfidence::Exact,
            tool,
            ToolSessions {
                count: 1,
                bytes: 0,
                last_active_unix,
            },
        );
    }
}

fn unix_mtime(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// The builtin store declarations, expressed as data — mirrors what the
    /// TOMLs declare, minus Claude Code's key hint so these tests exercise
    /// NAME decoding (see `hinted_claude_store` for the hint path).
    fn test_stores() -> Vec<(String, SessionStore)> {
        vec![
            (
                "claude_code".into(),
                SessionStore::EncodedDir {
                    path: "~/.claude/projects".into(),
                    separator: "-".into(),
                    key_glob: None,
                    key_field: None,
                },
            ),
            (
                "codex".into(),
                SessionStore::JsonlField {
                    path: "~/.codex/sessions".into(),
                    glob: "**/*.jsonl".into(),
                    key_field: "cwd".into(),
                    fallback_field: Some("payload.cwd".into()),
                },
            ),
            (
                "opencode".into(),
                SessionStore::SqliteColumn {
                    path: "~/.local/share/opencode/opencode.db".into(),
                    table: "session".into(),
                    path_column: "directory".into(),
                    updated_column: Some("time_updated".into()),
                    updated_unit: TimeUnit::Ms,
                    archived_column: Some("time_archived".into()),
                },
            ),
        ]
    }

    #[test]
    fn decode_resolves_hyphenated_segments_against_filesystem() {
        // Build a real tree containing a hyphenated segment, then encode it
        // the way Claude Code does and confirm the DFS decoder recovers it.
        let root = TempDir::new().unwrap();
        let project = root.path().join("work/side-projects/myapp");
        std::fs::create_dir_all(&project).unwrap();

        let encoded = project.display().to_string().replace('/', "-");
        let (decoded, confidence) = decode_claude_project_dir(&encoded, "-");
        assert_eq!(confidence, DecodeConfidence::Exact);
        assert_eq!(decoded, project.display().to_string());
    }

    #[test]
    fn decode_falls_back_to_naive_decode_when_dfs_fails() {
        // No such root exists anywhere, so DFS can't validate any split — the
        // naive `separator` -> `/` decode is proposed instead, at `Inferred`
        // confidence, rather than the opaque encoded name.
        let (out, confidence) = decode_claude_project_dir("-no-such-root-anywhere-zzz", "-");
        assert_eq!(confidence, DecodeConfidence::Inferred);
        assert_eq!(out, "/no/such/root/anywhere/zzz");
    }

    #[test]
    fn decode_is_unresolved_when_name_has_no_leading_separator() {
        // Nothing path-shaped to propose at all: the raw name is returned
        // unchanged, at `Unresolved` confidence.
        let (out, confidence) = decode_claude_project_dir("not-even-path-shaped", "_");
        assert_eq!(confidence, DecodeConfidence::Unresolved);
        assert_eq!(out, "not-even-path-shaped");
    }

    #[test]
    fn decode_folds_hyphenated_leaf_onto_deepest_surviving_ancestor() {
        // Fix round 1: the naive full-substitution fallback got this wrong
        // — it can't tell a literal hyphen inside a directory name from a
        // separator hyphen, so it would split a deleted "amarillo-project"
        // leaf into "amarillo/project". The corrected fallback instead runs
        // DFS as far as the filesystem allows (here: all the way through
        // the surviving "projects" parent), then folds whatever's left
        // ("amarillo", "project") into ONE final segment rejoined with the
        // separator — recovering the hyphenated leaf name exactly.
        let root = TempDir::new().unwrap();
        let parent = root.path().join("home/devo/projects");
        std::fs::create_dir_all(&parent).unwrap();
        let gone = parent.join("amarillo-project"); // deleted: never created

        let encoded = gone.display().to_string().replace('/', "-");
        let (decoded, confidence) = decode_claude_project_dir(&encoded, "-");
        assert_eq!(confidence, DecodeConfidence::Inferred);
        assert_eq!(decoded, gone.display().to_string());
    }

    #[test]
    fn census_groups_claude_and_codex_by_project_and_flags_orphans() {
        let home = TempDir::new().unwrap();
        // A live project (exists on disk) with a Claude store dir.
        let live = home.path().join("proj/alpha");
        std::fs::create_dir_all(&live).unwrap();
        let enc = live.display().to_string().replace('/', "-");
        let store = home.path().join(".claude/projects").join(&enc);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("s1.jsonl"), b"{}").unwrap();
        std::fs::write(store.join("s2.jsonl"), b"{}").unwrap();

        // A Codex session pointing at a project dir that DOESN'T exist.
        let gone = home.path().join("proj/deleted");
        let codex = home.path().join(".codex/sessions/2026/07");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(
            codex.join("rollout-1.jsonl"),
            format!("{{\"cwd\": \"{}\"}}\n", gone.display()),
        )
        .unwrap();

        let groups = census(home.path(), &test_stores(), false);
        let live_group = groups
            .iter()
            .find(|g| g.project_path == live.display().to_string())
            .expect("live project present");
        assert!(!live_group.orphaned);
        assert!(live_group.decoded);
        assert_eq!(live_group.tools["claude_code"].count, 2);

        let orphan = groups
            .iter()
            .find(|g| g.project_path == gone.display().to_string())
            .expect("codex project present");
        assert!(
            orphan.orphaned,
            "nonexistent project dir must flag orphaned"
        );
        assert_eq!(orphan.tools["codex"].count, 1);
    }

    #[test]
    fn census_reads_opencode_db_read_only() {
        let home = TempDir::new().unwrap();
        let dbdir = home.path().join(".local/share/opencode");
        std::fs::create_dir_all(&dbdir).unwrap();
        let conn = rusqlite::Connection::open(dbdir.join("opencode.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (directory TEXT, time_updated INTEGER, time_archived INTEGER);
             INSERT INTO session VALUES ('/tmp/oc-proj', 1752600000000, NULL);
             INSERT INTO session VALUES ('/tmp/oc-proj', 1752610000000, NULL);
             INSERT INTO session VALUES ('/tmp/archived', 1752600000000, 1752600000001);",
        )
        .unwrap();
        drop(conn);

        let groups = census(home.path(), &test_stores(), false);
        let g = groups
            .iter()
            .find(|g| g.project_path == "/tmp/oc-proj")
            .expect("opencode rows surfaced");
        assert_eq!(g.tools["opencode"].count, 2, "archived rows excluded");
        assert_eq!(g.tools["opencode"].last_active_unix, 1_752_610_000);
    }

    #[test]
    fn census_is_driven_by_declarations_not_hardcoded_paths() {
        // A store declared at a NON-default path must be read; a hardcoded
        // reader would find nothing here.
        let home = TempDir::new().unwrap();
        let live = home.path().join("proj/alpha");
        std::fs::create_dir_all(&live).unwrap();
        let enc = live.display().to_string().replace('/', "-");
        let store = home.path().join("custom/store").join(&enc);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("s1.jsonl"), b"{}").unwrap();

        let stores = vec![(
            "claude_code".to_string(),
            SessionStore::EncodedDir {
                path: "~/custom/store".into(),
                separator: "-".into(),
                key_glob: None,
                key_field: None,
            },
        )];
        let groups = census(home.path(), &stores, false);
        assert_eq!(groups.len(), 1, "declared custom path must be read");
        assert_eq!(groups[0].tools["claude_code"].count, 1);
    }

    #[test]
    fn sqlite_column_skips_whole_store_when_table_or_path_column_is_unsafe() {
        // `table`/`path_column` have no safe fallback to substitute — an
        // unsafe identifier there means there is no safe query to run at
        // all, so the whole store is skipped rather than ever interpolating
        // it into SQL.
        let home = TempDir::new().unwrap();
        let dbdir = home.path().join(".local/share/opencode");
        std::fs::create_dir_all(&dbdir).unwrap();
        let conn = rusqlite::Connection::open(dbdir.join("opencode.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (directory TEXT, time_updated INTEGER, time_archived INTEGER);
             INSERT INTO session VALUES ('/tmp/oc-proj', 1752600000000, NULL);",
        )
        .unwrap();
        drop(conn);

        let unsafe_table = vec![(
            "opencode".to_string(),
            SessionStore::SqliteColumn {
                path: "~/.local/share/opencode/opencode.db".into(),
                table: "session; DROP TABLE session;--".into(),
                path_column: "directory".into(),
                updated_column: Some("time_updated".into()),
                updated_unit: TimeUnit::Ms,
                archived_column: Some("time_archived".into()),
            },
        )];
        assert!(
            census(home.path(), &unsafe_table, false).is_empty(),
            "unsafe table identifier must skip the whole store without building a query"
        );

        let unsafe_path_column = vec![(
            "opencode".to_string(),
            SessionStore::SqliteColumn {
                path: "~/.local/share/opencode/opencode.db".into(),
                table: "session".into(),
                path_column: "directory, (SELECT 1)".into(),
                updated_column: Some("time_updated".into()),
                updated_unit: TimeUnit::Ms,
                archived_column: Some("time_archived".into()),
            },
        )];
        assert!(
            census(home.path(), &unsafe_path_column, false).is_empty(),
            "unsafe path_column identifier must skip the whole store without building a query"
        );
    }

    #[test]
    fn sqlite_column_degrades_gracefully_when_optional_columns_are_unsafe() {
        // Unlike `table`/`path_column`, the optional `updated_column` and
        // `archived_column` DO have a safe fallback: they are simply never
        // interpolated (substituted with `NULL` / the `WHERE` clause is
        // dropped) rather than aborting the whole store. Prove the store
        // still reads — including the row that would have been excluded had
        // the archived-column filter actually applied.
        let home = TempDir::new().unwrap();
        let dbdir = home.path().join(".local/share/opencode");
        std::fs::create_dir_all(&dbdir).unwrap();
        let conn = rusqlite::Connection::open(dbdir.join("opencode.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (directory TEXT, time_updated INTEGER, time_archived INTEGER);
             INSERT INTO session VALUES ('/tmp/oc-proj', 1752600000000, NULL);
             INSERT INTO session VALUES ('/tmp/archived', 1752600000000, 1752600000001);",
        )
        .unwrap();
        drop(conn);

        let stores = vec![(
            "opencode".to_string(),
            SessionStore::SqliteColumn {
                path: "~/.local/share/opencode/opencode.db".into(),
                table: "session".into(),
                path_column: "directory".into(),
                updated_column: Some("time_updated; --".into()),
                updated_unit: TimeUnit::Ms,
                archived_column: Some("time_archived OR 1=1".into()),
            },
        )];

        let groups = census(home.path(), &stores, false);
        assert!(
            groups.iter().any(|g| g.project_path == "/tmp/oc-proj"),
            "safe rows must still be read when only optional columns are unsafe"
        );
        assert!(
            groups.iter().any(|g| g.project_path == "/tmp/archived"),
            "unsafe archived_column must degrade to 'no filter', not skip the store \
             (the row that would have been excluded must still surface)"
        );
        let live = groups
            .iter()
            .find(|g| g.project_path == "/tmp/oc-proj")
            .unwrap();
        assert_eq!(
            live.tools["opencode"].last_active_unix, 0,
            "unsafe updated_column must fall back to the NULL substitution, not the real column"
        );
    }

    #[test]
    fn deleted_claude_project_is_inferred_and_orphaned() {
        // Today this is impossible: decode DFS-validates against the live
        // filesystem, so a DELETED project never decodes and therefore never
        // reports as orphaned. Inferred confidence is what makes Claude Code
        // orphan detection work at all.
        //
        // Fix round 1: the task brief's literal fixture used "deleted-app"
        // as the leaf segment, but its own naive-fallback algorithm
        // (`separator` -> `/` split-then-join over the WHOLE name) can't
        // distinguish that leaf's literal hyphen from a path-separator
        // hyphen — "deleted-app" round-trips to "deleted/app", not
        // "deleted-app". The corrected algorithm fixes this generally by
        // running DFS as far as the filesystem allows and folding only the
        // truly-unvalidated tail into one segment — which requires a real
        // surviving ancestor to anchor to. So unlike the brief's literal
        // fixture, this test creates `work/` (the project's parent) without
        // creating `work/deleted-app` (the project itself, which is gone).
        let home = TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join("work")).unwrap();
        let gone = home.path().join("work/deleted-app");
        let enc = gone.display().to_string().replace('/', "-");
        let store = home.path().join(".claude/projects").join(&enc);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("s.jsonl"), b"{}").unwrap();

        let groups = census(home.path(), &test_stores(), false);
        let g = groups
            .iter()
            .find(|g| g.project_path == gone.display().to_string())
            .expect("DFS-plus-fold decode should propose the deleted path");
        assert_eq!(g.confidence, DecodeConfidence::Inferred);
        assert!(g.orphaned, "a deleted project must report as orphaned");
        assert!(!g.decoded, "compat field: only Exact counts as decoded");
    }

    #[test]
    fn deleted_claude_project_directly_under_home_is_inferred_and_orphaned() {
        // Simpler coverage alongside the hyphenated-leaf case above: no
        // intermediate parent to fold across, so this also exercises the
        // "no ancestor beyond the trivial root chain" shape without any
        // separator ambiguity in play.
        let home = TempDir::new().unwrap();
        let gone = home.path().join("deleted_project");
        let enc = gone.display().to_string().replace('/', "-");
        let store = home.path().join(".claude/projects").join(&enc);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("s.jsonl"), b"{}").unwrap();

        let groups = census(home.path(), &test_stores(), false);
        let g = groups
            .iter()
            .find(|g| g.project_path == gone.display().to_string())
            .expect("decode should propose the deleted path");
        assert_eq!(g.confidence, DecodeConfidence::Inferred);
        assert!(g.orphaned, "a deleted project must report as orphaned");
        assert!(!g.decoded, "compat field: only Exact counts as decoded");
    }

    #[test]
    fn live_project_decodes_exact() {
        let home = TempDir::new().unwrap();
        let live = home.path().join("work/app");
        std::fs::create_dir_all(&live).unwrap();
        let enc = live.display().to_string().replace('/', "-");
        let store = home.path().join(".claude/projects").join(&enc);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("s.jsonl"), b"{}").unwrap();

        let groups = census(home.path(), &test_stores(), false);
        assert_eq!(groups[0].confidence, DecodeConfidence::Exact);
        assert!(groups[0].decoded);
        assert!(!groups[0].orphaned);
    }

    #[test]
    fn foreign_root_census_asserts_no_orphan_verdict() {
        // `sessions --home /mnt/other-machine-home`: the project dirs live on
        // ANOTHER machine's filesystem, so "does it exist?" is not decidable
        // here. Asserting it anyway flags essentially everything orphaned.
        // The same fixture read as a local home DOES flag the orphan, which
        // is what proves the flag (not the fixture) is doing the work.
        let home = TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join("work")).unwrap();
        let gone = home.path().join("work/deleted-app"); // never created
        let enc = gone.display().to_string().replace('/', "-");
        let store = home.path().join(".claude/projects").join(&enc);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("s.jsonl"), b"{}").unwrap();

        let codex = home.path().join(".codex/sessions/2026/07");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(
            codex.join("rollout-1.jsonl"),
            b"{\"cwd\": \"/no/such/project/anywhere\"}\n",
        )
        .unwrap();

        let local = census(home.path(), &test_stores(), false);
        assert!(
            local.iter().any(|g| g.orphaned),
            "a local census must still flag gone project dirs"
        );

        let foreign = census(home.path(), &test_stores(), true);
        assert!(!foreign.is_empty(), "groups are still reported");
        assert!(
            foreign.iter().all(|g| !g.orphaned),
            "a foreign root must not assert an orphan verdict for any group"
        );
    }

    #[cfg(unix)]
    #[test]
    fn jsonl_walk_terminates_on_a_symlink_cycle() {
        // `glob`'s own recursion stats directories with `fs::metadata`, which
        // FOLLOWS symlinks — a cycle under a store recursed without bound, and
        // the cap (counting yielded matches) never fired because a runaway
        // walk yields nothing. The hand-rolled walk uses `DirEntry::metadata`,
        // which does not follow symlinks, so this terminates.
        let home = TempDir::new().unwrap();
        let sessions = home.path().join(".codex/sessions/2026");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("rollout.jsonl"),
            b"{\"cwd\": \"/tmp/cycle-proj\"}\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(home.path().join(".codex/sessions"), sessions.join("loop"))
            .unwrap();

        let groups = census(home.path(), &test_stores(), false);
        assert!(
            groups.iter().any(|g| g.project_path == "/tmp/cycle-proj"),
            "the real session must still be read past the cycle"
        );
    }

    /// Claude Code's REAL encoding (every non-alphanumeric → `-`), as opposed
    /// to the `/`-only `replace` the older fixtures above use.
    fn enc(p: &Path) -> String {
        encode_project_path(&p.display().to_string(), "-")
    }

    /// The builtin Claude Code declaration, key hint included.
    fn hinted_claude_store() -> Vec<(String, SessionStore)> {
        vec![(
            "claude_code".into(),
            SessionStore::EncodedDir {
                path: "~/.claude/projects".into(),
                separator: "-".into(),
                key_glob: Some("*.jsonl".into()),
                key_field: Some("cwd".into()),
            },
        )]
    }

    #[test]
    fn encode_matches_claude_code_rule() {
        assert_eq!(
            encode_project_path("/Users/devo/my_app.v2/LARS Docs", "-"),
            "-Users-devo-my-app-v2-LARS-Docs"
        );
        assert_eq!(
            encode_project_path("/home/x/.config/nvim", "-"),
            "-home-x--config-nvim"
        );
    }

    #[test]
    fn live_project_with_underscore_dot_and_space_decodes_exact_from_its_name() {
        // Regression: v0.8.0 folded these onto the parent as `Inferred`
        // orphans (`…/work/my-app  [ORPHANED?]`) because the DFS only tried
        // literal joins, so any live project with a `_`, `.` or space in its
        // path was reported as gone — with `--orphans` steering cleanup at it.
        let home = TempDir::new().unwrap();
        let leaves = ["my_app", "app.v2", "LARS Docs", ".config"];
        for leaf in leaves {
            let live = home.path().join("work").join(leaf);
            std::fs::create_dir_all(&live).unwrap();
            let store = home.path().join(".claude/projects").join(enc(&live));
            std::fs::create_dir_all(&store).unwrap();
            std::fs::write(store.join("s.jsonl"), b"{}").unwrap(); // no cwd hint inside
        }
        let groups = census(home.path(), &hinted_claude_store(), false);
        for leaf in leaves {
            let want = home.path().join("work").join(leaf).display().to_string();
            let g = groups
                .iter()
                .find(|g| g.project_path == want)
                .unwrap_or_else(|| {
                    panic!(
                        "{leaf}: not decoded; got {:?}",
                        groups.iter().map(|g| &g.project_path).collect::<Vec<_>>()
                    )
                });
            assert_eq!(g.confidence, DecodeConfidence::Exact, "{leaf}");
            assert!(!g.orphaned, "{leaf} is live");
        }
    }

    #[test]
    fn key_hint_reads_true_path_from_transcript_even_when_dir_is_gone() {
        // The cwd recorded inside the transcript is authoritative: a deleted
        // project with a `_` in its name (unrecoverable from the directory
        // name alone) is reported at its TRUE path, Exact, and orphaned.
        let home = TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join("work")).unwrap();
        let gone = home.path().join("work/my_app"); // never created
        let store = home.path().join(".claude/projects").join(enc(&gone));
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(
            store.join("a.jsonl"),
            format!(
                "{{\"type\":\"summary\",\"summary\":\"x\"}}\n\
                 {{\"type\":\"user\",\"cwd\":\"{}\",\"sessionId\":\"1\"}}\n",
                gone.display()
            ),
        )
        .unwrap();

        let groups = census(home.path(), &hinted_claude_store(), false);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].project_path, gone.display().to_string());
        assert_eq!(groups[0].confidence, DecodeConfidence::Exact);
        assert!(groups[0].orphaned);
    }

    #[test]
    fn key_hint_is_ignored_when_it_does_not_encode_to_the_directory() {
        // A stray file claiming an unrelated cwd must not re-key the directory.
        let home = TempDir::new().unwrap();
        let live = home.path().join("work/app");
        std::fs::create_dir_all(&live).unwrap();
        let store = home.path().join(".claude/projects").join(enc(&live));
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("a.jsonl"), b"{\"cwd\":\"/somewhere/else\"}\n").unwrap();
        let groups = census(home.path(), &hinted_claude_store(), false);
        assert_eq!(groups[0].project_path, live.display().to_string());
        assert_eq!(
            groups[0].confidence,
            DecodeConfidence::Exact,
            "falls back to name decode"
        );
    }

    #[test]
    fn ambiguous_name_with_two_live_candidates_is_inferred_not_exact() {
        let home = TempDir::new().unwrap();
        let a = home.path().join("work/my_app");
        let b = home.path().join("work/my-app");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        assert_eq!(enc(&a), enc(&b), "the two collide in the store");
        let store = home.path().join(".claude/projects").join(enc(&a));
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("s.jsonl"), b"{}").unwrap();
        let groups = census(home.path(), &hinted_claude_store(), false);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].confidence,
            DecodeConfidence::Inferred,
            "cannot tell my_app from my-app"
        );
        assert!(!groups[0].orphaned, "whichever it is, it exists");
    }

    #[test]
    fn foreign_root_never_validates_names_against_the_local_filesystem() {
        // Without a hint the name is decoded naively at Inferred; with a hint
        // the recorded cwd is used verbatim, Exact — no local fs either way.
        let home = TempDir::new().unwrap();
        let root = home.path().join(".claude/projects");
        std::fs::create_dir_all(root.join("-home-other-work-my-app")).unwrap();
        std::fs::write(root.join("-home-other-work-my-app/s.jsonl"), b"{}").unwrap();
        std::fs::create_dir_all(root.join("-home-other-src-my-lib")).unwrap();
        std::fs::write(
            root.join("-home-other-src-my-lib/s.jsonl"),
            b"{\"cwd\":\"/home/other/src/my_lib\"}\n",
        )
        .unwrap();

        let groups = census(home.path(), &hinted_claude_store(), true);
        let naive = groups
            .iter()
            .find(|g| g.project_path == "/home/other/work/my/app")
            .expect("naive decode");
        assert_eq!(naive.confidence, DecodeConfidence::Inferred);
        assert!(!naive.orphaned);
        let hinted = groups
            .iter()
            .find(|g| g.project_path == "/home/other/src/my_lib")
            .expect("hinted decode");
        assert_eq!(hinted.confidence, DecodeConfidence::Exact);
        assert!(!hinted.orphaned, "no orphan verdict for a foreign root");
    }

    #[test]
    fn jsonl_glob_matches_relative_to_store_root_despite_metachars_and_trailing_slash() {
        // Regression: the pattern used to be `format!("{base}/{glob}")`, so a
        // `[`/`*`/`?` in the root acted as a metacharacter and a trailing `/`
        // on the declaration produced `//` — both a silent zero.
        let root = TempDir::new().unwrap();
        let home = root.path().join("fake home [2]");
        let codex = home.join(".codex/sessions/2026/07");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(
            codex.join("rollout-1.jsonl"),
            b"{\"cwd\": \"/tmp/glob-proj\"}\n",
        )
        .unwrap();

        let stores = vec![(
            "codex".to_string(),
            SessionStore::JsonlField {
                path: "~/.codex/sessions/".into(),
                glob: "**/*.jsonl".into(),
                key_field: "cwd".into(),
                fallback_field: None,
            },
        )];
        let groups = census(&home, &stores, false);
        assert!(
            groups.iter().any(|g| g.project_path == "/tmp/glob-proj"),
            "got {:?}",
            groups.iter().map(|g| &g.project_path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn jsonl_first_line_read_is_bounded() {
        let home = TempDir::new().unwrap();
        let codex = home.path().join(".codex/sessions");
        std::fs::create_dir_all(&codex).unwrap();
        // One 2 MiB line with the key at the very end: must not be read.
        let mut huge = String::from("{\"pad\":\"");
        huge.push_str(&"x".repeat(2 * 1024 * 1024));
        huge.push_str("\",\"cwd\":\"/tmp/huge-proj\"}\n");
        std::fs::write(codex.join("huge.jsonl"), huge).unwrap();
        std::fs::write(codex.join("ok.jsonl"), b"{\"cwd\":\"/tmp/ok-proj\"}\n").unwrap();
        let groups = census(home.path(), &test_stores(), false);
        assert!(groups.iter().any(|g| g.project_path == "/tmp/ok-proj"));
        assert!(
            !groups.iter().any(|g| g.project_path == "/tmp/huge-proj"),
            "a key beyond the byte budget is not read"
        );
    }

    #[test]
    fn sqlite_updated_column_coerces_text_rows_and_detects_millisecond_magnitude() {
        let home = TempDir::new().unwrap();
        let dbdir = home.path().join(".local/share/opencode");
        std::fs::create_dir_all(&dbdir).unwrap();
        let conn = rusqlite::Connection::open(dbdir.join("opencode.db")).unwrap();
        // `time_updated` has NO declared type (BLOB affinity), so the TEXT and
        // REAL rows are stored as written rather than coerced by SQLite.
        conn.execute_batch(
            "CREATE TABLE session (directory TEXT, time_updated, time_archived INTEGER);
             INSERT INTO session VALUES ('/tmp/text-row', '1752610000', NULL);
             INSERT INTO session VALUES ('/tmp/ms-row', 1752620000000, NULL);
             INSERT INTO session VALUES ('/tmp/real-row', 1752630000.0, NULL);",
        )
        .unwrap();
        drop(conn);
        // Declared (by default) in SECONDS — the ms row must still come out right.
        let stores = vec![(
            "opencode".to_string(),
            SessionStore::SqliteColumn {
                path: "~/.local/share/opencode/opencode.db".into(),
                table: "session".into(),
                path_column: "directory".into(),
                updated_column: Some("time_updated".into()),
                updated_unit: TimeUnit::S,
                archived_column: Some("time_archived".into()),
            },
        )];
        let groups = census(home.path(), &stores, false);
        let get = |p: &str| {
            groups
                .iter()
                .find(|g| g.project_path == p)
                .unwrap_or_else(|| panic!("{p} row dropped"))
                .tools["opencode"]
                .last_active_unix
        };
        assert_eq!(
            get("/tmp/text-row"),
            1_752_610_000,
            "TEXT epoch coerced, row kept"
        );
        assert_eq!(get("/tmp/ms-row"), 1_752_620_000, "ms magnitude read as ms");
        assert_eq!(get("/tmp/real-row"), 1_752_630_000, "REAL epoch coerced");
    }

    #[test]
    fn resolve_stores_reroots_env_discovered_tools_locally_only() {
        let registry = crate::tools::ToolRegistry::new().unwrap();
        let env = |k: &str| (k == "CODEX_HOME").then(|| "/mnt/data/codex/".to_string());
        let local = resolve_stores(registry.all(), Some(&env));
        let codex = local.iter().find(|(t, _)| t == "codex").unwrap();
        assert_eq!(codex.1.path(), "/mnt/data/codex/sessions");
        let claude = local.iter().find(|(t, _)| t == "claude_code").unwrap();
        assert_eq!(
            claude.1.path(),
            "~/.claude/projects",
            "symlink-discovered tools untouched"
        );
        assert!(
            local.windows(2).all(|w| w[0].0 <= w[1].0),
            "sorted by tool name"
        );

        let foreign = resolve_stores(registry.all(), None);
        assert_eq!(
            foreign.iter().find(|(t, _)| t == "codex").unwrap().1.path(),
            "~/.codex/sessions",
            "a foreign root ignores this machine's environment"
        );
    }
}
