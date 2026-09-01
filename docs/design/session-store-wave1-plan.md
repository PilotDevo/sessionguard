# Session-Store Model — Wave 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make session stores data rather than hardcoded Rust, add fleet-wide
census with origin-host orphan evaluation, and stop claiming reconcile support
that does not exist — all without mutating any session data.

**Architecture:** A new `[tool.session_store]` block on `ToolDefinition`
declares where a tool's sessions live and how they are keyed to projects, using
three layout kinds implemented in Rust but bound by data. `sessions.rs` becomes
a driver over those declarations instead of three hardcoded readers. A new
`fleet.rs` runs the remote binary over ssh and merges its JSON with host
provenance. Decode confidence replaces a boolean so deleted projects can finally
be detected as orphans.

**Tech Stack:** Rust 2021, serde (tagged enums), rusqlite (read-only), the
existing `glob` dependency, ssh as the fleet transport.

**Spec:** `docs/design/session-store-model.md`

## Global Constraints

- MSRV **1.85**, edition **2021** — do not raise either.
- **No new dependencies.** `glob` and `rusqlite` are already present; use them.
- **Read-only wave.** No task may write, rename, or delete session data on any
  host. Store re-keying is Wave 2.
- **No remote mutation ever in this wave** — remote access is `ssh <target>
  sessionguard sessions --format json` and a version probe, nothing else.
- Conventional commits (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`).
- `cargo fmt`, `cargo clippy -- -D warnings`, and `cargo test` must pass before
  every commit. Hooks run fmt/clippy automatically.
- `bash scripts/check-consistency.sh` must exit 0 before the release task.
- All copyright headers: `// Copyright 2026 Devin R O'Loughlin / Droco LLC`.

---

## File Structure

- `src/tools/mod.rs` — **modify**: add `SessionStore`, `TimeUnit`, and the
  `session_store` field on `ToolDefinition`. Schema lives with the other tool
  schema types.
- `src/sessions.rs` — **modify**: readers become driven by `SessionStore`
  declarations; add `DecodeConfidence`; add `host` provenance.
- `src/fleet.rs` — **create**: ssh transport and remote-census merge. Kept
  separate so `sessions.rs` stays a pure local-filesystem reader with no process
  spawning.
- `src/config.rs` — **modify**: add `HostSpec` and `Config.hosts`.
- `src/cli.rs` / `src/main.rs` — **modify**: `--home`, `--host`, `--all-hosts`.
- `src/tools/builtin/{claude_code,codex,opencode}.toml` — **modify**: declare
  real stores; remove verified-fictional `path_fields`.
- `src/tools/builtin/gemini_cli.toml` — **modify**: remove fictional field.
- `scripts/dogfood.sh` — **modify**: stop fabricating `project_path`.

---

### Task 1: `SessionStore` schema

**Files:**
- Modify: `src/tools/mod.rs`
- Test: `src/tools/mod.rs` (existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: `pub enum SessionStore { EncodedDir{path,separator}, JsonlField{path,glob,key_field,fallback_field}, SqliteColumn{path,table,path_column,updated_column,updated_unit,archived_column} }`, `pub enum TimeUnit { S, Ms }`, and `ToolDefinition.session_store: Option<SessionStore>`.

- [ ] **Step 1: Write the failing test**

Add to `src/tools/mod.rs` tests:

```rust
#[test]
fn session_store_parses_all_three_layouts() {
    let t: ToolDefinition = toml::from_str(
        r#"
        name = "t"
        session_patterns = []
        [tool_session_store_placeholder]
        "#,
    )
    .unwrap_or_else(|_| panic!("placeholder"));
    let _ = t;
}
```

Replace that placeholder immediately with the real test (kept separate so the
first run fails for the right reason):

```rust
#[test]
fn session_store_encoded_dir_parses_with_default_separator() {
    let s: SessionStore = toml::from_str(
        r#"
        layout = "encoded_dir"
        path = "~/.claude/projects"
        "#,
    )
    .unwrap();
    assert_eq!(
        s,
        SessionStore::EncodedDir {
            path: "~/.claude/projects".into(),
            separator: "-".into()
        }
    );
}

#[test]
fn session_store_jsonl_and_sqlite_parse() {
    let j: SessionStore = toml::from_str(
        r#"
        layout = "jsonl_field"
        path = "~/.codex/sessions"
        key_field = "cwd"
        fallback_field = "payload.cwd"
        "#,
    )
    .unwrap();
    match j {
        SessionStore::JsonlField { ref glob, ref key_field, .. } => {
            assert_eq!(glob, "**/*.jsonl");
            assert_eq!(key_field, "cwd");
        }
        _ => panic!("wrong variant"),
    }

    let q: SessionStore = toml::from_str(
        r#"
        layout = "sqlite_column"
        path = "~/.local/share/opencode/opencode.db"
        table = "session"
        path_column = "directory"
        updated_column = "time_updated"
        updated_unit = "ms"
        archived_column = "time_archived"
        "#,
    )
    .unwrap();
    match q {
        SessionStore::SqliteColumn { updated_unit, .. } => {
            assert_eq!(updated_unit, TimeUnit::Ms);
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn session_store_rejects_unknown_layout() {
    assert!(toml::from_str::<SessionStore>(
        r#"
        layout = "telepathy"
        path = "/x"
        "#
    )
    .is_err());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet session_store 2>&1 | head -20`
Expected: FAIL — `cannot find type SessionStore in this scope`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/tools/mod.rs`:

```rust
/// Where a tool's session store lives and how its entries are keyed to
/// projects. Layout *kinds* are implemented in Rust; their bindings are data,
/// so a tool with a known storage shape is a TOML file rather than a recompile.
/// Deliberately separate from [`HomeDirLayout`]: that says how to *repoint* a
/// tool at a relocated store, this says what the store is and how it is keyed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "layout", rename_all = "snake_case")]
pub enum SessionStore {
    /// One directory per project; the directory name is the project path with
    /// each path separator replaced by `separator` (Claude Code).
    EncodedDir {
        path: String,
        #[serde(default = "default_separator")]
        separator: String,
    },
    /// A tree of JSONL files; the project path is a field on the first line.
    JsonlField {
        path: String,
        #[serde(default = "default_jsonl_glob")]
        glob: String,
        key_field: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fallback_field: Option<String>,
    },
    /// Rows in a SQLite table; the project path is a column.
    SqliteColumn {
        path: String,
        table: String,
        path_column: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_column: Option<String>,
        #[serde(default)]
        updated_unit: TimeUnit,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        archived_column: Option<String>,
    },
}

fn default_separator() -> String {
    "-".to_string()
}
fn default_jsonl_glob() -> String {
    "**/*.jsonl".to_string()
}

/// Unit of a store's "last updated" column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimeUnit {
    /// Unix seconds.
    #[default]
    S,
    /// Unix milliseconds.
    Ms,
}
```

And on `ToolDefinition`, after `home_dir_layout`:

```rust
    /// Where this tool's sessions live and how they are keyed to projects.
    /// Tools without this block contribute nothing to `sessionguard sessions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_store: Option<SessionStore>,
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --quiet session_store`
Expected: 3 passed. Then `cargo test --quiet` — all existing tests still pass
(the new field is `#[serde(default)]`, so every existing TOML still parses).

- [ ] **Step 5: Commit**

```bash
git add src/tools/mod.rs
git commit -m "feat(tools): add [tool.session_store] schema with three data-bound layout kinds"
```

---

### Task 2: Data-driven census readers

**Files:**
- Modify: `src/sessions.rs`
- Modify: `src/main.rs` (the `Command::Sessions` arm, ~line 923)
- Test: `src/sessions.rs` tests

**Interfaces:**
- Consumes: `SessionStore`, `TimeUnit` from Task 1.
- Produces: `pub fn census(home: &Path, stores: &[(String, SessionStore)]) -> Vec<SessionGroup>` — replaces the old single-argument `census(home)`.

- [ ] **Step 1: Write the failing test**

Replace the existing `census_groups_claude_and_codex_by_project_and_flags_orphans`
body's call site and add a declarations helper in `src/sessions.rs` tests:

```rust
/// The builtin store declarations, expressed as data — mirrors what the
/// TOMLs will declare in Task 3.
fn test_stores() -> Vec<(String, SessionStore)> {
    vec![
        (
            "claude_code".into(),
            SessionStore::EncodedDir {
                path: "~/.claude/projects".into(),
                separator: "-".into(),
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
        },
    )];
    let groups = census(home.path(), &stores);
    assert_eq!(groups.len(), 1, "declared custom path must be read");
    assert_eq!(groups[0].tools["claude_code"].count, 1);
}
```

Update every existing call of `census(home.path())` in the test module to
`census(home.path(), &test_stores())`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet sessions 2>&1 | head -20`
Expected: FAIL — `census` takes 1 argument but 2 were supplied.

- [ ] **Step 3: Write minimal implementation**

In `src/sessions.rs`, add the tilde expansion helper and rewrite `census` to
dispatch on declarations:

```rust
use crate::tools::{SessionStore, TimeUnit};

/// Expand a leading `~` against `home`. Declarations use `~` so the same TOML
/// works for any user and any census root (including a mounted remote home).
fn expand(home: &Path, raw: &str) -> PathBuf {
    match raw.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if raw == "~" => home.to_path_buf(),
        None => PathBuf::from(raw),
    }
}

/// SQLite identifiers come from a TOML file, so they are validated rather than
/// interpolated blindly — a malicious or fat-fingered declaration must not be
/// able to inject SQL.
fn safe_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn census(home: &Path, stores: &[(String, SessionStore)]) -> Vec<SessionGroup> {
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
            SessionStore::EncodedDir { path, separator } => {
                read_encoded_dir(&expand(home, path), separator, tool, &mut absorb)
            }
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
            let exists = Path::new(&project_path).exists();
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
```

Rename the three readers to `read_encoded_dir`, `read_jsonl_field`,
`read_sqlite_column`, each taking its bindings as parameters instead of
hardcoding paths. `read_encoded_dir` takes `separator` and passes it to the
decoder. `read_jsonl_field` uses the existing `glob` crate:

```rust
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
    let full = format!("{}/{}", base.display(), pattern);
    let Ok(paths) = glob::glob(&full) else { return };
    for (visited, entry) in paths.enumerate() {
        if visited > SESSION_WALK_CAP {
            return;
        }
        let Ok(p) = entry else { continue };
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Some(cwd) = jsonl_key(&p, key_field, fallback_field) else {
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
```

`read_sqlite_column` validates identifiers before building its query:

```rust
    if !safe_ident(table) || !safe_ident(path_column) {
        tracing::warn!(tool, "session_store declares unsafe SQL identifiers; skipping");
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
```

Divide the updated value by 1000 when `updated_unit == TimeUnit::Ms`.

In `src/main.rs`, build the declarations from the registry:

```rust
            let tool_registry = ToolRegistry::new_with_config(&config)?;
            let stores: Vec<(String, sessionguard::tools::SessionStore)> = tool_registry
                .all()
                .filter_map(|t| t.session_store.clone().map(|s| (t.name.clone(), s)))
                .collect();
            let mut groups = sessionguard::sessions::census(&home, &stores);
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --quiet sessions` then `cargo test --quiet`
Expected: all pass, including the pre-existing census tests (behavior preserved).

- [ ] **Step 5: Commit**

```bash
git add src/sessions.rs src/main.rs
git commit -m "refactor(sessions): drive store readers from declarations instead of hardcoded paths"
```

---

### Task 3: Builtin bindings, honesty patch, dogfood fix

**Files:**
- Modify: `src/tools/builtin/claude_code.toml`, `codex.toml`, `opencode.toml`, `gemini_cli.toml`
- Modify: `scripts/dogfood.sh`
- Test: `tests/cli_smoke.rs`

**Interfaces:**
- Consumes: the schema from Task 1, the driver from Task 2.
- Produces: builtin tools that declare real stores; no tool claims a
  `path_fields` target that was verified not to exist.

- [ ] **Step 1: Write the failing test**

Add to `tests/cli_smoke.rs`:

```rust
#[test]
fn cli_tools_json_declares_real_session_stores_and_no_fictional_fields() {
    let home = TempDir::new().unwrap();
    let out = sg(&home)
        .args(["tools", "list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let tools: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let by = |n: &str| -> serde_json::Value {
        tools
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == n)
            .expect("tool present")
            .clone()
    };

    // Verified stores are declared.
    assert_eq!(by("claude_code")["session_store"]["layout"], "encoded_dir");
    assert_eq!(by("codex")["session_store"]["layout"], "jsonl_field");
    assert_eq!(by("opencode")["session_store"]["layout"], "sqlite_column");

    // Verified-fictional path_fields are gone (the field named does not exist
    // in any real install; see docs/design/session-store-model.md).
    for t in ["claude_code", "gemini_cli"] {
        let pf = by(t)["path_fields"].as_array().cloned().unwrap_or_default();
        assert!(pf.is_empty(), "{t} must not declare unverified path_fields");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet cli_tools_json_declares 2>&1 | tail -20`
Expected: FAIL — `session_store` is null and `path_fields` is non-empty.

- [ ] **Step 3: Write minimal implementation**

`src/tools/builtin/claude_code.toml` — remove the `[[tool.path_fields]]` block
entirely and append:

```toml
# Claude Code embeds the project path in NO in-project file (verified against
# real installs). Its path-bearing state is this home-dir store, whose
# directory names encode the project path. Reconciling a move therefore means
# re-keying this store — Wave 2 — not rewriting anything inside the project.
[tool.session_store]
path = "~/.claude/projects"
layout = "encoded_dir"

# Only `projects/` is migratable. The parent `~/.claude` holds live runtime
# state (sessions/, daemon/, sockets) and credential material that must never
# be relocated or symlinked. No quiesce unit is declared: Claude Code is an
# interactive desktop app with no service unit, so migrate's existing
# "no quiesce hook declared" warning is the correct behavior.
[tool.home_dir_layout]
default_path = "~/.claude/projects"
discovery = "symlink"
```

`codex.toml` — append:

```toml
[tool.session_store]
path = "~/.codex/sessions"
layout = "jsonl_field"
key_field = "cwd"
fallback_field = "payload.cwd"
```

`opencode.toml` — append:

```toml
[tool.session_store]
path = "~/.local/share/opencode/opencode.db"
layout = "sqlite_column"
table = "session"
path_column = "directory"
updated_column = "time_updated"
updated_unit = "ms"
archived_column = "time_archived"
```

`gemini_cli.toml` — remove the `[[tool.path_fields]]` block. Its declared
`.gemini/settings.json` → `project_root` was verified absent (real files contain
only `mcpServers`).

Then fix `scripts/dogfood.sh`. It currently fabricates
`{"project_path": ...}` at lines 52-54 and asserts on it at 109-144 — it tests
the JSON adapter using a field it invented, via a builtin tool that no longer
declares it. Change the fixture to declare its own synthetic tool in the
throwaway config (model on `scripts/migrate-dogfood.sh`, which already does
this) and rename the banner to say what it proves:

```bash
# Declare a SYNTHETIC tool for this smoke: it validates the JSON adapter's
# surgical single-field rewrite, not any shipped tool's real layout. Claude
# Code deliberately declares no path_fields (it has no in-project path state).
cat >> "$CONFIG" <<'TOML'
[[tools]]
name = "dogfood_json"
display_name = "Dogfood JSON Tool"
session_patterns = [".dogfood/"]
[[tools.path_fields]]
file = ".dogfood/settings.json"
field = "project_path"
format = "json"
TOML
```

Update the fixture to write `.dogfood/settings.json`, and the assertions to read
that file. Keep the sibling-field-untouched assertion — that is the adapter
property worth proving.

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test --quiet
cargo build --release --quiet
SESSIONGUARD_BIN=./target/release/sessionguard bash scripts/dogfood.sh
./target/release/sessionguard sessions | head -5   # real census still works
```
Expected: tests pass, dogfood PASSes, census output unchanged from before the
refactor.

- [ ] **Step 5: Commit**

```bash
git add src/tools/builtin scripts/dogfood.sh tests/cli_smoke.rs
git commit -m "feat(tools): declare verified session stores; drop fictional path_fields

claude_code and gemini_cli declared path_fields naming fields that do not
exist in any real install, making their reconcile a silent no-op. Remove
them and declare the stores that actually hold their path-bearing state.
dogfood.sh now uses a synthetic tool rather than fabricating the field it
verifies."
```

---

### Task 4: Decode confidence

**Files:**
- Modify: `src/sessions.rs`
- Modify: `src/main.rs` (sessions text rendering, ~line 955)
- Modify: `tools/dashboard/app.py` (`_activity_from_cli`)
- Test: `src/sessions.rs` tests

**Interfaces:**
- Consumes: `census` from Task 2.
- Produces: `pub enum DecodeConfidence { Exact, Inferred, Unresolved }` (ordered
  most→least confident), `SessionGroup.confidence`, and a retained
  `SessionGroup.decoded: bool` compatibility field.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn deleted_claude_project_is_inferred_and_orphaned() {
    // Today this is impossible: decode DFS-validates against the live
    // filesystem, so a DELETED project never decodes and therefore never
    // reports as orphaned. Inferred confidence is what makes Claude Code
    // orphan detection work at all.
    let home = TempDir::new().unwrap();
    let gone = home.path().join("work/deleted-app");
    let enc = gone.display().to_string().replace('/', "-");
    let store = home.path().join(".claude/projects").join(&enc);
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("s.jsonl"), b"{}").unwrap();

    let groups = census(home.path(), &test_stores());
    let g = groups.iter().find(|g| g.project_path == gone.display().to_string())
        .expect("naive decode should propose the deleted path");
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

    let groups = census(home.path(), &test_stores());
    assert_eq!(groups[0].confidence, DecodeConfidence::Exact);
    assert!(groups[0].decoded);
    assert!(!groups[0].orphaned);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet decode 2>&1 | head -20`
Expected: FAIL — `DecodeConfidence` not found.

- [ ] **Step 3: Write minimal implementation**

```rust
/// How confidently a store entry was resolved to a real project path.
/// Ordered most-confident first so `min` keeps the best decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DecodeConfidence {
    /// Validated against a real directory on this host.
    Exact,
    /// The encoding resolved to a plausible path that does not exist — the
    /// most likely reading is a deleted project.
    Inferred,
    /// No plausible decode; the raw store name is reported.
    Unresolved,
}
```

Change the decoder to return confidence, adding the naive fallback:

```rust
pub fn decode_encoded_dir(name: &str, separator: &str) -> (String, DecodeConfidence) {
    let Some(rest) = name.strip_prefix(separator) else {
        return (name.to_string(), DecodeConfidence::Unresolved);
    };
    let parts: Vec<&str> = rest.split(separator).collect();
    if let Some(found) = walk(Path::new("/"), &parts) {
        return (found.display().to_string(), DecodeConfidence::Exact);
    }
    // DFS failed: every candidate split hit a missing directory. Propose the
    // naive decode so a deleted project still surfaces as one row with a
    // usable path, rather than an opaque encoded name.
    (
        format!("/{}", parts.join("/")),
        DecodeConfidence::Inferred,
    )
}
```

On `SessionGroup`, keep both fields:

```rust
    /// How confidently `project_path` was resolved.
    pub confidence: DecodeConfidence,
    /// Compatibility alias for `confidence == Exact`, retained one release so
    /// existing JSON consumers (the dashboard's Activity adapter) do not break
    /// silently. Prefer `confidence`.
    pub decoded: bool,
```

In `src/main.rs` text output, replace `if !g.decoded` with:

```rust
                        match g.confidence {
                            sessionguard::sessions::DecodeConfidence::Inferred if g.orphaned => {
                                markers.push_str("  [ORPHANED?]")
                            }
                            sessionguard::sessions::DecodeConfidence::Unresolved => {
                                markers.push_str("  [ENCODED NAME]")
                            }
                            _ => {}
                        }
```
(keep the existing `[ORPHANED]` marker for `Exact`).

In `tools/dashboard/app.py::_activity_from_cli`, pass confidence through:

```python
                "encoded": g.get("confidence", "exact") == "unresolved",
                "orphaned": bool(g.get("orphaned", False)),
                "confidence": g.get("confidence", "exact"),
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test --quiet
python3 -m py_compile tools/dashboard/app.py
./target/release/sessionguard sessions --orphans   # Claude orphans now appear
```
Expected: tests pass; the fedora-style double-row artifact collapses to one row.

- [ ] **Step 5: Commit**

```bash
git add src/sessions.rs src/main.rs tools/dashboard/app.py
git commit -m "feat(sessions): three-state decode confidence so deleted projects report as orphans"
```

---

### Task 5: `--home` census root

**Files:**
- Modify: `src/cli.rs` (the `Sessions` variant), `src/main.rs`
- Test: `tests/cli_smoke.rs`

**Interfaces:**
- Consumes: `census` from Task 2.
- Produces: `sessions --home <path>`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn cli_sessions_honors_explicit_home_root() {
    // A census root other than $HOME — the mounted/rsync'd-home case.
    let real = TempDir::new().unwrap();   // process HOME: empty
    let other = TempDir::new().unwrap();  // the root we actually census
    let live = other.path().join("p/app");
    std::fs::create_dir_all(&live).unwrap();
    let enc = live.display().to_string().replace('/', "-");
    let store = other.path().join(".claude/projects").join(&enc);
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("s.jsonl"), "{}").unwrap();

    sg(&real)
        .args(["sessions", "--home", &other.path().display().to_string()])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 project(s) with sessions"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet honors_explicit_home 2>&1 | tail -10`
Expected: FAIL — `unexpected argument '--home'`.

- [ ] **Step 3: Write minimal implementation**

In `src/cli.rs`, add to the `Sessions` variant:

```rust
        /// Census this directory as the home root instead of $HOME (e.g. a
        /// mounted or rsync'd home from another machine).
        #[arg(long)]
        home: Option<PathBuf>,
```

In `src/main.rs`, replace the home resolution:

```rust
            let home = match home {
                Some(h) => h,
                None => match directories::BaseDirs::new() {
                    Some(d) => d.home_dir().to_owned(),
                    None => anyhow::bail!("cannot determine your home directory"),
                },
            };
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --quiet` — all pass.

- [ ] **Step 5: Commit**

```bash
git add src/cli.rs src/main.rs tests/cli_smoke.rs
git commit -m "feat(sessions): --home to census an arbitrary root"
```

---

### Task 6: Fleet census over ssh

**Files:**
- Create: `src/fleet.rs`
- Modify: `src/lib.rs` (declare the module), `src/config.rs`, `src/cli.rs`, `src/main.rs`
- Test: `src/fleet.rs` tests, `src/config.rs` tests

**Interfaces:**
- Consumes: `SessionGroup` (Task 2/4), `update::parse_version`.
- Produces: `pub struct HostSpec { name: String, ssh: String }`, `Config.hosts: Vec<HostSpec>`, `pub fn remote_census(host: &HostSpec) -> Result<Vec<SessionGroup>, FleetError>`, `pub fn adopt_host(groups: &mut [SessionGroup], host: &str)`.

- [ ] **Step 1: Write the failing test**

In `src/config.rs` tests:

```rust
#[test]
fn config_parses_hosts() {
    let c: Config = toml::from_str(
        r#"
        watch_roots = []
        [[hosts]]
        name = "fedora"
        ssh = "devo@192.168.10.90"
        "#,
    )
    .unwrap();
    assert_eq!(c.hosts.len(), 1);
    assert_eq!(c.hosts[0].name, "fedora");
}
```

In `src/fleet.rs` tests — the merge semantics, which are the risky part and are
testable without ssh:

```rust
#[test]
fn adopt_host_stamps_provenance_without_re_evaluating_orphans() {
    // A remote path that does not exist locally must KEEP the origin host's
    // orphan verdict. Re-deriving it locally would flag every remote group as
    // orphaned (observed: all 8 fedora paths report exists=false on the Mac).
    let mut groups = vec![SessionGroup {
        project_path: "/home/devo/personal/v2".into(),
        confidence: DecodeConfidence::Exact,
        decoded: true,
        orphaned: false, // origin host says it exists there
        host: "local".into(),
        tools: Default::default(),
    }];
    adopt_host(&mut groups, "fedora");
    assert_eq!(groups[0].host, "fedora");
    assert!(
        !groups[0].orphaned,
        "orphan status must come from the origin host, never be re-derived"
    );
}

#[test]
fn remote_version_below_minimum_is_rejected_with_a_named_error() {
    let e = check_remote_version("fedora", "sessionguard 0.6.3").unwrap_err();
    assert!(matches!(e, FleetError::TooOld { .. }));
    assert!(e.to_string().contains("fedora"));
    assert!(e.to_string().contains("0.6.3"));
}

#[test]
fn remote_version_at_minimum_is_accepted() {
    assert!(check_remote_version("fedora", "sessionguard 0.7.0").is_ok());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet fleet 2>&1 | head -10`
Expected: FAIL — `src/fleet.rs` does not exist.

- [ ] **Step 3: Write minimal implementation**

Create `src/fleet.rs`:

```rust
// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Fleet-wide session census.
//!
//! Runs the *remote* `sessionguard` over ssh and merges its JSON. The remote
//! binary owns its own filesystem truth — in particular whether a project
//! directory still exists — so orphan status is never re-derived locally.
//! Read-only by construction: the only remote commands are `--version` and
//! `sessions --format json`.

use crate::config::HostSpec;
use crate::sessions::SessionGroup;

/// Oldest remote release that has `sessionguard sessions`.
const MIN_REMOTE_VERSION: (u64, u64, u64) = (0, 7, 0);

#[derive(Debug, thiserror::Error)]
pub enum FleetError {
    #[error("host `{host}`: unreachable ({detail})")]
    Unreachable { host: String, detail: String },
    #[error("host `{host}`: sessionguard {found} is too old for `sessions` (needs 0.7.0+); run `sessionguard update` there")]
    TooOld { host: String, found: String },
    #[error("host `{host}`: could not parse remote output ({detail})")]
    BadOutput { host: String, detail: String },
}

/// Verify a remote `sessionguard --version` string meets the floor.
pub fn check_remote_version(host: &str, version_output: &str) -> Result<(), FleetError> {
    let found = version_output
        .split_whitespace()
        .find_map(|w| crate::update::parse_version(w).map(|v| (w.to_string(), v)));
    match found {
        Some((_, v)) if v >= MIN_REMOTE_VERSION => Ok(()),
        Some((raw, _)) => Err(FleetError::TooOld {
            host: host.to_string(),
            found: raw,
        }),
        None => Err(FleetError::BadOutput {
            host: host.to_string(),
            detail: format!("no version in {version_output:?}"),
        }),
    }
}

/// Stamp provenance on groups returned by a remote host. Deliberately does NOT
/// touch `orphaned` — that verdict belongs to the host the sessions live on.
pub fn adopt_host(groups: &mut [SessionGroup], host: &str) {
    for g in groups.iter_mut() {
        g.host = host.to_string();
    }
}

fn ssh(host: &HostSpec, remote_args: &[&str]) -> Result<String, FleetError> {
    let out = std::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", &host.ssh])
        .args(remote_args)
        .output()
        .map_err(|e| FleetError::Unreachable {
            host: host.name.clone(),
            detail: e.to_string(),
        })?;
    if !out.status.success() {
        return Err(FleetError::Unreachable {
            host: host.name.clone(),
            detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Census one remote host. Read-only.
pub fn remote_census(host: &HostSpec) -> Result<Vec<SessionGroup>, FleetError> {
    let version = ssh(host, &["sessionguard", "--version"])?;
    check_remote_version(&host.name, &version)?;
    let json = ssh(host, &["sessionguard", "sessions", "--format", "json"])?;
    let mut groups: Vec<SessionGroup> =
        serde_json::from_str(&json).map_err(|e| FleetError::BadOutput {
            host: host.name.clone(),
            detail: e.to_string(),
        })?;
    adopt_host(&mut groups, &host.name);
    Ok(groups)
}
```

`SessionGroup` must gain `#[derive(Deserialize)]` (it is currently
`Serialize`-only) so remote JSON parses, and `DecodeConfidence` likewise.

`src/config.rs`:

```rust
/// A machine in the fleet that can be censused over ssh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostSpec {
    /// Short name used with `--host`.
    pub name: String,
    /// ssh destination, e.g. `devo@192.168.10.90`.
    pub ssh: String,
}
```
plus `#[serde(default)] pub hosts: Vec<HostSpec>` on `Config`.

`src/cli.rs` — add to `Sessions`:

```rust
        /// Census a configured host instead of this machine.
        #[arg(long, conflicts_with = "home")]
        host: Option<String>,
        /// Census every configured host plus this machine.
        #[arg(long, conflicts_with_all = ["home", "host"])]
        all_hosts: bool,
```

`src/main.rs` — resolve the group source before filtering:

```rust
            let mut groups = if all_hosts {
                let mut all = sessionguard::sessions::census(&home, &stores);
                for h in &config.hosts {
                    match sessionguard::fleet::remote_census(h) {
                        Ok(mut g) => all.append(&mut g),
                        Err(e) => eprintln!("warning: {e}"),
                    }
                }
                all
            } else if let Some(name) = &host {
                let h = config
                    .hosts
                    .iter()
                    .find(|h| &h.name == name)
                    .ok_or_else(|| anyhow::anyhow!("no host named `{name}` in config"))?;
                sessionguard::fleet::remote_census(h)?
            } else {
                sessionguard::sessions::census(&home, &stores)
            };
```

Text output prefixes the host when it is not `local`.

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test --quiet
cargo clippy --quiet --all-targets -- -D warnings
# live check against the real fleet (read-only):
./target/release/sessionguard sessions --host fedora | head -5
```
Expected: unit tests pass; the live call returns fedora's 8 groups with 2
orphans — matching what fedora itself reports.

- [ ] **Step 5: Commit**

```bash
git add src/fleet.rs src/lib.rs src/config.rs src/cli.rs src/main.rs
git commit -m "feat(fleet): --host/--all-hosts census with origin-host orphan verdicts"
```

---

### Task 7: Docs and release v0.8.0

**Files:**
- Modify: `README.md`, `ROADMAP.md`, `SECURITY.md`, `CHANGELOG.md`, `CLAUDE.md`, `Cargo.toml`

- [ ] **Step 1: Correct the support table**

In `README.md`, Claude Code / Gemini CLI move off an unqualified "✅ Reconcile".
Add a support level and mark honestly:

- Claude Code: **Census + Migrate** (reconcile is Wave 2 store re-keying)
- Gemini CLI, Cursor, Windsurf, Aider: **Detect** — reconciliation unverified
- Codex, OpenCode: **Census + Migrate** (+ Reconcile where a real field exists)

Document `sessions --home/--host/--all-hosts` and `[[hosts]]` in Basic usage.

- [ ] **Step 2: Update CLAUDE.md module map**

Add `fleet.rs`; note `sessions.rs` is now declaration-driven.

- [ ] **Step 3: Version, changelog, roadmap, security**

Bump `Cargo.toml` to `0.8.0`. Add a `## [0.8.0]` CHANGELOG entry describing:
the `session_store` schema, declaration-driven census, fleet census, decode
confidence (deleted Claude projects now detectable as orphans), and the removal
of fictional `path_fields`. Note the JSON shape change (`confidence` added,
`decoded` retained one release). Update ROADMAP "(current)" to 0.8.x and
SECURITY to `0.8.x`.

- [ ] **Step 4: Verify everything**

```bash
bash scripts/check-consistency.sh          # must exit 0
cargo fmt -- --check && cargo clippy --quiet --all-targets -- -D warnings
cargo test --quiet
cargo build --release --quiet
for s in dogfood migrate-dogfood update-dogfood; do
  SESSIONGUARD_BIN=./target/release/sessionguard bash scripts/$s.sh >/dev/null && echo "  ✓ $s" || echo "  ✗ $s"
done
python3 -m py_compile tools/dashboard/app.py
```
Expected: consistency gate green, all suites pass, all three dogfoods PASS.

- [ ] **Step 5: Ship**

```bash
git add -A
git commit -m "chore(release): v0.8.0 — session-store model wave 1"
git checkout main && git merge --no-ff feat/session-store-model \
  -m "Merge branch 'feat/session-store-model' — session-store model wave 1 (v0.8.0)"
git push origin main
git tag -a v0.8.0 -m "v0.8.0 — session stores as data, fleet census, decode confidence"
git push origin v0.8.0
```

---

## Self-Review

**Spec coverage:** `[tool.session_store]` schema → T1. Data-driven readers → T2.
Builtin bindings + `claude_code` `home_dir_layout` → T3. Honesty patch (fictional
fields, README, dogfood) → T3 + T7. Decode confidence + consumer impact → T4.
`--home` → T5. Hosts, provenance, origin-host orphans, version check → T6.
Non-goals (no remote mutation, no A2A messaging, only `projects/` migratable)
are enforced by the Global Constraints and T3's TOML comments. Wave 2 items
(re-keying) and Wave 3 (A2A detection, archive) are deliberately absent.

**Placeholders:** none — every step carries the code or the exact command.

**Type consistency:** `SessionStore`/`TimeUnit` (T1) are used with identical
variant and field names in T2's `census` match and T3's TOMLs.
`DecodeConfidence` (T4) is used by T6's tests with the same variant names.
`census(home, stores)` has one signature across T2, T4, T5, T6.
`SessionGroup` gains `confidence`, `decoded`, `host` — all three are constructed
in T2/T4 and consumed in T6's test with the same names.
