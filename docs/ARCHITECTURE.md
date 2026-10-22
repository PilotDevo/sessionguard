# Architecture

How SessionGuard is put together and why. The rules an agent must not break are summarized
in `AGENTS.md` ("Invariants"); this page carries the detail. Moved from `CLAUDE.md` on the
2026-10-22 instruction review.

SessionGuard is a filesystem daemon that watches for project directory moves and reconciles AI tool session artifacts so tools don't lose their state.

## Pipeline (fully wired as of v0.1.0)

```
Filesystem Event → Watcher → Detector → Reconciler → Event Log
                                ↕              ↕
                          Tool Registry    SQLite Registry
```

Events flow:
1. `notify` fires a rename/move event
2. `watcher.rs` classifies it as `SessionEvent::Moved { from, to }`
3. `daemon.rs` calls `handle_session_event()` which dispatches to:
   - **Filter first** (v0.11): a rename whose destination is a *file* is ignored
     (one `stat`), and so is a directory rename unless the directory IS or
     CONTAINS a known project (`known::KnownProjects` — census keys + registry).
     Through v0.10 every rename — each git commit, editor save, Cargo build —
     planned a re-key across every store (~1.2 s / ~3.9 GB per event). A folder
     move yields one `(old, new)` pair per project beneath it.
   - `detector::detect_tools()` — finds AI tool artifacts at new path
   - `reconciler::reconcile()` — rewrites old path strings in IN-PROJECT files
   - `rekey::rekey_all()` — re-keys the HOME-DIR session stores (v0.9); driven
     by the `session_store` declarations, NOT by `detect_tools`, because a
     store lives under `$HOME` and a project may have no in-project artifact.
     **Runs FIRST, before the in-project detection guard.** v0.9.0 had it after,
     so a project with no `.claude/` inside it hit the early return and was
     never re-keyed — the exact case the feature exists for. Don't reorder.
   - `registry` — re-registers project under new path, drops old entry
4. Every DECISION — including the decisions to do nothing — is recorded to the
   `EventLog`'s `activity` table via `activity::ActivityRecord::emit()` (v0.10).
   Recording only actions taken is what let a totally inert daemon look healthy.

## Binary + Library Split

`src/main.rs` is the CLI dispatcher — **not thin**: at ~2.1k lines it holds real command logic (undo, rekey output, init, service install, census rendering) with almost no tests of its own. Prefer putting new logic in a `src/lib.rs` module (pure, injectable, unit-testable) and keeping `main.rs` to I/O and printing; `service.rs` renders files and `main.rs` only writes them and runs the commands, which is the pattern to follow.

## Module Map

- **`activity.rs`** (v0.10) — **What was decided, and why — including "nothing".** `Outcome` (`Acted{actions}`/`NoOp{reason}`/`Refused`/`Failed`) replaced `success: bool` on `ReconcileResult`. A bool cannot express "succeeded and did nothing", so it got used for "didn't fail": the `Notify` arm returned `success: true` with an empty action list, making a healthy reconcile and a totally inert one the same value — which is how store re-keying stayed broken for months. `Outcome::acted(n, reason)` is the ONLY way to build an `Acted` and returns a `NoOp` when `n == 0`, so that shape is unconstructible. `ActivityRecord::emit()` writes to the event log AND emits a matching tracing event so the two channels cannot disagree.
- **`cli.rs`** — Clap derive definitions for all subcommands. Add new commands here.
- **`config.rs`** — TOML config loading from `~/.config/sessionguard/config.toml`, defaults. Supports `SESSIONGUARD_DATA_DIR` env var override (used by tests for isolation).
- **`daemon.rs`** — Daemon lifecycle: PID file, signal handling, main event loop (`tokio::select!`). Contains `handle_session_event()` — the pipeline dispatcher.
- **`watcher.rs`** — Wraps `notify` v8. Classifies raw fs events into `SessionEvent` variants.
- **`detector.rs`** — Scans a project dir against `ToolRegistry` patterns to find session artifacts. Returns `DetectionResult` with resolved artifact file paths.
- **`tools/mod.rs`** — `ToolDefinition` struct and `ToolRegistry`. `new()` loads builtins only; `new_with_config(config)` loads the full chain (built-in → system → user → project config.tools). Production callers use `new_with_config`. `register` inherits `binary`/`home_dir_layout`/`session_store` from the definition it replaces when the override omits them; `SessionStore::validate` rejects degenerate store declarations at load, naming the file.
- **`tools/builtin/`** — Built-in TOML tool patterns compiled into the binary via `include_str!`.
- **`registry.rs`** — SQLite-backed project-to-session mapping. Stores actual artifact file paths (e.g., `.claude/settings.json`), not just project roots. Schema auto-migrates on open.
- **`reconciler.rs`** — Adapter-based path rewriting engine. `JsonAdapter` and `TomlAdapter` parse files and surgically rewrite only the declared target field. `TextAdapter` falls back to string replace. Dispatched by `PathFieldSpec.format`.
- **`event_log.rs`** — SQLite audit log of reconciliation actions, **migrations**, and **store re-keys** (the `migrations` and `rekeys` tables each store an opaque JSON undo-plan blob, decoupled from the engine that produced it), powering `undo` for all three. Plus the v0.10 `activity` table — one row per DECISION, no-ops included. Tables are separated by DURABILITY CLASS: `events`/`migrations`/`rekeys` back `undo` and are never auto-pruned; `activity` is disposable and bounded by `prune_activity`. Don't blur that boundary.
- **`health.rs`** — Tool-presence / launcher health checks (binary on PATH, etc.), plus `DaemonHealth` (v0.10): whether the daemon is actually DOING anything. Every field is DERIVED at query time (PID file, config vs. filesystem, activity log) — a stored health record is a lie the moment the process dies. `is_inert()` (running but never acted) is the direct antidote to the silent-no-op failure. Backs `status --deep`.
- **`inventory.rs`** — Bounded filesystem walk that enumerates each tool's declared `home_dir_layout`: location, size, file count, last-modified. Backs `sessionguard inventory`; read-only lead-in to `migrate`.
- **`migrate/`** (`mod.rs` + `tests.rs`) — The v0.4 migration engine: a nine-stage state machine (Preflight → Snapshot → Quiesce → Copy → Verify → Rewrite → Resume → Validate → Retain, then Done) with trait-DI backends (`Quiescer`/`EnvWriter` + `Fake*` test doubles), `undo_migration`, and `cleanup_migration`. Returns a `MigrationResult`; `main.rs` persists it to the event log. Driven by `home_dir_layout` on `ToolDefinition`.
- **`sessions.rs`** — Per-project session census across the tools' home-dir stores. As of v0.8, **declaration-driven**: it no longer hardcodes the three store paths but dispatches on each loaded tool's `[tool.session_store]` binding (`SessionStore::EncodedDir`/`JsonlField`/`SqliteColumn` in `tools/mod.rs`) — Claude Code encoded-dir decoding — the declaration's *key hint* (`key_glob`/`key_field`, builtin `*.jsonl`/`cwd`) reads the literal path recorded inside a transcript first, and only hint-less directories fall back to the encoding-aware filesystem DFS, with three-state `DecodeConfidence`: `exact`/`inferred`/`unresolved` (so a deleted project can decode as an orphan instead of vanishing). `resolve_stores()` builds the store list from the registry, re-rooting env-discovered stores (`CODEX_HOME`) for a local census, Codex JSONL field lookup, OpenCode SQLite read-only. `census(home, stores, foreign_root)` also backs `--home <path>` (an arbitrary root, e.g. a mounted remote home). Backs `sessionguard sessions` (+ `--orphans`); the dashboard's Activity tab consumes its `--format json`.
- **`rekey.rs`** (v0.9) — **Store re-keying: the reconcile for home-dir stores.** `reconciler.rs` rewrites paths *inside a project*; the three store-bearing tools keep none there, so this re-keys the store itself from an old project path to a new one, driven by the same `[tool.session_store]` declaration `sessions.rs` reads. `plan()` is pure (no mutation) and returns exactly what `apply()` will do — that is what backs `--dry-run`; `apply()` returns the inverse plan, which `main.rs` records in the event log so `undo` can reverse it. `encoded_dir` is TWO actions (rename the dir **and** rewrite the recorded `cwd` inside, since v0.8.1's key hint made the store keyed twice); rewrites match the whole JSON token so `/a/b` can't match inside `/a/bc`. Refuses on an existing destination store (would merge histories) or a locked DB. Called by `daemon.rs::rekey_stores` on a move — from the STORE list, never from `detect_tools` (detection scans the project; the store is under `$HOME`). Carries its own `RekeyError`/`RekeyFailure`.
- **`known.rs`** (v0.11) — `KnownProjects`: which directories are projects the daemon should act on (every census store key that decodes, plus every registered project; `$HOME` and its ancestors excluded). `pairs_for_move(from, to)` returns `from` itself if known plus every known project beneath it, translated under `to`, matching through symlinked prefixes (macOS `/var` → `/private/var`). Rebuilt at startup, on SIGHUP, and on a miss at most every 2 s (a census is ~20 ms).
- **`service.rs`** (v0.11) — Login service: renders a launchd plist (macOS) or systemd user unit (Linux) that runs `start --foreground`, restarting only on a crash (a clean `stop` stays stopped). Pure rendering + command lists; `main.rs` writes and runs them. `stable_exe_path` points the service at the `PATH` name, not a Homebrew Cellar path that `brew upgrade` deletes. Through v0.10 macOS had no autostart at all.
- **`fleet.rs`** (v0.8) — Fleet-wide session census: runs the *remote* `sessionguard --version` + `sessions --format json` over ssh per `[[hosts]]` config and merges the result, stamping `host` provenance without re-deriving `orphaned` (that verdict always comes from the origin host). Backs `sessionguard sessions --host <name>` / `--all-hosts`. Read-only by construction — no other remote command is ever run — and upgrades a pre-`confidence` 0.7.0 payload on the fly. Refuses an ssh destination starting with `-` (and passes `--` before it) and restricts the per-host `binary` to a plain path before spawning any process (argv/remote-shell injection guards); distinguishes ssh's own exit 255 (`Unreachable`) from the remote command's status (`RemoteFailed`, 127 = not on the remote PATH). Carries its own `FleetError`.
- **`update.rs`** — Self-update for `sessionguard update` (v0.5): install-method detection (defer to brew/cargo, refuse dev builds), version compare, a curl-backed `ReleaseClient` trait (faked in tests), and SHA256SUMS-verified download → atomic swap with `.bak-<ver>` rollback → daemon restart. Carries its own `UpdateError`.
- **`error.rs`** — `thiserror` error enum used across the daemon/reconciler core (`migrate.rs`, `update.rs`, and `fleet.rs` carry their own domain errors).

## Runtime Tool Pattern System

Tool definitions are **data, not code**. They load from TOML at startup in this precedence (later overrides earlier):

1. Built-in (`src/tools/builtin/*.toml`, compiled in)
2. System (`/etc/sessionguard/tools/*.toml`)
3. User (`~/.config/sessionguard/tools/*.toml`)
4. Project (`sessionguard.toml` `[[tools]]` section)

To add a new tool: create a TOML file in `src/tools/builtin/`, add its `include_str!` to `tools/mod.rs`, and register it in `load_builtin()`.

## Repository Layout

```
src/                    # Library + binary source (cli, daemon, watcher, detector,
                        #   reconciler, registry, event_log, tools/, health,
                        #   inventory, migrate, sessions, fleet, rekey, activity, known, service,
                        #   update, config, error,
                        #   main, lib)
tests/
  cli_smoke.rs          # Basic CLI invocation tests
  sandbox.rs            # Full integration tests with real project fixtures
examples/
  notify_dump.rs        # Standalone notify-event dumper (debug aid)
scripts/
  dogfood.sh            # E2E reconcile smoke test
  migrate-dogfood.sh    # E2E migrate → undo smoke test
  rekey-dogfood.sh      # E2E re-key -> undo (byte-identity) + REAL DAEMON: git noise ignored, project & folder moves re-keyed
  update-dogfood.sh     # E2E self-update smoke test (offline fake release)
  check-consistency.sh  # release-metadata consistency gate (runs in CI)
docs/
  ARCHITECTURE.md       # This page
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
