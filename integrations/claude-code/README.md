# SessionGuard — Claude Code integration

Optional companion files for driving SessionGuard from
[Claude Code](https://claude.ai/code). These are **not** part of the core
`cargo build` — SessionGuard is a standalone CLI/daemon and needs none of this
to work. This directory just lets an agent run the migration workflow with the
tool's intended discipline.

## What's here

```
integrations/claude-code/
└── skills/
    └── sessionguard-migrate/
        └── SKILL.md   — the migration workflow + safety rules
```

### `sessionguard-migrate` skill

Teaches Claude Code to run the home-dir migration workflow — `inventory` →
`migrate --dry-run` → confirm → `migrate` → verify → `undo` / `migrate-cleanup`
— and to honor the safety rules baked into the tool: always preview with
`--dry-run`, the original is preserved (never auto-deleted), `undo` reverses a
migration, cleanup is irreversible and explicit, and credential files are never
printed.

It drives the existing CLI (using `--format json` where structured output
helps); it does not require any change to SessionGuard itself.

## Installing the skill

Claude Code discovers skills under `~/.claude/skills/`. Symlink (so it tracks
this repo) or copy:

```bash
# symlink — stays in sync with the repo
ln -s "$(pwd)/integrations/claude-code/skills/sessionguard-migrate" \
      ~/.claude/skills/sessionguard-migrate

# or copy — pin a snapshot
cp -r integrations/claude-code/skills/sessionguard-migrate \
      ~/.claude/skills/sessionguard-migrate
```

Then ensure `sessionguard` is on `PATH` (`cargo install --path .`, the
`install.sh` curl-pipe, or a release binary). The skill falls back to
`cargo run --release --` when used from inside this repo.

## Roadmap (not built)

- **MCP server** — expose `inventory`/`migrate`/`undo`/`migrate-cleanup`/`log`/
  `status` as typed MCP tools with gating on the destructive ops, for any MCP
  client. Ecosystem-neutral; the natural next step if non-CLI agent drive is
  wanted.
- **Plugin bundle** — package the skill + slash commands (`/sg-migrate`,
  `/sg-status`) (+ the MCP server) as one installable Claude Code plugin.
