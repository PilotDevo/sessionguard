# Contributing to SessionGuard

Thanks for your interest in contributing! SessionGuard is MIT-licensed and contributions are welcome from day one.

## Getting Started

```bash
git clone https://github.com/PilotDevo/sessionguard.git
cd sessionguard
cargo build
cargo test
```

## Development Workflow

We use [just](https://github.com/casey/just) as a command runner:

```bash
just check      # fmt + clippy + test
just test       # run tests
just lint       # clippy with -D warnings
just fmt        # format code
just run        # run daemon in foreground with debug logging
```

## Commit Convention

We follow [Conventional Commits](https://www.conventionalcommits.org/):

- `feat:` — new feature
- `fix:` — bug fix
- `docs:` — documentation
- `refactor:` — code restructuring
- `test:` — test additions/changes
- `chore:` — maintenance (CI, deps, tooling)

Examples:
```
feat(detector): add windsurf session pattern support
fix(reconciler): handle symlinks in path rewriting
docs: update supported tools table
```

## High-Impact Contributions

- **Tool pattern definitions** — Add TOML files to `src/tools/builtin/` for new AI tools
- **Platform support** — Windows filesystem watching
- **Reconciliation logic** — Tool-specific path rewriting edge cases
- **Testing** — Test fixtures for tool session formats

## Adding a New Tool

1. Create `src/tools/builtin/your_tool.toml` following the existing pattern
2. Update `src/tools/mod.rs` to include the new built-in
3. Add tests in `src/detector.rs`
4. Update the supported tools table in `README.md`

## Code Quality

- All PRs must pass `cargo clippy -- -D warnings`
- All PRs must pass `cargo fmt -- --check`
- All PRs must pass `cargo test`
- Prefer integration tests over unit tests for CLI behavior
