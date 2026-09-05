# Design: session-store model — stores as data, fleet-aware census, store re-keying

> **Status**: Wave 1 (this doc's `[tool.session_store]` schema, the
> declaration-driven census, the `path_fields` honesty patch, three-state
> decode confidence, `sessions --home`, and the read-only fleet census) is
> implemented and released in 0.8.0. Waves 2-3 — store re-keying and A2A
> detection plus `sessions archive` — remain design-only. Drafted 2026-09-01
> against a v0.7.0 baseline.

## Why this exists

Three problems reported separately turned out to be one root cause.

**1. The reconcile layer targets fields that do not exist.** Verified on real
installs on this machine:

| Tool | Declared `path_fields` target | Reality |
| --- | --- | --- |
| `claude_code` | `.claude/settings.json` → `project_path` | file exists, **field does not** |
| `gemini_cli` | `.gemini/settings.json` → `project_root` | file exists (2×), **field does not** |
| `cursor` | `.cursor/state.json` | file not found on this host — **unverified** |
| `windsurf` | `.windsurf/state.json` | file not found on this host — **unverified** |
| `aider` | `.aider.chat.history.md` `_content` (text) | real filename, plausible — **unverified** |

When the declared field is absent the JSON adapter matches nothing and returns
"unchanged", so reconciliation is a **silent no-op** while detection still
reports the tool as supported. `grep` for the absolute project path across every
file in a real `.claude/` directory returns nothing: Claude Code embeds the
project path in **no** in-project file.

**2. The session stores that do work are hardcoded.** `sessions.rs` contains
three store readers as Rust string literals (`.claude/projects`,
`.codex/sessions`, `.local/share/opencode/opencode.db`). They cannot be
configured, overridden, or extended without a recompile — directly violating the
project's own stated principle (CLAUDE.md): *"Tool definitions are data, not
code."*

**3. The census is single-host.** `census(home: &Path)` already accepts an
arbitrary root, but `main.rs` pins it to the local home dir. A probe of the
`fedora` hub found **8 project groups and ~160 MB of session history** invisible
from the Mac, including a Codex session active seconds earlier.

The common thread: **there is no schema describing where a tool's session state
actually lives and how it is keyed to a project.** Everything above follows from
that absence.

## The model

There are two fundamentally different kinds of path-bearing state. SessionGuard
currently has a mechanism only for the first.

**Type 1 — in-project state.** A file inside the project embeds the absolute
project path. Fixing a move means rewriting that file. This is what
`path_fields` expresses. It is real for text-style stores (aider chat history)
and largely fictional for the structured ones.

**Type 2 — home-dir store keyed by project path.** The store's *location or
contents* encode which project the sessions belong to. Fixing a move means
**re-keying the store** — renaming a directory, rewriting a field, updating a
column — not touching anything inside the project.

Every modern harness uses Type 2. It is the mechanism SessionGuard lacks, and
adding it resolves all three problems at once.

## Schema: `[tool.session_store]`

A new optional block on `ToolDefinition`, orthogonal to `home_dir_layout`:

- `session_store` — *what the store is and how it is keyed to projects*
  (read the census, detect orphans, re-key on move).
- `home_dir_layout` — *how to repoint the tool at a new location* (symlink, env
  var, config edit) when relocating the store to another disk.

They frequently name the same path and are still separate concerns; a tool may
declare either, both, or neither. Neither implies the other.

### Layout kinds

Three kinds, matching the three readers that exist today. Kinds are implemented
in Rust; their **bindings are data**. This boundary is deliberate — accepting
arbitrary SQL or arbitrary traversal logic from a TOML file would be a footgun,
and this is the one place "data, not code" should bend. A new tool with a known
layout is a TOML file; only a genuinely novel storage shape needs Rust.

```toml
# encoded_dir — one directory per project, name encodes the path
[tool.session_store]
path   = "~/.claude/projects"
layout = "encoded_dir"
separator = "-"              # path separator replacement; default "-"

# jsonl_field — a tree of JSONL files, project named in the first line
[tool.session_store]
path           = "~/.codex/sessions"
layout         = "jsonl_field"
glob           = "**/*.jsonl"
key_field      = "cwd"
fallback_field = "payload.cwd"    # optional; newer layouts nest it

# sqlite_column — rows in a table, project named in a column
[tool.session_store]
path            = "~/.local/share/opencode/opencode.db"
layout          = "sqlite_column"
table           = "session"
path_column     = "directory"
updated_column  = "time_updated"
updated_unit    = "ms"            # "s" | "ms"; default "s"
archived_column = "time_archived" # optional; NULL means active
```

### Rust representation

A serde-tagged enum, so invalid combinations are unrepresentable:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "layout", rename_all = "snake_case")]
pub enum SessionStore {
    EncodedDir { path: String, #[serde(default = "dash")] separator: String },
    JsonlField {
        path: String,
        #[serde(default = "jsonl_glob")] glob: String,
        key_field: String,
        #[serde(default)] fallback_field: Option<String>,
    },
    SqliteColumn {
        path: String,
        table: String,
        path_column: String,
        #[serde(default)] updated_column: Option<String>,
        #[serde(default)] updated_unit: TimeUnit,
        #[serde(default)] archived_column: Option<String>,
    },
}
```

`ToolDefinition` gains `#[serde(default)] pub session_store: Option<SessionStore>`.
Declarations ride the existing four-tier precedence chain (built-in → system →
user → project), so an operator with a non-standard install overrides a path in
their own TOML rather than filing a bug.

### Builtin bindings

Only stores verified against real data are declared. Absence is honest.

| Tool | `session_store` | Basis |
| --- | --- | --- |
| `claude_code` | `encoded_dir` @ `~/.claude/projects` | verified: 967 MB across 16 dirs |
| `codex` | `jsonl_field` @ `~/.codex/sessions`, key `cwd` | verified: 469-line index, real `cwd` |
| `opencode` | `sqlite_column` @ `opencode.db` | verified: `session.directory` |
| `cursor`, `windsurf`, `gemini_cli`, `aider` | none | no store located on any available host |

`claude_code` also gains a `home_dir_layout` (`default_path = "~/.claude/projects"`,
`discovery = "symlink"`) so its 967 MB becomes migratable. Scope is deliberately
`projects/` and **not** all of `~/.claude`: the parent holds live runtime state
(`sessions/`, `daemon/`, sockets) and credential material that must not be
relocated or symlinked. No `quiesce` block is declared — Claude Code is an
interactive desktop app with no service unit, so migrate's existing "no quiesce
hook declared" warning is the correct and honest behavior.

## Fleet-aware census

### Configuration

`Config` gains a host list; the local host is implicit and always present.

```toml
[[hosts]]
name = "fedora"
ssh  = "devo@192.0.2.10"
```

### Interface

- `sessions --home <path>` — census an arbitrary root (a mounted or rsync'd
  home). Nearly free: `census()` already takes a root.
- `sessions --host <name>` / `--all-hosts` — census a configured host, or every
  host including local.

### Host provenance and orphan semantics

`SessionGroup` gains `host: String` (default `"local"`).

**Orphan status is evaluated on the host the sessions live on, never re-derived
by the caller.** This is mandatory, not defensive: every one of fedora's 8
project paths reports `exists == false` when tested from the Mac, so a naive
merge would flag all 8 as orphaned when the origin host correctly reports 2.

### Transport

Remote census runs the remote binary and merges its JSON:
`ssh <target> sessionguard sessions --format json`. This works today because the
fleet self-updates and fedora already runs 0.7.0. The remote version is checked;
a remote older than 0.7.0 (no `sessions` command) degrades to a clear message
naming the host and its version, never a silent empty result.

**Read-only in v1.** No remote mutation of any kind — no re-key, no archive, no
migrate across hosts. A merge bug can therefore never damage another machine.

### Decode confidence

Today an undecodable Claude directory and a deleted project are
indistinguishable, so one deleted project appears as two rows — one
`[ORPHANED]` from Codex, one `[ENCODED NAME]` from Claude (observed on fedora
with `amarillo-project`).

`SessionGroup.decoded: bool` is replaced by `confidence: DecodeConfidence`:

- `Exact` — DFS-validated against a real directory.
- `Inferred` — DFS failed; naive `separator → /` substitution produced a path
  that does not exist. The most likely reading is a deleted project.
- `Unresolved` — no plausible decode at all.

`orphaned` becomes `matches!(confidence, Exact | Inferred) && !exists_on_host`,
which merges the two `amarillo-project` rows without asserting more certainty
than the evidence supports. Text output distinguishes `[ORPHANED]` from
`[ORPHANED?]` for `Inferred`.

**Consumer impact — this rename is a breaking JSON change.** Two consumers read
`decoded` today and must land in the same change or they break silently:
`tools/dashboard/app.py` (`"encoded": not g.get("decoded", False)` — would
default every row to "encoded" and mislabel the whole Activity tab) and
`src/main.rs` text rendering. Emitting `confidence` *in addition to* a retained
`decoded` boolean for one release is the safer path and is the recommended
option; whichever is chosen, the dashboard adapter changes in the same commit.

## Store re-keying (the real reconcile)

For a tool with a `session_store`, a project move re-keys the store:

| Layout | Operation | Reversibility |
| --- | --- | --- |
| `encoded_dir` | rename `<old-encoding>` → `<new-encoding>` | trivial (rename back) |
| `jsonl_field` | rewrite the key field in matching files | existing atomic write + event log |
| `sqlite_column` | `UPDATE <table> SET <path_column>` for matching rows | requires a write connection |

Every re-key is `--dry-run`-able, recorded in the event log, and reversible via
`undo`, reusing the machinery `migrate` already proved.

**Refusals.** A re-key must refuse rather than guess when: the destination
encoding already exists (would merge two projects' history); the SQLite database
is locked by a running tool; or — for `claude_code` — live sessions reference the
old path (see below). Refusing loudly is correct; silently merging session
histories is not.

## Agent-to-agent awareness

Claude Code maintains a peer registry at `~/.claude/sessions/<pid>.json`
containing `{pid, sessionId, cwd, name, nameSource: "derived",
messagingSocketPath, peerProtocol, peerFeatures, kind, entrypoint}`, with
transport over per-PID unix sockets. Sessions are **addressed by a name derived
from `cwd`** — three were live during this investigation.

Moving a project therefore invalidates the identity live agents use to address
each other, which is SessionGuard's own thesis applied to the addressing layer.

v1 scope is **detection only**: read the registry, and when a watched project
moves, report how many live sessions still reference the old path (and refuse
the `claude_code` re-key while they do). Notifying peers over their sockets is
explicitly out of scope — it would make SessionGuard a participant in the A2A
protocol rather than an observer of it.

## Honesty patch

Independent of the schema work and shippable immediately:

- Remove the verified-fictional `path_fields` from `claude_code` and
  `gemini_cli`.
- Mark `cursor`, `windsurf`, and `aider` patterns provisional in the README
  support table — detection is verified, reconciliation is not — until someone
  with the tool installed confirms a real path-bearing file.
- Fix `scripts/dogfood.sh`, which currently fabricates a `project_path` field
  and then verifies the field it invented. It tests the JSON adapter, not any
  tool. It must exercise a store re-key against a synthetic store instead.

## Waves

**Wave 1 — read-only.** `session_store` schema + serde types; `sessions.rs`
becomes a driver over declarations rather than hardcoded readers; builtin
bindings; `--home`, `[[hosts]]`, `--host`/`--all-hosts` with provenance and
origin-host orphan evaluation; decode confidence; honesty patch. No mutation of
session data anywhere, so the risk ceiling is a wrong report.

**Wave 2 — mutation.** Store re-keying for all three layouts, with dry-run,
event-log records, `undo`, and the refusal rules. The riskiest wave; it lands
alone.

**Wave 3 — control.** A2A live-session detection; `sessions archive` for
orphans (rename-aside, undoable, never delete).

**Wave 1 is the scope of the first implementation plan.** Waves 2 and 3 get
their own plans written against this same model once wave 1 has soaked.

## Testing

- Schema round-trip: each layout kind parses, rejects invalid combinations, and
  honors the four-tier precedence chain.
- Per-layout readers against synthetic stores, reusing the existing `sessions`
  unit tests as the starting fixtures.
- Multi-host merge: groups carry provenance; orphan status comes from the origin
  host; a remote too old to support `sessions` degrades with a named message.
- Decode confidence: exact, inferred, and unresolved cases, including the
  two-rows-for-one-deleted-project merge.
- Wave 2: re-key dry-run changes nothing; real re-key is undoable; each refusal
  rule fires (existing destination, locked database, live sessions).
- Consumer contract: the dashboard's Activity adapter and the CLI text
  renderer both survive the confidence change (the JSON-shape test that the
  wiring audit added for `tools list` extended to `sessions`).
- `dogfood.sh` rewritten to exercise a real store re-key.

## Non-goals

- Remote mutation of any kind (v1 read-only across hosts).
- Participating in the A2A protocol — observe the registry, do not message peers.
- Relocating all of `~/.claude`; only the session store is in scope.
- Session *content* portability — that remains `docs/design/handoff.md`, which
  this work feeds: `handoff pack` needs exactly "enumerate one tool's sessions
  for one project", which the store schema provides.

## Risks

- **SQLite re-key on a live WAL database.** Mitigated by refusing when locked;
  `migrate` already treats OpenCode this way.
- **Encoding collisions on re-key.** Mitigated by refusing an existing
  destination rather than merging.
- **Remote version skew.** Mitigated by an explicit version check per host.
- **Store formats drift.** These are undocumented third-party layouts that can
  change without notice. Mitigated by keeping bindings in data (a drift becomes
  a TOML edit, not a release) and by degrading to "unresolved" rather than
  guessing.
