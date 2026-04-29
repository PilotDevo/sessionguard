# SessionGuard Roadmap

Living document. Version milestones capture the thesis arc; exact feature
ordering inside each milestone may shift based on what proves most useful
in real-world dogfooding.

## Where we are

**v0.3.2 (current)** — Daemon reliably detects moves on macOS and Linux
(proven via `scripts/dogfood.sh` on real hardware) and reconciles seven
built-in tools (five reconciling, two detect-only). `sessionguard undo`
reverses any logged action from the event log. `--format json` available
on `tools`, `log`, and `status` for tooling integration.

The local read-only **dashboard** (`tools/dashboard/`) now ships with an
**Activity** tab that gives a per-project, per-assistant view across
Claude Code, Codex, and OpenCode session stores — answering "where am I
working, and which assistants have touched what?" at a glance.

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

## v0.4 — Migrate

Goal: ship the feature conversation suggested by the fedora fastpool work.
Turn SessionGuard from *"watches for moves"* into *"the tool that moves
AI dev environments safely."*

Concrete target — fedora hub box:
`~/.codex/sessions/` (14 GB) + `~/.claude/projects/` (3.8 GB) +
`~/droco-mem-data/` (18 GB) → `/mnt/fastpool/<target>` without losing
session state.

- `sessionguard migrate <tool> --to <path>` — tool-aware relocation
  (stop related services → rsync → rewrite config → restart → verify)
- `sessionguard relocate <src> <dst>` — path-aware; scan all registered
  tools for references to `<src>`, move data, rewrite references
- `sessionguard inventory` — enumerate tracked tools, their data
  locations, sizes; suggest migration candidates
- btrfs snapshot integration — on btrfs roots, take a snapshot before
  migrating for atomic rollback
- `--dry-run` on every destructive command as a first-class pattern
- Docs site (MkDocs Material, not Docusaurus — lighter)

## v0.5 — Tool Pattern Library

Goal: let the community extend the pattern catalog safely.

- Community tool-definition contribution model (separate repo or
  `contrib/tools/` dir)
- `sessionguard tools validate <toml>` — lint contributor TOMLs
- Docs: contributing guide, pattern authoring cookbook
- `cargo-audit` integration in CI
- Homebrew formula auto-publish workflow

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
