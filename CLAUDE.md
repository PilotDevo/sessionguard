@AGENTS.md

## Claude Code specifics

Hooks in `.claude/settings.json`:

- **PostToolUse (`Edit|Write`)**: when the edited path ends in `.rs`, runs `cargo fmt` on the
  whole crate in the session's directory (errors are silenced). Other files: a `jq` parse only.
- **Stop**: runs `cargo clippy -- -D warnings | tail -8` at the end of every turn (60 s
  timeout). It always exits 0, so it never blocks and its output shows only in the transcript
  view, not to Claude. Run `just check` yourself before committing.

`integrations/claude-code/skills/sessionguard-migrate/` is a product skill for SessionGuard's
users. It is not loaded for work on this repo.
