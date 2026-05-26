# Changelog

All notable changes to SessionGuard will be documented in this file.

## [0.3.5] - 2026-05-26

### Features

- **`sessionguard inventory`** — pure read-only command that walks every
  registered tool with a `home_dir_layout` declaration and reports
  `{tool, location, size, last_activity}`. The lead-in to v0.4
  `migrate`: answers *"what should I move and how big is it?"*
  - Text mode renders a compact table with human-friendly size + age
    formatting.
  - `--format json` for tooling integration.
  - Walks capped at 200k files per store; result includes a
    `truncated` flag when the cap was hit.
  - Symlinks are skipped (don't follow).
- **`home_dir_layout` schema on `ToolDefinition`** — declarative
  description of where each tool stores user-scoped data and how
  `sessionguard migrate` (v0.4, in flight) should rewrite its
  self-references. Optional; tools without it behave exactly as
  before. Full schema in `docs/design/migrate.md`.
- **Codex** and **OpenCode** builtins populated with home_dir_layout:
  - Codex: `discovery = "env"`, `env_var = "CODEX_HOME"`.
  - OpenCode: `discovery = "symlink"` (default XDG path; no env var
    or config file declares the data dir).

### Internal

- New `src/inventory.rs` module with `inventory_tools_impl()` plus
  9 unit tests (5 inventory module + 3 home_dir_layout schema + 1
  per-builtin assertion). 88 tests total (was 79).
- New `src/main.rs` helpers `fmt_size` and `fmt_ago` for the
  inventory text table, both with unit tests.

### Roadmap

- v0.3.4 docs/design/migrate.md captured the v0.4 contract. v0.3.5
  delivers schema + inventory (steps 1–2 of the implementation
  order). Next: state-machine skeleton (step 3).

## [0.3.4] - 2026-05-26

### Features

- **`sessionguard doctor --clean`** — unregister tracked projects whose
  directory no longer exists on disk. Pure report mode remains the
  default; cleanup is opt-in. Add `--dry-run` to preview without
  writing. Cascades through SQLite's `ON DELETE CASCADE` to drop any
  associated `session_artifacts` rows in one shot.
  - Operator's own Mac registry had ~33 stale entries from sandbox
    test fixture leftovers accumulated over months. One command
    cleared the lot.

### Tests

- Two new sandbox tests cover the new flag:
  - `sandbox_doctor_clean_dry_run_does_not_mutate` — verifies the
    registry survives a `--clean --dry-run` invocation
  - `sandbox_doctor_clean_removes_stale_entries` — registers two
    projects, deletes one, runs `--clean`, asserts the stale entry is
    gone and the live one survives

### Docs

- New `docs/ops/homebrew-tap-token.md` walking through the one-time
  `HOMEBREW_TAP_TOKEN` fine-grained PAT setup that the release
  workflow's `homebrew` job needs. The job has been failing loud (by
  design) on every release since v0.3.2 until the secret is created.
  Cross-referenced from the v0.3.2 changelog entry and the README
  roadmap "Shipped" section.

## [0.3.3] - 2026-04-18

### Features

- **Launcher health checks** — the *visibility* path of the "runtime
  upgrade lost my launcher" problem. When you upgrade Node, Python, or
  any runtime that hosts AI tooling, the global package installs under
  the previous version vanish from PATH; your session data is intact
  but `claude` / `codex` / etc. become "command not found." Sessions
  appear gone — they aren't.
  - New optional `binary` field on `ToolDefinition` declares the
    launcher binary expected on PATH.
  - All 7 built-in patterns populated: `claude_code → claude`,
    `cursor → cursor`, `windsurf → windsurf`, `aider → aider`,
    `gemini_cli → gemini`, `codex → codex`, `opencode → opencode`.
  - New `src/health.rs` module with `check_binary()` that resolves
    against PATH via a built-in `which(1)`-equivalent walker (no
    subprocess, works on minimal Linux images).
  - `BinaryStatus` enum: `Present { path }`, `Missing { binary }`,
    `NotConfigured`. Tagged JSON repr for dashboard consumption.

### CLI

- `sessionguard doctor` now reports a `launcher health` section
  alongside the existing tracked-project check. Missing launchers
  are flagged with a `[WARN]` line that explicitly notes
  *"session data intact; check installer / runtime version"* so
  users don't think their history is lost.
- `sessionguard tools list` gains a `LAUNCHER` column in the text
  output and a `binary_status` field in the `--format json` output.

### Dashboard

- **Tools tab** — per-tool block now shows a launcher status pill
  (`launcher OK` / `launcher missing` / `no launcher configured`)
  with the resolved path or actionable diagnostic.
- **Activity tab** — column headers for stores whose launcher binary
  is missing get a ⚠ badge, so at a glance you can see "this column
  has 14 sessions but the tool can't run."

### Roadmap

- Path B from the v0.3.x launcher-health roadmap entry (active
  *availability* — actually restoring launchers across runtime
  changes via `sessionguard restore-launcher`) remains deferred.
  Path A (visibility, this release) ships first to let real-world
  data inform whether visibility alone is enough.

## [0.3.2] - 2026-04-18

### Features

- **`--format json`** on `tools list`, `log`, and `status`. Emits the
  same structured data the library already serialises internally. Text
  output remains the default; JSON is opt-in via flag. The dashboard
  now consumes this instead of parsing the human-readable text output,
  eliminating a class of fragility (CLI text changes breaking the UI).
- **CI dogfood job** — `scripts/dogfood.sh` now runs in GitHub Actions
  on both `ubuntu-latest` and `macos-latest` after the Check matrix
  completes. Regression gate for the class of bugs that historically
  slipped past unit tests (rename pairing, macOS path aliasing).
- **Homebrew tap auto-update** — a new `release-homebrew.yml` workflow
  fires on `release: published`, downloads the asset tarballs, computes
  SHA256s, renders a fresh `Formula/sessionguard.rb`, and pushes to
  `PilotDevo/homebrew-tap`. Requires repository secret
  `HOMEBREW_TAP_TOKEN` (fine-grained PAT, `Contents: write` on the tap).
  Fails fast with a clear message if the secret isn't configured. See
  [`docs/ops/homebrew-tap-token.md`](docs/ops/homebrew-tap-token.md) for
  the one-time PAT creation walkthrough.

### Changes

- `log` text output now tags undone events with `(undone)` at end of line.
- Dashboard: `list_tools()` consumes `--format json`; stale text-parsing
  fallback removed.

### Tests

- 3 new CLI smoke tests verify that `--format json` produces valid JSON
  for `tools list`, `log`, and `status` (67 tests total).

## [0.3.1] - 2026-04-17

### Features

- **Two new built-in tool patterns**: Codex and OpenCode. Both declared as
  `on_move = "notify"` for now — their session data lives under `~/.codex`
  and `~/.local/share/opencode` respectively, keyed on absolute project
  paths. Home-dir reconciliation is v0.4 `migrate` scope; until then these
  patterns surface the tools' presence (via `AGENTS.md` + per-project
  markers) without touching the home-dir stores. Total built-in count is
  now **7**.
- **Dashboard: Sessions tab** — enumerates home-dir session stores for
  Claude Code, Codex, OpenCode, Cursor, and Gemini CLI. Shows presence,
  item count, aggregate size, and last-modified time. Walks are capped
  at 200k files per store and cached for 30 seconds so polling doesn't
  re-scan multi-GB trees.

### Notes

- Dashboard smoke test on the author's Mac reveals 13 GB of Codex
  rollouts, 1.6 GB of Claude Code projects, and 6 OpenCode sessions —
  exactly the kind of data the v0.4 `migrate` feature will target.

## [0.3.0] - 2026-04-17

### Features

- **`sessionguard undo`** — reverse previously-logged reconciliation actions.
  Routes to the same adapter used during reconciliation with `old_value` /
  `new_value` swapped. Supports `--last N` (default 1), `--id <N>` for a
  specific event, and `--dry-run` for preview-only. Undone events are marked
  via `undone_at` so they're excluded from future `undo` invocations.
- **`sessionguard tools [list] [--verbose]`** — inspect registered tool
  patterns (built-in + user config + project config). `--verbose` shows
  session patterns and path_fields per tool.
- **3 new built-in tool patterns**: Windsurf, Aider, Gemini CLI. Built-in
  count is now 5 (plus any user or project patterns).

### Changes

- **Event log schema**: `format` column (adapter hint for undo) and
  `undone_at` timestamp column added. Fresh DBs get the full schema; pre-v0.3
  DBs are migrated in-place via idempotent `ALTER TABLE ADD COLUMN`.
- **`ReconcileAction`** now carries the `format` field so undo can route to
  the right adapter without needing the tool definition.
- **`ROADMAP.md`** added, capturing v0.3 → v1.0 arc and the v0.4 "migrate"
  thesis shift.

### Internal

- `reconciler::rewrite_field` exposed as `pub(crate)` to support undo reuse
- Schema migration fixed: index creation on new columns now happens AFTER
  `ALTER TABLE` (previously both ran in one batch and the index failed,
  aborting the migration)

## [0.2.3] - 2026-04-17

### Bug Fixes (Critical)

- *(watcher)* Rename pairing buffer — `notify` emits renames as two half-events (`From`/`To` on Linux with cookies, back-to-back `Any` events on macOS FSEvents with no cookies). The watcher now buffers half-events and pairs them into proper `Moved` events by cookie match or FIFO-within-TTL fallback. Before this fix, end-to-end reconciliation never fired on macOS or Linux despite the v0.2.2 claims; dogfooding revealed the gap.
- *(reconciler)* macOS `/private` path aliasing — `notify` reports canonical paths (`/private/var/...`, `/private/tmp/...`), but user tooling stores the short form (`/var/...`, `/tmp/...`). Reconciliation now tries both forms and rewrites with the matching pair's form, so stored paths keep the style the user sees.

### Test

- Added `scripts/dogfood.sh` — end-to-end smoke test that runs the real daemon and verifies reconciliation against a synthetic Claude Code project. Safe to run anywhere; uses isolated config and data dirs.
- Added `examples/notify_dump.rs` — diagnostic tool that prints every raw `notify` event for a watched directory. Used to reverse-engineer macOS FSEvents behaviour.

## [0.2.2] - 2026-04-16

### Bug Fixes

- *(reconciler)* Prefix-safe path replacement — paths like `/foo/code-backup/x` are no longer corrupted when `old_root` is `/foo/code` (#19)
- *(watcher,daemon)* Explicit `RenameMode` classification — Linux inotify renames (separate From/To events) are no longer silently dropped (#19)

### Robustness

- `try_send` in the notify callback — the sync watcher thread can no longer deadlock on a full channel (#19)
- Atomic PID file write with tempfile + rename; refuses to clobber a live daemon (#19)
- RAII `PidGuard` removes the PID file on any exit path, including early errors (#19)
- `shutdown_signal` no longer panics on signal-registration failure (#19)
- `Stop` verifies the daemon is alive before sending SIGTERM; cleans up stale PID files (#19)

### Refactor

- `Scan` canonicalizes paths to match `Watch` (macOS `/var` → `/private/var`) (#19)
- `register_project` is now a single atomic `INSERT ... ON CONFLICT ... RETURNING` (#19)
- `EventLog` orders by `id DESC` instead of `timestamp` (SQLite `datetime('now')` is 1s resolution) (#19)

### Build

- *(deps)* Bump libc from 0.2.183 to 0.2.184 (#15)
- *(deps)* Bump toml from 1.1.0+spec-1.1.0 to 1.1.2+spec-1.1.0 (#16)
- *(deps)* Bump tokio from 1.50.0 to 1.51.1 (#17)
- *(deps)* Bump clap_complete from 4.6.0 to 4.6.1 (#18)

### Miscellaneous

- Add rust-toolchain.toml, PR template, crates.io badge (#13)

## [0.2.1] - 2026-03-30

### Miscellaneous

- Documentation polish and housekeeping

## [0.2.0] - 2026-03-30

### Bug Fixes

- Disable git-cliff GitHub remote auto-detection
- Use --allow-dirty and env var for cargo publish

### Documentation

- Update README and CLAUDE.md for v0.2 state (#11)

### Features

- Add install script, systemd service, issue templates, and SECURITY.md
- Wire full runtime tool loading chain (#8)

### Miscellaneous

- Add copyright headers to all source files
- Add Claude Code project hooks and update CLAUDE.md

### Refactor

- Adapter-based reconciliation with JSON/TOML parsers (#10)
- Store actual artifact file paths in registry (#9)

### Testing

- Add end-to-end reconciliation proof tests (#12)

## [0.1.0] - 2026-03-29

### Bug Fixes

- *(ci)* Update deny.toml for cargo-deny v2 format and add MPL-2.0 license

### Documentation

- Update README for accuracy, add sandbox tests and funding
- Fix droco.io link in README footer
- Activate GitHub Sponsors link

### Features

- Initial scaffold for SessionGuard
- Wire reconciliation pipeline and isolate test registry

### Miscellaneous

- Ignore MCP tool artifacts, remove stray playwright-mcp log
- Wire crates.io publish to release workflow, fix author email
- Remove GITHUB_REPO from git-cliff to fix 403 on changelog generation

### Build

- *(deps)* Bump notify from 7.0.0 to 8.2.0
- *(deps)* Bump toml from 0.8.23 to 1.1.0+spec-1.1.0
- *(deps)* Bump actions/upload-artifact from 4 to 7
- *(deps)* Bump rusqlite from 0.32.1 to 0.39.0
- *(deps)* Bump actions/checkout from 4 to 6
- *(deps)* Bump actions/download-artifact from 4 to 8
- *(deps)* Bump directories from 5.0.1 to 6.0.0


