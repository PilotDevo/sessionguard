# Design: `sessionguard update` — self-update across the fleet (v0.5?)

> **Status**: Design draft. No code yet. Land this doc first, take a beat,
> then implement — same cadence as `migrate.md` (→ v0.4) and `handoff.md`.
> Reviewers: feedback as GitHub issues. Last revised: 2026-06-25
> (v0.4.3 baseline).

## Thesis

SessionGuard runs on a fleet — Mac, `DOLOAMD`, `fedora` — and keeping every
box on the same version is currently manual and error-prone. Concrete proof:
on 2026-06-25 the `fedora` hub was found running **0.3.12** while the rest of
the fleet was on **0.4.3** — four minor versions and the entire v4 `migrate`
feature set behind, and nobody noticed until we SSH'd in to check.

`sessionguard update` turns "is every box current?" from a chore into one
command: `sessionguard update --check` on any machine tells you if you're
behind; `sessionguard update` brings you current. It is **not** a new
distribution channel — it reuses the existing GitHub releases that `install.sh`
and the Homebrew tap already consume.

It also pairs naturally with the existing `health` module (which already
reasons about tool binaries and launchers) and the `--dry-run` discipline every
mutating command already honors.

## Ground truth from the fleet (what the design must handle)

From the real `fedora` install, 2026-06-25:

- Binary at **`/usr/local/bin/sessionguard`, owned by root** (installed via
  `install.sh`, which prefers `/usr/local/bin` and uses `sudo` when it isn't
  writable). → **the updater must handle a non-writable install dir (sudo).**
- The daemon is **active as a systemd `--user` service** (`start --foreground`
  under the unit). → **the updater must stop/restart the running daemon.**
- 0.3 → 0.4 changed the SQLite registry schema (added the `migrations` table +
  `cleaned_at`). Schema **auto-migrates on open** and the additions are
  backward-compatible (older code ignores unknown tables/columns). → **an
  upgrade is safe; a same-session downgrade is also safe.** The updater doesn't
  own migrations, but `--check` should *warn* when crossing a minor that
  changes schema so the operator knows a daemon restart will migrate.

## The command

```bash
sessionguard update            # upgrade to the latest release (if newer)
sessionguard update --check    # report current vs latest; exit 0/non-zero; no changes
sessionguard update --dry-run  # show exactly what would happen; touch nothing
sessionguard update --to 0.4.3 # pin a specific version (downgrade/rollback)
```

`--check` is the fleet-health primitive — cheap, read-only, scriptable (a cron
on each box could surface drift). `--dry-run` prints the resolved install
method, the asset URL, the swap path, and whether a sudo/daemon-restart would
be needed.

## Install-method detection (the crux)

The updater must **not fight the package manager**. It self-replaces only
binaries it owns; for everything else it defers with the right command. Detect
by inspecting the running binary's own path (`std::env::current_exe()`):

| Detected method | Heuristic | Action |
|---|---|---|
| **Homebrew** | path under `$(brew --prefix)` / `/opt/homebrew` / `/usr/local/Cellar` | Defer: print `brew upgrade sessionguard`, exit |
| **Cargo** | path under `~/.cargo/bin` | Defer: print `cargo install sessionguard --force`, exit |
| **Standalone** | `/usr/local/bin`, `~/.local/bin`, or `$SESSIONGUARD_INSTALL_DIR` (the `install.sh` targets) | **Self-update** (the path below) |
| **Git checkout / dev** | path under a `target/{debug,release}` | Refuse: print "running a dev build; rebuild from source", exit |
| **Unknown** | none of the above | Refuse with a clear message + manual instructions |

Only the **Standalone** row does an actual binary swap. This keeps the blast
radius tiny and avoids corrupting a Homebrew/cargo-managed install.

## The update state machine

Mirrors `migrate`'s shape (preflight → act → verify) for the same auditability:

```
0. detect    resolve install method (above); refuse non-standalone
1. check     fetch latest release tag; compare to own version; stop if current
2. resolve   pick the asset for this OS/arch (the install.sh naming scheme:
             sessionguard-<arch>-<os>.tar.gz)
3. download   fetch the tarball + the SHA256SUMS asset to a temp dir
4. verify    check the asset's SHA256 against SHA256SUMS (REFUSE on mismatch)
5. quiesce   if the daemon is running, stop it (systemd --user stop, else SIGTERM)
6. swap      atomically replace the binary: write alongside, fsync, rename;
             keep the previous binary as <bin>.bak-<version> for rollback;
             use sudo iff the install dir isn't writable
7. resume    restart the daemon if step 5 stopped it
8. verify    run `<new bin> version`; confirm it matches the target; on failure
             roll back from <bin>.bak-<version> and restart
```

Atomic-rename swap means a crash mid-update never leaves a half-written binary
on `PATH`. Keeping `<bin>.bak-<version>` gives a one-command rollback
(`update --to <old>` re-fetches, but the local `.bak` is the instant path).

## Source of truth + integrity

- **Releases live on GitHub** (`PilotDevo/sessionguard`); the asset naming
  already exists (`install.sh` builds the same URLs). The version oracle is the
  `releases/latest` API (what `install.sh` already calls), with crates.io as a
  cross-check.
- **The `self_update` crate** handles fetch + replace against GitHub releases
  and is the likely implementation backbone (~150 LOC of glue), *if* its
  verification story is sufficient; otherwise hand-roll steps 3–6.

### Prerequisite: publish a `SHA256SUMS` release asset

**Today there is no published checksum file.** `release.yml` computes per-asset
SHA256s but only injects them into the Homebrew formula (lines ~143–148);
`install.sh` downloads the tarball and verifies **nothing**. A self-updater that
downloads and *executes* a replacement binary without integrity checking is a
supply-chain hole.

So step 0 of *implementation* (before the command) is: **add a `SHA256SUMS`
asset to the GitHub release** in `release.yml`, and teach both `install.sh` and
`update` to verify against it. This hardens the existing curl-pipe installer as
a bonus.

## What I'm explicitly NOT building in v1

- **No background auto-update.** `update` is operator-invoked. (A future
  `--check` cron is the operator's to wire, not a daemon behavior.)
- **No Windows.** Same Unix-only stance as `migrate`/`handoff`.
- **No fleet-wide push.** Each box updates itself; orchestrating "update all
  three machines" is a shell loop over SSH, not SessionGuard's job.
- **No new download infrastructure.** GitHub releases only; no mirror, no CDN.
- **No partial/delta updates.** Whole-binary swap; the binary is a few MB.

## Open questions to resolve before code

1. **`self_update` crate vs hand-rolled.** Does `self_update` verify checksums
   the way we want, and does it cleanly handle the sudo-needed + daemon-restart
   cases? If not, hand-roll steps 3–6 (it's not much code).
2. **Sudo UX.** When the install dir needs root, do we shell out to `sudo` for
   the swap (prompting interactively) or refuse and print the `sudo` command for
   the operator to run? Leaning: attempt `sudo` with a clear prompt, `--dry-run`
   shows it's needed.
3. **Daemon detection.** systemd `--user` is the fedora reality, but a bare
   `start --foreground` (no unit) or a `--daemon` fork also exist. Reuse the PID
   file + `systemctl --user` probe; fall back to PID-file SIGTERM.
4. **Version oracle.** `releases/latest` (GitHub) vs crates.io `index`. GitHub
   is canonical for the binary; crates.io can lag the tag. Use GitHub.
5. **Rollback retention.** How many `.bak-<version>` binaries to keep (1?), and
   does `update` prune old ones.
6. **MSRV-of-the-installed-binary is irrelevant** (we ship a built binary), but
   `--check` could note when the *latest* needs a newer glibc than the host.

## Implementation order (rough)

1. **`SHA256SUMS` in `release.yml`** + verify in `install.sh` (prerequisite;
   ships independently, hardens the installer now).
2. **`update --check`** — read-only: detect method, fetch latest tag, compare,
   print + exit code. Useful immediately as a fleet-drift probe.
3. **Install-method detection module** — `current_exe()` classification + tests
   (table above), the gate everything else depends on.
4. **Download + verify** — asset resolve, fetch, SHA256 check (REFUSE on
   mismatch). Reuse for `install.sh` parity if practical.
5. **Swap + rollback** — atomic rename, `.bak-<version>`, sudo-if-needed.
6. **Daemon stop/restart** — reuse the `migrate` quiesce knowledge (systemd
   `--user`), PID-file fallback.
7. **`update-dogfood.sh`** — CI smoke that fakes a "newer" release in a temp dir
   and asserts a standalone swap + rollback works (no network: point the asset
   URL at a local file).

## Acceptance criteria

update v1 ships when:

- [ ] `release.yml` publishes `SHA256SUMS`; `install.sh` verifies it.
- [ ] `update --check` reports current-vs-latest and exits non-zero when behind,
      on a standalone, cargo, and homebrew install (deferring correctly for the
      latter two).
- [ ] `update` on a standalone install swaps to the latest, restarts a running
      systemd `--user` daemon, and `version` confirms the new version.
- [ ] A corrupted/mismatched download is **refused** (SHA check) and leaves the
      existing binary untouched.
- [ ] A failed post-swap `version` check auto-rolls-back from `.bak-<version>`.
- [ ] `--dry-run` makes zero changes and prints the resolved plan.
- [ ] Homebrew/cargo installs are never silently overwritten — they defer.

## Why commit to this design before code

1. **Integrity-first.** The one-way door here is executing a downloaded binary.
   The `SHA256SUMS` prerequisite is non-negotiable and must land *before* any
   self-replacement code exists.
2. **Don't fight the package manager.** Baking method-detection + deferral into
   the contract prevents a self-updater that corrupts brew/cargo installs.
3. **Reversible like everything else.** `.bak-<version>` + auto-rollback keeps
   the project's "never strand the operator" promise — the update analogue of
   `migrate`'s never-auto-delete rule.

This doc is the contract. Code that contradicts it bounces back here.
