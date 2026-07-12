# SessionGuard hardening audit — footgun register (v0.5.1)

> Recursive whole-codebase audit for footguns, correctness, data-loss, and
> security issues. Compiled 2026-07-12 from three subsystem sweeps + a UX pass.
> This is the correction backlog; work top-down. `file:line` refs are at
> audit time.

> **Status:** Wave 1 (B1, B2, B3, + H8) fixed in **v0.5.2**. Wave 2 (H1–H7)
> fixed in **v0.6.0** (2026-07-12). Wave 3 (MED/LOW) open.

## BLOCKER — fix before more users (data-loss / brick / RCE) — ✅ FIXED v0.5.2

- **B1 · Non-atomic artifact rewrite corrupts session files.**
  `reconciler.rs:271/325/372` do in-place `std::fs::write` (truncate-then-write).
  Crash / power loss / ENOSPC mid-write → truncated/half-written session file,
  unrecoverable (no backup). Violates the product's own "corrupting session state
  is worse than losing it." The daemon's *PID* writer already does temp+rename
  (`daemon.rs:44`) — the *user-data* writer doesn't.
  **Fix:** write to a `.tmp` sibling → fsync → atomic `rename` over the original;
  preserve mode.

- **B2 · `update` swap bricks the binary with no rollback.**
  `update.rs:301-343` — both branches remove the working binary *before* the new
  one lands and don't restore on failure. Disk-full/sudo-expiry mid-copy → no
  `sessionguard` on PATH, fleet-wide.
  **Fix:** copy new binary to a temp in dest's dir → fsync → single atomic
  `rename`; on any error `rename(backup, dest)` to restore. Sudo path: one
  `sudo sh -c` so a partial failure still ends with a working dest.

- **B3 · Update integrity is TLS-to-one-host + a production-honored base-URL seam,
  no signature.** `update.rs:362-414` fetches `SHA256SUMS` from the *same* base as
  the tarball (self-consistent, not authenticity), and `SESSIONGUARD_UPDATE_BASE_URL`
  is honored in production → anyone who sets it (or MITMs a mirror) installs
  arbitrary code as `sessionguard` and restarts it. No GPG/minisign/cosign.
  **Fix:** gate the env override behind `#[cfg(test)]` (or `--allow-custom-base`);
  add a detached signature verified against a compiled-in public key before swap.

## HIGH — ✅ FIXED v0.6.0 (H8 in v0.5.2)

- **H1 · No `busy_timeout`/WAL on any SQLite conn** (`registry.rs:52`,
  `event_log.rs:83`). Daemon+CLI concurrency fails instantly with "database is
  locked"; can drop an undo-log write. **Fix:** `PRAGMA busy_timeout=5000` +
  `journal_mode=WAL` on every open.
- **H2 · Orphaned undo record** (`reconciler.rs:96-99`). File mutated *before*
  `event_log.record()`, whose failure is only `warn!`-ed → irreversible rewrite,
  `undo` can't find it. **Fix:** record intent before mutating, or hard-fail the
  action if logging fails.
- **H3 · `stop` can SIGTERM an unrelated recycled-PID process** and print success
  (`daemon.rs:77-93`, `main.rs:53`). `is_running()` only checks *some* process
  holds the PID. **Fix:** verify exe identity (`/proc/<pid>/exe`, `ps -o comm=`)
  or store+compare start-time.
- **H4 · `watch`/`unwatch` don't change what the daemon watches**
  (`main.rs:95-124`, `daemon.rs:116`). They only write registry rows; the daemon
  watches `config.watch_roots`, with no live reload. A `watch`ed path outside a
  root is never monitored. **Fix:** append to `watch_roots` (persist) + SIGHUP
  reload, or rename to `register`/`track` and document.
- **H5 · migrate leaves the service stopped on abort** (`migrate/mod.rs:1594-1688`).
  Copy/Verify/Rewrite failure paths `return Err` without `quiescer.resume(...)` →
  silent outage on a server. **Fix:** best-effort resume on every abort after a
  real UnitStopped.
- **H6 · macOS rename cross-pairing** (`watcher.rs:166-198`, 500ms FIFO, recursive
  watch). Two independent renames in the window fuse into a bogus `(old,new)` fed
  to the reconciler. **Fix:** shrink window, require path relationship, verify
  `from` gone + `to` exists.
- **H7 · `start --daemon` is a dead flag** (`cli.rs:54`, `main.rs:36`) + no real
  backgrounding — runs foreground, prints "not implemented." **Fix:** implement
  double-fork daemonize + macOS launchd plist, or remove the flag.
- **H8 · `--to` allows arbitrary downgrade** (`main.rs:872`, `update.rs:348`) — no
  `is_newer` guard; downgrade to a known-vulnerable release verifies cleanly.
  **Fix:** require `is_newer` unless `--allow-downgrade`; validate tag `^v?\d+\.\d+\.\d+$`.

## MED

- **M1 · Text adapter global-replace, no boundary guard** (`reconciler.rs:367`),
  wired to the shipping `aider` builtin → incidental path mentions / `code` vs
  `code-backup` in chat history corrupted. Apply the JSON/TOML boundary check.
- **M2 · `export`/`import` silently drop all artifacts + tool associations**
  (`main.rs:364`, `registry.rs:163` hardcodes empty). "Backup" loses the core data.
  Export the full graph.
- **M3 · Verify only counts files+bytes** (`migrate/mod.rs:643`) — can't detect a
  same-size corruption/bit-flip; ignores symlinks. Hash per-file. (Contained: source
  is renamed aside, never deleted.)
- **M4 · `scan` is shallow (depth-1) + empty-roots silent no-op** (`main.rs:126`).
  Nested/monorepo projects missed; no guidance when no roots. Recursive bounded
  walk + `--depth` + actionable empty-state.
- **M5 · Default watch_roots home-locked** (`config.rs:58` — `~/{projects,repos,code,dev}`).
  Doesn't fit everyone. Add onboarding/`init` that offers a bounded `$HOME` scan.
- **M6 · HOME-unset → cwd-relative `./.sessionguard`** (`config.rs:103`,
  `daemon.rs:19`). PID/registry differ per working dir; double-start protection
  breaks. Hard-error if no stable data dir; unify home resolution
  (`config.rs` BaseDirs vs `inventory.rs:121` `$HOME`).
- **M7 · `write_pid` TOCTOU → two daemons** (`daemon.rs:28-48`). Check-then-write
  isn't exclusive; `PidGuard` may delete the winner's file. Use `create_new` lock
  / `flock`; guard removes only if PID matches.
- **M8 · Daemon never reloads config / no SIGHUP** (`daemon.rs:96`). Editing config
  has no effect until restart. Add SIGHUP reload.
- **M9 · Undo logs `pairs[0]` not the matched pair** (`reconciler.rs:93`). Undo
  no-ops on `/private`,`/var`,`/tmp` projects yet marks undone. Return+log the
  applied pair.
- **M10 · TOML/JSON reserialization loses comments/formatting** (`reconciler.rs:267/321`).
  Use `toml_edit`; preserve trailing newline.
- **M11 · Registry paths uncanonicalized** (`registry.rs:102`) → stale rows on
  trailing-slash/symlink/case/NFC-NFD variants; `to_string_lossy` corrupts non-UTF8.
  Normalize before store/lookup.
- **M12 · macOS case-insensitive/unicode silent no-rewrite** (`reconciler.rs:237`).
  Case/normalization-aware compare on macOS.
- **M13 · Predictable `/tmp` update workdir** (`update.rs:394`) — symlink attack on
  the download. `mkdtemp` 0700.
- **M14 · `restart_daemon` only systemd --user; macOS launchd stale post-update**
  (`update.rs:278`); migrate quiesce is systemd-only too. Detect launchd / print
  manual-restart note on non-systemd.
- **M15 · Install-method substring classification misfires** (`update.rs:69`) — a
  nonstandard brew prefix → overwritten; a path containing `/homebrew/` → refused.
  Anchor on real prefixes + ownership.
- **M16 · Recursive watch inotify-limit exhaustion** (`watcher.rs:111`). Big tree
  blows `fs.inotify.max_user_watches`, events silently dropped. Count + warn with
  the sysctl fix.
- **M17 · Unbounded event-log growth** (`event_log.rs`). Retention/prune/vacuum.
- **M18 · Unbounded `read_to_string`** (`reconciler.rs:256/310/368`) — huge aider
  history into RAM; non-UTF8 hard-errors and aborts the whole tool loop. Size-cap;
  skip non-UTF8 per-file.

## LOW

- L1 `kill()` return ignored → false "sent stop signal" (`main.rs:53`).
- L2 `unwatch` on a moved/deleted path no-ops but prints success (`main.rs:121`).
- L3 `RUST_LOG` silently overrides `--verbose` (`main.rs:21`).
- L4 `config edit` doesn't re-validate the TOML (`main.rs:400`).
- L5 `copy_tree` doc says "symlinks skipped" but it recreates them (`migrate/mod.rs:480`).
- L6 Error variants drop path/op context (`error.rs:26`, `migrate/mod.rs:163`).
- L7 Inventory/verify dir stack unbounded; file-cap doesn't bound dir count
  (`inventory.rs:130`, `migrate/mod.rs:1937`).
- L8 Config-backup restore silent on partial failure (`migrate/mod.rs:904`).

## What is safe (verified)

- Panic surface clean: all `unwrap`/`expect`/`as`-truncation hits are in
  `#[cfg(test)]`; the one prod `unwrap` is an idiomatic mutex lock. No reachable
  prod panic found.
- Migrate's "never auto-delete source" holds throughout — Copy/Verify are
  read-only on source; disk-full/permission mid-copy cleans dst and leaves source
  intact; Retain renames (never removes). The stubbed Snapshot stage records intent
  only and nothing downstream assumes it ran.

## Suggested correction waves

1. **Wave 1 — BLOCKERs, ship as v0.5.2 immediately:** B1 atomic artifact write ·
   B2 atomic swap+rollback · B3 close the update-integrity seam (+ H8 downgrade
   guard rides along). Small, data-loss/security, urgent.
2. **Wave 2 — HIGH correctness:** H1 WAL/busy_timeout · H2 undo ordering · H3 PID
   identity · H5 migrate resume-on-abort · H6 rename pairing · H4 watch/reload
   model · H7 real backgrounding.
3. **Wave 3 — "works for everyone" + polish:** M4 recursive scan · M5 onboarding/
   defaults · M1 text boundary · M2 export/import graph · M11 canonicalization ·
   M16 inotify guard · M10 comment-preserving edits · the rest of MED/LOW.
