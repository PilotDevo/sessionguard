# SessionGuard Roadmap

Living document. Version milestones capture the thesis arc; exact feature
ordering inside each milestone may shift based on what proves most useful
in real-world dogfooding.

## Where we are

**v0.5.0 (current)** — The v0.4 **Migrate** arc has shipped. On top of the
v0.1–v0.3 reconcile pipeline (move detection on macOS + Linux, seven built-in
tools, `undo`, `--format json`, launcher health, `doctor --clean`),
SessionGuard now relocates a tool's home-dir data between disks:
`sessionguard inventory` enumerates what's migratable; `migrate <tool> --to
<path>` runs a nine-stage state machine (quiesce → copy → verify → rewrite →
validate → retain) that's fully reversible via `undo` / `undo --migration
<id>`; `migrate-cleanup` reclaims the preserved originals. The source is never
auto-deleted. Dogfooded on a multi-GB OpenCode store → fast pool. v0.4.3
added symlink-faithful copy plus a repo-health pass (docs realigned to
reality, end-to-end migrate tests + `migrate-dogfood.sh`, an enforced 1.85
MSRV CI job, and a non-fatal Homebrew release job).

**v0.5.0** adds **`sessionguard update`** — one-command, checksum-verified,
rollback-safe self-update across the fleet — plus a published `SHA256SUMS`
release asset that `install.sh` now verifies.

The local read-only **dashboard** (`tools/dashboard/`) ships with an
**Activity** tab that gives a per-project, per-assistant view across
Claude Code, Codex, and OpenCode session stores, and surfaces launcher
health in the Tools tab — answering "where am I working, which
assistants have touched what, and can the tool actually run?" at a
glance.

The v0.4 *migrate* design is retired to
[`docs/history/migrate.md`](docs/history/migrate.md). The next arc —
cross-machine session **handoff** — is drafted in
[`docs/design/handoff.md`](docs/design/handoff.md).

## v0.3 — Undo + More Patterns  *(shipping)*

Goal: build trust. Users won't run an auto-reconciler they can't reverse.

- [x] `sessionguard undo` — reverses events via the existing adapter
      dispatch; supports `--last N`, `--id`, `--dry-run`
- [x] Tool patterns: Windsurf, Aider, Gemini CLI (on top of Claude Code +
      Cursor)
- [x] `sessionguard tools [list] [--verbose]` — inspect registered patterns
- [x] Event log stores `format` + `undone_at`; schema migration for
      pre-v0.3 logs
- [ ] Real background daemonisation (`--daemon` that actually forks)
      *(deferred to v0.3.x)*
- [ ] `scripts/dogfood.sh` as a required CI check *(deferred)*

## v0.4 — Migrate  *(shipped)*

> **Design**: retired to [`docs/history/migrate.md`](docs/history/migrate.md).

Turned SessionGuard from *"watches for moves"* into *"the tool that moves AI
dev environments safely."*

- [x] `sessionguard migrate <tool> --to <path>` — tool-aware relocation via a
      nine-stage state machine (quiesce → copy → verify → rewrite → resume →
      validate → retain)
- [x] `sessionguard inventory` — enumerate tools, their data locations and
      sizes; the read-only lead-in to `migrate`
- [x] `migrate` is reversible — `undo` / `undo --migration <id>` reverses a
      completed migration; the source is preserved at `.migrated-<unix>`
- [x] `sessionguard migrate-cleanup` — reclaim preserved originals once a
      migration is trusted (un-undoable after cleanup; live data untouched)
- [x] `--dry-run` on every destructive command as a first-class pattern
- [x] Quiesce graceful-skip — a declared-but-not-loaded systemd unit is benign
- [ ] btrfs snapshot integration for the Snapshot stage (currently stubbed) —
      *deferred*
- ~~Docs site (MkDocs)~~ — dropped; README + design docs suffice for now
- ~~`relocate <src> <dst>`~~ — dropped; `migrate` covers the real need

## v0.5 — Cross-machine handoff

> **Design**: [`docs/design/handoff.md`](docs/design/handoff.md) (draft).

Goal: the third "move" axis — resume the same session on a *different machine*.

- `sessionguard handoff pack/apply/inspect` — a portable `.sgbundle` that
  re-keys + path-remaps a tool's session for the target machine
- Claude Code + Codex first (JSON-keyed); OpenCode (SQLite) deferred to a
  follow-on
- Secrets never travel in a bundle; one-directional by design; undoable

## Fleet self-update — `sessionguard update`  *(shipped in v0.5.0)*

> **Design**: retired to [`docs/history/update.md`](docs/history/update.md).

Keeps every box on the fleet current with one command (the `fedora` hub was
found four minor versions behind on 2026-06-25 — exactly the drift this closes).

- [x] `sessionguard update [--check] [--dry-run] [--to <ver>]` — self-replace
  from GitHub releases; detect the install method and **defer to brew/cargo**
  rather than fight them; restart a running daemon; atomic swap + `.bak` rollback
- [x] Publish a `SHA256SUMS` release asset and verify it in both `install.sh`
  and `update` — closed the no-integrity-check gap in the curl-pipe installer
- [x] `--check` doubles as a read-only fleet-drift probe

## v0.6 — Tool Pattern Library

Goal: let the community extend the pattern catalog safely.

- Community tool-definition contribution model (separate repo or
  `contrib/tools/` dir)
- `sessionguard tools validate <toml>` — lint contributor TOMLs
- Docs: contributing guide, pattern authoring cookbook
- `cargo-audit` integration in CI
- Homebrew formula auto-publish workflow

## Launcher health  *(Path A shipped in v0.3.3 — `src/health.rs`)*

Sessions can survive *the project* moving (the original thesis) and
*the disk* changing (v0.4 `migrate`), but they don't survive *the
runtime* changing — which is a separate failure mode that SessionGuard
should be able to surface even if it can't fix it. The operator has
hit this repeatedly across runtime upgrades (Node, Python, others).

Path A (*Visibility* — notice and report missing launchers) shipped in v0.3.3
via `src/health.rs`, wired into `doctor` and the dashboard Tools tab. Path B
(*Availability* — restore launchers) remains deferred; the rest of this section
is retained as the design record.

**Motivating scenarios.** Same shape across runtimes:

- Upgrade Node v23 → v24. Globally-installed npm packages
  (Claude Code, Codex CLI, Gemini CLI, OpenCode) live under the
  previous Node version's `lib/node_modules/`; the new version has
  its own empty globals tree.
- Upgrade Python 3.11 → 3.12 with `pyenv`. `pipx`-installed AI
  tooling (Aider, etc.) is rooted in the previous Python's venv;
  the new Python sees nothing.
- Same pattern for any rbenv/asdf/system-package upgrade that
  swaps the runtime under tools installed against it.

In all of these the session data at `~/.claude/projects/`,
`~/.codex/sessions/`, `~/.local/share/opencode/` is untouched —
but the launcher binaries are gone from PATH. From the user's
POV "my sessions are gone" — they aren't, the launcher is just
unreachable.

**Two paths through this problem, scoped honestly:**

| | A — *Visibility* | B — *Availability* |
|---|---|---|
| What | SessionGuard *notices* missing launchers and tells you | SessionGuard *restores* launchers across runtime changes |
| Scope | ~200 LOC, fits v0.3.3 cleanly | Big. Effectively reimplements parts of nvm/volta/pyenv/pipx |
| Risk | Low | Scope-creep into territory existing tools cover well |
| Recommendation | **Ship now** | Defer until A is in real use and we know if visibility alone is enough |

This entry covers A. B is captured as a reach goal below — likely
folded into v0.4 *if* A doesn't reduce the pain enough on its own.

**Design sketch.**

- Add optional `binary` field to `ToolDefinition`:
  ```toml
  [tool]
  name = "claude_code"
  binary = "claude"             # new — name of the launcher on PATH
  ```
- New `src/health.rs` module:
  - `check_binary(tool: &ToolDefinition) -> BinaryStatus` —
    runs `which <binary>` (or platform equivalent), reports
    `{ Present, Missing, NotConfigured }`.
- Wire into `sessionguard doctor`:
  ```
  ⚠  Claude Code — 18 sessions present in ~/.claude/projects/,
     but `claude` is not on PATH. The launcher may have been lost
     in a Node/runtime upgrade; sessions themselves are intact.
  ```
- Wire into the dashboard Activity tab:
  - New "Launcher" column per row: ✅ present, ⚠️ missing, — n/a
  - Surfaced cross-store so even a "history-only" project tells
    you which tools can still open it
- Reach goal: `sessionguard restore-launcher <tool>` that shells
  out to each tool's documented installer (curl-pipe, brew, etc.).
  Owns no install logic itself — just runs the vendor's. Earns
  `--dry-run` like every other destructive command.

**Scope.** ~150–200 LOC plus tests. Naturally pairs with v0.4
`migrate` (both speak the same "tools have state, runtime, and a
binary on disk" model) but small enough to land standalone as v0.3.3
if real-world bites surface faster than the migrate design lands.

## Dashboard / Activity tab — incremental

The dashboard's read-only Activity view (added in v0.3.2) covers
Claude Code, Codex, and OpenCode today. Natural extensions, all
small enough to land outside the main version track:

- **More assistant stores in Activity** — Cursor, Windsurf, Aider,
  Gemini CLI. Each needs its own per-store discovery logic since
  none of them share Claude Code's "dir-per-project" or Codex's
  "JSONL with `cwd` in line 1" conventions.
- **Sessions tab → Activity drilldown** — clicking a row in
  Sessions filters Activity to that store.
- **Live-update via SSE** — current 3s poll + 30s server-side
  cache is fine for a dev box; an SSE endpoint pushing updates
  on filesystem-event triggers would feel snappier without
  burning CPU on idle pages.
- **Once `baton` ships** — overlay live `baton` sessions onto
  Activity rows so we get true "active right now" instead of the
  current mtime-based "touched within 5 min" heuristic.
- **Cleanup hints** — flag projects whose decoded path no longer
  exists on disk (Claude `ENC` rows are already a hint; could
  add a "stale — N stores still reference this path" pill).
- **Click-through to session content** — read-only, paginated,
  for diagnosing "what did Claude do here?" without leaving the
  browser.

## v1.0 — UI + Polish

Goal: broad audience.

- Tauri-based local UI (native binary, no separate server, no auth
  concerns) — visualise disks/mounts/tools, one-click migrate
- Windows support (notify v8 covers it; main work is path separators +
  reparse points)
- Opt-in telemetry: which tools are actually being reconciled on real
  users' machines
- Show HN / r/rust launch post
- `sessionguard doctor` with real checks — artifact paths valid,
  daemon alive, registry consistent
- Action surface on the dashboard — once `migrate` lands in v0.4,
  promote the Activity tab from read-only to "click any row to
  relocate / undo / archive"

## Deferred / not doing

- CLI TUI browser — nice-to-have, doesn't move the needle
- Plugin marketplace — premature at our size
- Cloud sync of sessions — outside the thesis; different product
- ML "which dir to relocate" — laughable at our size

## Thesis shift (v0.3 → v0.4)

v0.1–v0.3 pitch: *"keeps AI coding sessions intact when your projects
move."* Narrow, specific, passive reconciler.

v0.4+ pitch: *"the tool that moves AI dev environments between disks
without breaking them."* Bigger market — every developer who installs
Ollama + HuggingFace + Docker + Claude + Codex eventually wants hot data
on fast storage and runs out of room on `/home`. Today they hand-edit
configs and cross fingers.

The v0.3 adapter architecture already supports this generalisation —
each new tool is ~30 lines of TOML. `migrate` just adds the data-move
side alongside the path-rewrite side we already have.
