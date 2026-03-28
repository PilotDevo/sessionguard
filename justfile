# SessionGuard development commands

# Default: run checks
default: check

# Run all checks (fmt, clippy, test)
check: fmt-check lint test

# Build debug binary
build:
    cargo build

# Build release binary
release:
    cargo build --release

# Run tests
test:
    cargo test

# Run tests with output
test-verbose:
    cargo test -- --nocapture

# Run a single test by name
test-one NAME:
    cargo test {{NAME}} -- --nocapture

# Run clippy lints
lint:
    cargo clippy -- -D warnings

# Check formatting
fmt-check:
    cargo fmt -- --check

# Format code
fmt:
    cargo fmt

# Run cargo-deny checks (license, advisory)
deny:
    cargo deny check

# Generate shell completions
completions SHELL="zsh":
    cargo run -- completions {{SHELL}}

# Run the daemon in foreground with debug logging
run:
    RUST_LOG=debug cargo run -- start --foreground

# Show CLI help
help:
    cargo run -- --help

# Clean build artifacts
clean:
    cargo clean

# Generate changelog from conventional commits
changelog:
    git cliff -o CHANGELOG.md

# Bump version (patch/minor/major)
bump LEVEL="patch":
    cargo release {{LEVEL}} --no-publish --execute
