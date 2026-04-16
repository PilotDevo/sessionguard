# Changelog

All notable changes to SessionGuard will be documented in this file.

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


