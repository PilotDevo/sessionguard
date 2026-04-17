# Changelog

All notable changes to SessionGuard will be documented in this file.

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


