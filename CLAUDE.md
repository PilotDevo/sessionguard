# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Development Commands

```bash
cargo build                          # Debug build
cargo build --release                # Release build (LTO, stripped)
cargo test                           # Run all tests (unit + integration)
cargo test <name> -- --nocapture     # Run a single test with output
cargo clippy -- -D warnings          # Lint (CI enforces zero warnings)
cargo fmt                            # Format code
cargo fmt -- --check                 # Check formatting without modifying
cargo run -- <subcommand>            # Run CLI (e.g., cargo run -- status)
RUST_LOG=debug cargo run -- start --foreground  # Run daemon with debug logging
```

If `just` is installed:
```bash
just check    # fmt-check + lint + test (CI equivalent)
just run      # daemon foreground with debug logging
just test-one <name>  # single test
```

## Architecture

SessionGuard is a filesystem daemon that watches for project directory moves and reconciles AI tool session artifacts so tools don't lose their state.

### Pipeline

```
Filesystem Event → Watcher → Detector → Reconciler → Event Log
                                ↕              ↕
                          Tool Registry    SQLite Registry
```

### Binary + Library Split

`src/main.rs` is a thin CLI dispatcher. All logic lives in `src/lib.rs` modules for testability.

### Module Map

- **`cli.rs`** — Clap derive definitions for all subcommands. Add new commands here.
- **`config.rs`** — TOML config loading from `~/.config/sessionguard/config.toml`, defaults.
- **`daemon.rs`** — Daemon lifecycle: PID file, signal handling, main event loop (`tokio::select!`).
- **`watcher.rs`** — Wraps `notify` crate. Classifies raw fs events into `SessionEvent` variants.
- **`detector.rs`** — Scans a project dir against `ToolRegistry` patterns to find session artifacts.
- **`tools/mod.rs`** — `ToolDefinition` struct and `ToolRegistry`. Loads patterns at runtime from TOML.
- **`tools/builtin/`** — Built-in TOML tool patterns compiled into the binary via `include_str!`.
- **`registry.rs`** — SQLite-backed project-to-session mapping. Schema auto-migrates on open.
- **`reconciler.rs`** — Path rewriting engine. String-replaces old paths in session artifacts.
- **`event_log.rs`** — SQLite audit log of all reconciliation actions (for undo capability).
- **`error.rs`** — `thiserror` error enum used across all library modules.

### Runtime Tool Pattern System

Tool definitions are **data, not code**. They load from TOML at startup in this precedence (later overrides earlier):

1. Built-in (`src/tools/builtin/*.toml`, compiled in)
2. System (`/etc/sessionguard/tools/*.toml`)
3. User (`~/.config/sessionguard/tools/*.toml`)
4. Project (`sessionguard.toml` `[[tools]]` section)

To add a new tool: create a TOML file in `src/tools/builtin/`, add its `include_str!` to `tools/mod.rs`, and register it in `load_builtin()`.

## CI/CD

- **`.github/workflows/ci.yml`** — Runs on PR/push to main. Matrix: ubuntu + macos. Checks: fmt, clippy, test, cargo-deny.
- **`.github/workflows/release.yml`** — On `v*` tags. Builds binaries for linux-x86_64, macos-x86_64, macos-aarch64. Creates GitHub release with `git-cliff` changelog.
- **`.github/dependabot.yml`** — Weekly updates for cargo deps and GH actions.

## Versioning & Release

Uses conventional commits. Version lives in `Cargo.toml`.

```bash
cargo release patch --no-publish --execute  # Bump, tag, push
git cliff -o CHANGELOG.md                    # Regenerate changelog
```

Tags follow `v0.1.0` format. The release workflow auto-builds binaries on tag push.

## Key Design Decisions

- **Async runtime**: tokio (for concurrent fs event handling + signal trapping).
- **SQLite bundled**: via `rusqlite` with `bundled` feature — no system SQLite dependency.
- **`notify` v7**: Cross-platform fs watching (FSEvents on macOS, inotify on Linux).
- **MSRV 1.75**: Set in `Cargo.toml` and `clippy.toml`.
