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

## Automated Hooks (via .claude/settings.json)

- **PostToolUse (Edit|Write)** — `cargo fmt` runs automatically after any `.rs` file is edited.
- **Stop** — `cargo clippy -- -D warnings` runs when Claude finishes a task. Any warnings will appear in the output.

No manual format step needed during development — just edit and the hooks handle it.

## Architecture

SessionGuard is a filesystem daemon that watches for project directory moves and reconciles AI tool session artifacts so tools don't lose their state.

### Pipeline (fully wired as of v0.1.0)

```
Filesystem Event → Watcher → Detector → Reconciler → Event Log
                                ↕              ↕
                          Tool Registry    SQLite Registry
```

Events flow:
1. `notify` fires a rename/move event
2. `watcher.rs` classifies it as `SessionEvent::Moved { from, to }`
3. `daemon.rs` calls `handle_session_event()` which dispatches to:
   - `detector::detect_tools()` — finds AI tool artifacts at new path
   - `reconciler::reconcile()` — rewrites old path strings in artifact files
   - `registry` — re-registers project under new path, drops old entry
4. All actions are recorded to `EventLog` for auditability

### Binary + Library Split

`src/main.rs` is a thin CLI dispatcher. All logic lives in `src/lib.rs` modules for testability.

### Module Map

- **`cli.rs`** — Clap derive definitions for all subcommands. Add new commands here.
- **`config.rs`** — TOML config loading from `~/.config/sessionguard/config.toml`, defaults. Supports `SESSIONGUARD_DATA_DIR` env var override (used by tests for isolation).
- **`daemon.rs`** — Daemon lifecycle: PID file, signal handling, main event loop (`tokio::select!`). Contains `handle_session_event()` — the pipeline dispatcher.
- **`watcher.rs`** — Wraps `notify` v8. Classifies raw fs events into `SessionEvent` variants.
- **`detector.rs`** — Scans a project dir against `ToolRegistry` patterns to find session artifacts. Returns `DetectionResult` with resolved artifact file paths.
- **`tools/mod.rs`** — `ToolDefinition` struct and `ToolRegistry`. `new()` loads builtins only; `new_with_config(config)` loads the full chain (built-in → system → user → project config.tools). Production callers use `new_with_config`.
- **`tools/builtin/`** — Built-in TOML tool patterns compiled into the binary via `include_str!`.
- **`registry.rs`** — SQLite-backed project-to-session mapping. Stores actual artifact file paths (e.g., `.claude/settings.json`), not just project roots. Schema auto-migrates on open.
- **`reconciler.rs`** — Adapter-based path rewriting engine. `JsonAdapter` and `TomlAdapter` parse files and surgically rewrite only the declared target field. `TextAdapter` falls back to string replace. Dispatched by `PathFieldSpec.format`.
- **`event_log.rs`** — SQLite audit log of all reconciliation actions (for undo capability).
- **`error.rs`** — `thiserror` error enum used across all library modules.

### Runtime Tool Pattern System

Tool definitions are **data, not code**. They load from TOML at startup in this precedence (later overrides earlier):

1. Built-in (`src/tools/builtin/*.toml`, compiled in)
2. System (`/etc/sessionguard/tools/*.toml`)
3. User (`~/.config/sessionguard/tools/*.toml`)
4. Project (`sessionguard.toml` `[[tools]]` section)

To add a new tool: create a TOML file in `src/tools/builtin/`, add its `include_str!` to `tools/mod.rs`, and register it in `load_builtin()`.

## Testing

Tests use `SESSIONGUARD_DATA_DIR` to point each test at an isolated per-test SQLite registry — no shared state between runs.

```bash
cargo test                           # all 41 tests
cargo test sandbox_                  # integration tests only
cargo test reconcile_               # end-to-end reconciliation proofs
cargo test -- --nocapture            # with stdout
```

The `cmd()` helper in `tests/sandbox.rs` wraps `Command::cargo_bin` and injects the env var automatically — use it for all new sandbox tests.

## CI/CD

- **`.github/workflows/ci.yml`** — Runs on PR/push to main. Matrix: ubuntu + macos. Checks: fmt, clippy, test, cargo-deny.
- **`.github/workflows/release.yml`** — On `v*` tags. Builds binaries for linux-x86_64, macos-x86_64, macos-aarch64. Creates GitHub release with `git-cliff` changelog. Publishes to crates.io via `CARGO_REGISTRY_TOKEN` secret.
- **`.github/dependabot.yml`** — Weekly updates for cargo deps and GH actions.

## Versioning & Release

Uses conventional commits. Version lives in `Cargo.toml`.

```bash
cargo release patch --no-publish --execute  # Bump, tag, push → triggers full release pipeline
git cliff -o CHANGELOG.md                    # Regenerate changelog locally
```

Tags follow `v0.1.0` format. Pushing a tag triggers: build → GitHub release → crates.io publish.

## Repository Layout

```
src/                    # Library + binary source
tests/
  cli_smoke.rs          # Basic CLI invocation tests
  sandbox.rs            # Full integration tests with real project fixtures
contrib/
  sessionguard.service  # systemd user service for Linux autostart
.github/
  workflows/            # CI and release pipelines
  ISSUE_TEMPLATE/       # Bug, feature, and tool-pattern issue templates
  FUNDING.yml           # GitHub Sponsors
install.sh              # Curl-pipe installer (OS/arch auto-detection)
cliff.toml              # Changelog generation config
deny.toml               # cargo-deny license/advisory config
justfile                # Dev task runner
```

## Key Design Decisions

- **Async runtime**: tokio (for concurrent fs event handling + signal trapping).
- **SQLite bundled**: via `rusqlite` with `bundled` feature — no system SQLite dependency.
- **`notify` v8**: Cross-platform fs watching (FSEvents on macOS, inotify on Linux).
- **MSRV 1.75**: Set in `Cargo.toml` and `clippy.toml`.
- **Copyright**: All source files carry `// Copyright 2026 Devin R O'Loughlin / Droco LLC` header.
