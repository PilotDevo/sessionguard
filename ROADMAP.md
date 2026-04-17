# SessionGuard Roadmap

Living document. Version milestones capture the thesis arc; exact feature
ordering inside each milestone may shift based on what proves most useful
in real-world dogfooding.

## Where we are

**v0.3 (current)** — *Undo + more tool patterns.* The daemon reliably
detects moves on macOS and Linux (proven via `scripts/dogfood.sh` on real
hardware) and reconciles five built-in tools. `sessionguard undo` reverses
any logged action from the event log.

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
