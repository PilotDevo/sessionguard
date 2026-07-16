#!/usr/bin/env sh
# Copyright 2026 Devin R O'Loughlin / Droco LLC
# SPDX-License-Identifier: MIT
#
# SessionGuard installer
# Usage: curl -fsSL https://raw.githubusercontent.com/PilotDevo/sessionguard/main/install.sh | sh
#
# Options (env vars):
#   SESSIONGUARD_VERSION  — install a specific version (default: latest)
#   SESSIONGUARD_INSTALL_DIR — install location (default: /usr/local/bin, fallback: ~/.local/bin)

set -e

REPO="PilotDevo/sessionguard"
BINARY="sessionguard"

# ── Helpers ───────────────────────────────────────────────────────────────────

say()  { printf '\033[1;32m==> \033[0m%s\n' "$*"; }
warn() { printf '\033[1;33mwarn:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

need() {
    command -v "$1" >/dev/null 2>&1 || die "Required tool not found: $1 — please install it and retry."
}

# Print the SHA-256 of a file using whichever tool is available.
sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        die "Need 'sha256sum' or 'shasum' to verify the download integrity."
    fi
}

# ── Platform detection ────────────────────────────────────────────────────────

detect_target() {
    OS="$(uname -s)"
    ARCH="$(uname -m)"

    case "$OS" in
        Linux)
            case "$ARCH" in
                x86_64)  echo "x86_64-unknown-linux-gnu" ;;
                # No prebuilt ARM-Linux binary is published (release builds
                # x86_64-linux + both macOS targets only) — advertising the
                # triple here used to produce a guaranteed download 404.
                aarch64|arm64) die "No prebuilt binary for ARM Linux yet. Use: cargo install $BINARY" ;;
                *) die "Unsupported Linux architecture: $ARCH. Try: cargo install $BINARY" ;;
            esac
            ;;
        Darwin)
            case "$ARCH" in
                x86_64)  echo "x86_64-apple-darwin" ;;
                arm64)   echo "aarch64-apple-darwin" ;;
                *) die "Unsupported macOS architecture: $ARCH" ;;
            esac
            ;;
        *)
            die "Unsupported OS: $OS. Try: cargo install $BINARY"
            ;;
    esac
}

# ── Version resolution ────────────────────────────────────────────────────────

resolve_version() {
    if [ -n "$SESSIONGUARD_VERSION" ]; then
        echo "$SESSIONGUARD_VERSION"
        return
    fi

    need curl

    VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name"' \
        | sed 's/.*"tag_name": *"\(.*\)".*/\1/')"

    [ -n "$VERSION" ] || die "Could not determine latest release version."
    echo "$VERSION"
}

# ── Install directory ─────────────────────────────────────────────────────────

resolve_install_dir() {
    if [ -n "$SESSIONGUARD_INSTALL_DIR" ]; then
        echo "$SESSIONGUARD_INSTALL_DIR"
        return
    fi

    # Prefer /usr/local/bin if writable, otherwise fall back to ~/.local/bin
    if [ -w "/usr/local/bin" ]; then
        echo "/usr/local/bin"
    elif [ -d "$HOME/.local/bin" ] || mkdir -p "$HOME/.local/bin" 2>/dev/null; then
        echo "$HOME/.local/bin"
    else
        die "Cannot determine a writable install directory. Set SESSIONGUARD_INSTALL_DIR."
    fi
}

# ── Main ──────────────────────────────────────────────────────────────────────

main() {
    need curl
    need tar

    TARGET="$(detect_target)"
    VERSION="$(resolve_version)"
    INSTALL_DIR="$(resolve_install_dir)"
    DEST="$INSTALL_DIR/$BINARY"

    say "Installing $BINARY $VERSION for $TARGET"
    say "Destination: $DEST"

    URL="https://github.com/${REPO}/releases/download/${VERSION}/${BINARY}-${TARGET}.tar.gz"
    TMPDIR="$(mktemp -d)"
    trap 'rm -rf "$TMPDIR"' EXIT

    say "Downloading $URL"
    curl -fsSL "$URL" -o "$TMPDIR/${BINARY}.tar.gz" || \
        die "Download failed. Check that release $VERSION has a binary for $TARGET."

    # Verify integrity against the release's SHA256SUMS asset. Releases from
    # v0.5.0 on publish it; for older releases that predate it we warn and
    # proceed rather than fail (back-compat).
    ASSET="${BINARY}-${TARGET}.tar.gz"
    SUMS_URL="https://github.com/${REPO}/releases/download/${VERSION}/SHA256SUMS"
    if curl -fsSL "$SUMS_URL" -o "$TMPDIR/SHA256SUMS" 2>/dev/null; then
        expected="$(awk -v f="$ASSET" '$2 == f {print $1}' "$TMPDIR/SHA256SUMS" | head -n1)"
        [ -n "$expected" ] || die "SHA256SUMS has no entry for $ASSET — refusing to install."
        actual="$(sha256_of "$TMPDIR/${BINARY}.tar.gz")"
        [ "$expected" = "$actual" ] || \
            die "Checksum mismatch for $ASSET: expected $expected, got $actual. Aborting."
        say "Checksum verified ($ASSET)"
    else
        warn "No SHA256SUMS published for $VERSION — skipping integrity check (older release)."
    fi

    tar -xzf "$TMPDIR/${BINARY}.tar.gz" -C "$TMPDIR"
    chmod +x "$TMPDIR/$BINARY"

    # Use sudo only if install dir requires it
    if [ -w "$INSTALL_DIR" ]; then
        mv "$TMPDIR/$BINARY" "$DEST"
    else
        say "Requesting sudo to install to $INSTALL_DIR"
        sudo mv "$TMPDIR/$BINARY" "$DEST"
    fi

    say "Installed $BINARY to $DEST"

    # Warn if install dir is not in PATH
    case ":$PATH:" in
        *":$INSTALL_DIR:"*) ;;
        *) warn "$INSTALL_DIR is not in your PATH. Add it to your shell profile." ;;
    esac

    # Verify the binary we just INSTALLED — not whatever `sessionguard` PATH
    # happens to resolve to (a stale copy earlier in PATH would report the
    # wrong version and mask a half-successful install).
    say "All done! $("$DEST" --version)"
    echo ""
    echo "  Quick start:"
    echo "    sessionguard init                   # find your projects, set up watch roots"
    echo "    sessionguard start                  # run the daemon (backgrounds itself)"
    echo "    sessionguard status                 # check what's tracked"
    echo "    sessionguard --help                 # all commands"
    echo ""
}

main "$@"
