# Contributing to SessionGuard

Thanks for your interest in contributing! SessionGuard is MIT-licensed and contributions are welcome.

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

## Branching Model

- **`main`** — stable, protected. CI must pass to merge.
- **`feat/*`** — new features (e.g., `feat/windsurf-pattern`)
- **`fix/*`** — bug fixes (e.g., `fix/canonicalize-on-macos`)
- **`docs/*`** — documentation changes
- **`chore/*`** — maintenance and tooling

All merges to `main` go through pull requests. Force pushes to `main` are blocked.

## Versioning

We follow [Semantic Versioning](https://semver.org/):

- **Patch** (0.2.1) — bug fixes, doc fixes, CI fixes
- **Minor** (0.3.0) — new features, non-breaking changes
- **Major** (1.0.0) — stable API, breaking changes

Tags trigger the full release pipeline: binary builds → GitHub Release → crates.io → Homebrew tap update.

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

## Pull Request Process

1. Fork the repo and create your branch from `main`
2. Make your changes with conventional commits
3. Run `just check` (or `cargo fmt && cargo clippy -- -D warnings && cargo test`)
4. Open a PR — CI will run automatically
5. All status checks must pass before merge

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

Or — drop a `.toml` file in `~/.config/sessionguard/tools/` to test without modifying the codebase.

## Code Quality

- All PRs must pass `cargo clippy -- -D warnings`
- All PRs must pass `cargo fmt -- --check`
- All PRs must pass `cargo test`
- Prefer integration tests over unit tests for CLI behavior
