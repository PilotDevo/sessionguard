# Changelog

All notable changes to SessionGuard will be documented in this file.

## [Unreleased]

### Miscellaneous

- Documentation polish and housekeeping

## [0.2.0] - 2026-03-30

### Bug Fixes

- Disable git-cliff GitHub remote auto-detection
- Use --allow-dirty and env var for cargo publish

### Documentation

- Update README and CLAUDE.md for v0.2 state (#11)

### Features

- Add install script, systemd service, issue templates, and SECURITY.md
- Wire full runtime tool loading chain (#8)

### Miscellaneous

- Add copyright headers to all source files
- Add Claude Code project hooks and update CLAUDE.md

### Refactor

- Adapter-based reconciliation with JSON/TOML parsers (#10)
- Store actual artifact file paths in registry (#9)

### Testing

- Add end-to-end reconciliation proof tests (#12)

## [0.1.0] - 2026-03-29

### Bug Fixes

- *(ci)* Update deny.toml for cargo-deny v2 format and add MPL-2.0 license

### Documentation

- Update README for accuracy, add sandbox tests and funding
- Fix droco.io link in README footer
- Activate GitHub Sponsors link

### Features

- Initial scaffold for SessionGuard
- Wire reconciliation pipeline and isolate test registry

### Miscellaneous

- Ignore MCP tool artifacts, remove stray playwright-mcp log
- Wire crates.io publish to release workflow, fix author email
- Remove GITHUB_REPO from git-cliff to fix 403 on changelog generation

### Build

- *(deps)* Bump notify from 7.0.0 to 8.2.0
- *(deps)* Bump toml from 0.8.23 to 1.1.0+spec-1.1.0
- *(deps)* Bump actions/upload-artifact from 4 to 7
- *(deps)* Bump rusqlite from 0.32.1 to 0.39.0
- *(deps)* Bump actions/checkout from 4 to 6
- *(deps)* Bump actions/download-artifact from 4 to 8
- *(deps)* Bump directories from 5.0.1 to 6.0.0


