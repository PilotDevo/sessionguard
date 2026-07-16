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
- **`event_log.rs`** — SQLite audit log of reconciliation actions **and migrations** (the `migrations` table stores an opaque JSON undo-plan blob, decoupled from the migrate engine), powering `undo` for both.
- **`health.rs`** — Tool-presence / launcher health checks (binary on PATH, etc.).
- **`inventory.rs`** — Bounded filesystem walk that enumerates each tool's declared `home_dir_layout`: location, size, file count, last-modified. Backs `sessionguard inventory`; read-only lead-in to `migrate`.
- **`migrate/`** (`mod.rs` + `tests.rs`) — The v0.4 migration engine: a nine-stage state machine (Preflight → Snapshot → Quiesce → Copy → Verify → Rewrite → Resume → Validate → Retain, then Done) with trait-DI backends (`Quiescer`/`EnvWriter` + `Fake*` test doubles), `undo_migration`, and `cleanup_migration`. Returns a `MigrationResult`; `main.rs` persists it to the event log. Driven by `home_dir_layout` on `ToolDefinition`.
- **`sessions.rs`** — Per-project session census across the tools' home-dir stores (Claude Code encoded-dir DFS decoding, Codex JSONL `cwd`, OpenCode SQLite read-only). Backs `sessionguard sessions` (+ `--orphans`); the dashboard's Activity tab consumes its `--format json`.
- **`update.rs`** — Self-update for `sessionguard update` (v0.5): install-method detection (defer to brew/cargo, refuse dev builds), version compare, a curl-backed `ReleaseClient` trait (faked in tests), and SHA256SUMS-verified download → atomic swap with `.bak-<ver>` rollback → daemon restart. Carries its own `UpdateError`.
- **`error.rs`** — `thiserror` error enum used across the daemon/reconciler core (`migrate.rs` and `update.rs` carry their own domain errors).

### Runtime Tool Pattern System

Tool definitions are **data, not code**. They load from TOML at startup in this precedence (later overrides earlier):

1. Built-in (`src/tools/builtin/*.toml`, compiled in)
2. System (`/etc/sessionguard/tools/*.toml`)
3. User (`~/.config/sessionguard/tools/*.toml`)
4. Project (`sessionguard.toml` `[[tools]]` section)

To add a new tool: create a TOML file in `src/tools/builtin/`, add its `include_str!` to `tools/mod.rs`, and register it in `load_builtin()`.

## Testing

Tests use `SESSIONGUARD_DATA_DIR` (and `SESSIONGUARD_CONFIG_DIR`) to point each test at an isolated per-test SQLite registry and config dir — no shared state, and no reads of the operator's real `~/.config`/`$HOME`.

```bash
cargo test                           # ~180 tests (unit + integration)
cargo test sandbox_                  # integration tests only
cargo test reconcile_               # end-to-end reconciliation proofs
cargo test -- --nocapture            # with stdout
```

The `cmd()` helper in `tests/sandbox.rs` wraps `Command::cargo_bin` and injects the isolation env vars automatically — use it for all new sandbox tests.

End-to-end smoke scripts live in `scripts/`: `dogfood.sh` (reconcile path), `migrate-dogfood.sh` (migrate → undo round-trip), and `update-dogfood.sh` (self-update swap/rollback/tamper-refusal via a file:// fake release). All isolate via the env vars and a throwaway config; CI runs all three on both OSes. `scripts/check-consistency.sh` gates release-metadata drift in CI.

## CI/CD

- **`.github/workflows/ci.yml`** — Runs on PR/push to main. Matrix: ubuntu + macos. Checks: fmt, clippy, test, cargo-deny, and the `scripts/*dogfood.sh` e2e smokes.
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
src/                    # Library + binary source (cli, daemon, watcher, detector,
                        #   reconciler, registry, event_log, tools/, health,
                        #   inventory, migrate, config, error, main, lib)
tests/
  cli_smoke.rs          # Basic CLI invocation tests
  sandbox.rs            # Full integration tests with real project fixtures
examples/
  notify_dump.rs        # Standalone notify-event dumper (debug aid)
scripts/
  dogfood.sh            # E2E reconcile smoke test
  migrate-dogfood.sh    # E2E migrate → undo smoke test
  update-dogfood.sh     # E2E self-update smoke test (offline fake release)
  check-consistency.sh  # release-metadata consistency gate (runs in CI)
docs/
  design/               # Active design drafts (e.g. handoff.md)
  history/              # Retired design docs (e.g. migrate.md, shipped in v0.4)
  ops/                  # Operational runbooks (e.g. homebrew-tap-token.md)
integrations/
  claude-code/          # Optional Claude Code skill (NOT part of `cargo build`)
tools/
  dashboard/            # Read-only Python web dashboard (NOTE: distinct from src/tools/)
contrib/
  sessionguard.service  # systemd user service for Linux autostart
.github/
  workflows/            # CI and release pipelines
  ISSUE_TEMPLATE/       # Bug, feature, and tool-pattern issue templates
  PULL_REQUEST_TEMPLATE.md
  FUNDING.yml           # GitHub Sponsors
install.sh              # Curl-pipe installer (OS/arch auto-detection)
cliff.toml              # Changelog generation config
deny.toml               # cargo-deny license/advisory config
justfile                # Dev task runner
# Root meta: README, CHANGELOG, ROADMAP, CONTRIBUTING, SECURITY,
#   CODE_OF_CONDUCT, LICENSE, rust-toolchain.toml, rustfmt.toml,
#   clippy.toml, release.toml, .editorconfig
```

## Key Design Decisions

- **Async runtime**: tokio (for concurrent fs event handling + signal trapping).
- **SQLite bundled**: via `rusqlite` with `bundled` feature — no system SQLite dependency.
- **`notify` v8**: Cross-platform fs watching (FSEvents on macOS, inotify on Linux).
- **MSRV 1.85**: Set in `Cargo.toml` and `clippy.toml`.
- **Copyright**: All source files carry `// Copyright 2026 Devin R O'Loughlin / Droco LLC` header.
