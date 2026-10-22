# AGENTS.md — SessionGuard

Instructions for any coding agent working in this repository. Claude Code loads this file
through `CLAUDE.md`. If this file and the code disagree, the code wins and this file is fixed
in the same change.

## Purpose

SessionGuard (`sessionguard`, Rust, MIT) is a filesystem daemon and CLI that keeps AI coding
sessions intact when projects move. It rewrites path references inside a project, re-keys the
tools' home-directory session stores, takes a per-project session census (locally or across
ssh hosts), migrates a tool's data directory, and can undo every mutation it makes. Tool
support is data (TOML patterns), not code. The pipeline, module map and the reasons behind
them are in `docs/ARCHITECTURE.md`; read the relevant section before changing a module.

## Boundaries

- **`local` dev only.** Build and test in `target/`. Tests and the dogfood scripts isolate
  themselves with `SESSIONGUARD_DATA_DIR` and `SESSIONGUARD_CONFIG_DIR`; use scratch values
  for any manual run too, so nothing reads or writes the real `~/.config`, data dir or `$HOME`.
- **Maintainer machines run the released daemon as a login service** against real session
  stores. Without the maintainer's explicit ask, don't `cargo install` over it, run
  `service install|uninstall`, `start`/`stop`, or run mutating commands (`rekey`, `migrate`,
  `migrate-cleanup --execute`, `undo`, `update`, `import`, `init`, `watch`/`unwatch`)
  against real stores. Read-only use is fine: `status [--deep]`, `sessions`, `inventory`,
  `log`, `doctor`, `tools`, `simulate mv`, and `rekey`/`migrate` with `--dry-run`.
- **Fleet probes** (`sessions --host <name>` / `--all-hosts`) need a throwaway config dir
  with its own `[[hosts]]`. They only ever run `--version` and `sessions --format json`
  remotely; keep it that way.
- **Publishing needs approval.** Pushing to `main` runs CI. Pushing a `v*` tag runs
  `.github/workflows/release.yml`: binaries + `SHA256SUMS` → GitHub Release → crates.io →
  Homebrew tap. Both are public and permanent.

## Commands

```bash
cargo build                          # debug build
cargo build --release                # release build (LTO, stripped)
cargo test                           # unit + integration tests
cargo test <name> -- --nocapture     # one test with output (just test-one <name>)
cargo test sandbox_                  # integration tests in tests/sandbox.rs
cargo test reconcile_                # end-to-end reconciliation proofs
cargo clippy -- -D warnings          # lint; CI enforces zero warnings
cargo fmt -- --check                 # format check (cargo fmt to apply)
cargo deny check                     # licenses + RustSec advisories (just deny)
bash scripts/check-consistency.sh    # release-metadata drift gate (CI runs it)
RUST_LOG=debug cargo run -- start --foreground   # daemon in the foreground (just run)
cargo run -- <subcommand>            # run the CLI, e.g. cargo run -- status

just check                           # fmt-check + clippy + test (CI's core checks)
just dogfood                         # release build + dogfood, migrate and update smokes
SESSIONGUARD_BIN=./target/release/sessionguard bash scripts/rekey-dogfood.sh
                                     # the fourth CI smoke; `just dogfood` omits it
```

CI (`.github/workflows/ci.yml`, ubuntu + macos) runs fmt, the consistency script, clippy,
build, test, an MSRV 1.85 build + test, `cargo-deny`, and all four dogfood scripts. CI lints
with the latest stable Rust; if local stable lags, `rustup update stable` or
`rustup run <toolchain> cargo clippy -- -D warnings` before pushing.

## Invariants (don't break these)

- The daemon filters first: a rename whose destination is a file is ignored, and so is a
  directory rename unless the directory is or contains a known project (`known.rs`).
- `rekey::rekey_all()` runs **before** the in-project detection guard and is driven by the
  `[tool.session_store]` declarations, never by `detect_tools` (a store lives under `$HOME`;
  a project may have no in-project artifact). Don't reorder.
- Every decision, including "did nothing", is recorded through `ActivityRecord::emit()`.
  `Outcome::acted(n, reason)` is the only way to build `Acted` and returns `NoOp` when
  `n == 0`; don't reintroduce a bare success bool.
- Event-log durability classes: `events`/`migrations`/`rekeys` back `undo` and are never
  auto-pruned; `activity` is disposable (`prune_activity`). Don't blur that boundary.
- Health is derived at query time (PID file, config vs filesystem, activity log), never
  stored.
- `rekey::plan()` is pure and is exactly what `apply()` does (that backs `--dry-run`);
  `apply()` returns the inverse plan for `undo`. Path rewrites match the whole JSON token.
- `fleet.rs` is read-only by construction and keeps its argv/remote-shell injection guards.
- New logic goes in a `src/lib.rs` module (pure, injectable, unit-tested); `src/main.rs`
  (~2.1k lines, few tests of its own) stays I/O and printing. `service.rs` renders files and
  `main.rs` writes them and runs the commands: follow that pattern.

## Tests

- Use the `cmd()` helper in `tests/sandbox.rs` for new sandbox tests; it injects the
  isolation env vars.
- **Unit tests are not enough for the daemon path.** Every daemon defect so far passed its
  unit tests and was caught only by running the real daemon. Every mutating operation has a
  dogfood script in `scripts/` (reconcile, migrate, re-key, update); keep that 1:1 when you
  add one.

## Adding a tool

Create `src/tools/builtin/<tool>.toml`, add its `include_str!` to `src/tools/mod.rs`, register
it in `load_builtin()`, add detector tests, and update the supported-tools table in
`README.md`. Load precedence (later wins): built-in → `/etc/sessionguard/tools/*.toml` →
`~/.config/sessionguard/tools/*.toml` → project `sessionguard.toml` `[[tools]]`.

## Branches, commits, releases

- Work on `feat/*`, `fix/*`, `docs/*` or `chore/*` branches and land them on `main` as merge
  commits; force pushes to `main` are blocked (`CONTRIBUTING.md`).
- Conventional commits (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`).
- The version lives in `Cargo.toml`. A release bumps it and updates the README `Status:`
  line, the ROADMAP `(current)` marker, the SECURITY supported table and a dated
  `CHANGELOG.md` entry; `scripts/check-consistency.sh` fails on drift. Tags are `vX.Y.Z`.
- `CHANGELOG.md` is hand-written (Keep a Changelog). Don't regenerate it with
  `git cliff -o CHANGELOG.md` (`just changelog`); that overwrites it. The release workflow
  uses git-cliff for release notes only.

## Conventions

- tokio runtime; `rusqlite` with `bundled` SQLite; `notify` v8 (FSEvents / inotify).
- MSRV 1.85, set in `Cargo.toml` and `clippy.toml`; CI's MSRV job enforces it.
- Every source file carries `// Copyright 2026 Devin R O'Loughlin / Droco LLC`.
- `tools/dashboard/` is a read-only Python dashboard, distinct from `src/tools/`.
  `integrations/claude-code/` is an optional end-user skill, not part of `cargo build`.

## Where things live

- `README.md` (users), `CONTRIBUTING.md`, `ROADMAP.md`, `CHANGELOG.md`, `SECURITY.md`.
- `docs/ARCHITECTURE.md` (pipeline + module map), `docs/design/` (active designs, e.g.
  `handoff.md`), `docs/history/` (retired designs), `docs/ops/` (runbooks, e.g. the Homebrew
  tap token).
