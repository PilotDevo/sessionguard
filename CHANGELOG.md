# Changelog

All notable changes to SessionGuard will be documented in this file.

## [0.8.1] - 2026-09-05

### Fixed — post-merge review of the v0.8.0 patch

A ten-angle code review of v0.7.0..v0.8.0 (line-by-line, removed behaviour,
cross-file, adapters, efficiency, conventions, altitude, Rust pitfalls, …)
converged on one regression and a set of silent-zero edge cases in the new
census. Everything below is read-only census behaviour except where noted.

- **Live projects with `_`, `.` or a space in their path were reported as
  orphans** (the v0.8.0 regression: `…/work/my-app  [ORPHANED?]` for a live
  `~/work/my_app`, and `--orphans` would have steered cleanup at it). Claude
  Code encodes a store directory by replacing *every* non-alphanumeric
  character with `-`, not just `/`, so the 0.8.0 decoder — literal joins
  validated against the filesystem — could never match such a name and
  folded it onto the parent as a best-guess orphan. Two fixes:
  1. `encoded_dir` stores can declare a **key hint** (`key_glob` +
     `key_field`). The Claude Code builtin declares `*.jsonl` / `cwd`: the
     literal path each transcript records is read (bounded scan of the
     leading lines, newest file first, cross-checked against the directory
     name so a stray file cannot re-key a directory) and the directory name
     is never decoded when a transcript is present. On real stores 9 of 10
     transcripts carry it within the first 22 lines.
  2. The name decoder is **encoding-aware**: at each level it also accepts a
     real child directory whose *encoding* equals the next segment(s), so
     hint-less directories for `my_app`, `app.v2`, `LARS Docs` and `.config`
     decode `Exact`. A name that two live directories both encode to
     (`my_app` beside `my-app`) is `Inferred`, not silently one of them.
- **`--home` (a foreign root) validated names against the local
  filesystem** — the wrong one. A key hint now yields the recorded path;
  everything else is a naive decode shown as `[INFERRED]`. A `--home` that
  does not exist is an error instead of an empty census exiting 0.
- **Codex sessions silently missing** when `$HOME`/`--home`/a declared path
  contained a glob metacharacter (`[`, `*`, `?`) or the declaration ended in
  `/`: the glob is matched against each file's path relative to the store
  root instead of being spliced together with the root.
- **`--project` never matched the path `sessions` itself printed** on macOS
  (`/var/…` vs `/private/var/…`) or through a symlink; it accepts the
  recorded spelling, a trailing slash and the canonical form.
- **OpenCode rows were dropped** when `time_updated` held a TEXT or REAL
  value; a seconds-declared column whose values can only be milliseconds is
  read as ms with one warning instead of every session showing `0s ago`.
- **Fleet: a reachable host whose login shell could not find `sessionguard`
  was reported "unreachable".** ssh's own failures (exit 255) are now told
  apart from the remote command's status; exit 127 says so and points at the
  new per-host `binary = "/absolute/path"` option. `--version` is parsed
  only from a `sessionguard X.Y.Z` line (an MOTD's `Fedora 42.0.1` no longer
  passes as the version). `--` precedes the ssh destination in addition to
  the leading-`-` refusal, and `binary` is restricted to a plain path before
  it reaches the remote shell.
- **`--all-hosts` died on a local failure** (a broken user tool TOML) before
  censusing any remote host; the local census is one member of the fleet —
  its failure is a warning that counts toward the incomplete exit code.
- **A user/project tool override written before 0.8 silently dropped the
  builtin's `session_store`** (and `home_dir_layout`/`binary`):
  `ToolRegistry::register` inherits those blocks when the override omits
  them.
- **Store declarations are validated at load** — empty `separator`,
  unparseable glob, half-declared key hint, unsafe SQL `table`/`path_column`,
  empty `path` — with the offending file named, instead of censusing zero
  sessions silently. `[[hosts]]` is validated too: the name `local` is
  reserved, duplicates and empty fields are rejected.
- **Env-discovered stores are honoured**: after `sessionguard migrate codex`
  sets `CODEX_HOME`, `sessions` reads `$CODEX_HOME/sessions` (local census
  only; another machine's layout is not this machine's environment).
- **Dashboard**: `confidence` is derived from `decoded` for 0.7.x binaries
  instead of defaulting to `exact`; an Inferred orphan renders `orphaned?`,
  never the confirmed `orphaned`; the Activity table grows a column for any
  user-declared store and shows fleet host provenance.
- Text output: every `(confidence, orphaned)` state has a marker
  (`[INFERRED]` is new) and a legend explains them. JSONL first-line reads
  are bounded (64 KiB); the store-walk cap warns when hit. `claude_code` and
  `gemini_cli` declare `on_move = "notify"` — no in-project path field exists
  to rewrite, so `rewrite_paths` was a no-op claim. A failed symlink install
  during `migrate` now reports whether the original was actually moved back.

## [0.8.0] - 2026-09-01

### Added — session-store model, wave 1: stores as data, fleet census, decode confidence

Three problems traced to one root cause (`docs/design/session-store-model.md`):
the reconcile layer targeted fields that don't exist on real installs, the
session stores that *do* work were hardcoded Rust rather than data, and the
census was single-host. The census and fleet paths this release adds are
read-only — no session data is mutated by `sessions`, `--host`, or
`--all-hosts` anywhere in this wave. But the wave is not read-only end to
end: `claude_code` gains a `[tool.home_dir_layout]` below, making
`sessionguard migrate claude_code` a real, opt-in mutating operation against
a store the tool may be actively writing to, with no quiesce hook declared
for it — non-deleting and undoable, but capable of leaving split-brain
history until Claude Code restarts. Store re-keying on a project move
(rewriting a session's *contents* to follow it) is still Wave 2.

- **`[tool.session_store]` schema.** A new block on `ToolDefinition` declares
  where a tool's sessions live and how they're keyed to a project, as one of
  three layout kinds implemented in Rust but bound entirely by TOML data:
  `encoded_dir` (one directory per project, path separators collapsed —
  Claude Code), `jsonl_field` (project path is a field, optionally dotted,
  on a JSONL file's first line — Codex), and `sqlite_column` (project path is
  a column in a SQLite table, opened read-only — OpenCode). A tool with a
  known storage shape is now a config file away from being censused, not a
  recompile.
- **`sessions.rs` is declaration-driven.** It no longer hardcodes three
  reader paths; it dispatches to the matching reader for whatever
  `session_store` bindings the loaded tool registry declares.
- **Verified store bindings** shipped for `claude_code`
  (`~/.claude/projects`), `codex` (`~/.codex/sessions`), and `opencode`
  (`opencode.db`).
- **Honesty patch.** `claude_code`'s and `gemini_cli`'s previously declared
  `path_fields` named fields (`.claude/settings.json` → `project_path`,
  `.gemini/settings.json` → `project_root`) that do not exist on real
  installs — reconciliation for both was a silent no-op while `tools list`
  and detection kept reporting them as supported. Both declarations are
  removed. `claude_code` also gained a `home_dir_layout`, so its `projects/`
  store (and only that directory — never the parent `~/.claude`, which holds
  live runtime state and credentials) is migratable via `sessionguard
  migrate`. The README's support table no longer claims unqualified
  "Reconcile" for any built-in tool; see the table for the honest per-tool
  breakdown (Cursor/Windsurf/Aider's reconcile targets are plausible but
  unverified against real installs).
- **Three-state decode confidence** (`exact` / `inferred` / `unresolved`)
  replaces a boolean for how confidently a Claude Code store entry resolves
  to a real project path. This is what makes a **deleted** Claude Code
  project detectable as an orphan for the first time — the decoder validates
  candidate splits against the live filesystem, so previously a project that
  no longer existed could never decode and was invisible to `sessions
  --orphans` instead of flagged by it.
- **`sessionguard sessions --home <path>`** censuses an arbitrary root
  instead of `$HOME` — e.g. a mounted or rsync'd home directory from another
  machine.
- **`sessionguard sessions --host <name>` / `--all-hosts`** with a new
  `[[hosts]]` config block censuses one or every configured machine over
  ssh, merging each host's JSON with provenance (a new `fleet.rs`). Read-only
  by construction — the only remote commands ever run are `--version` and
  `sessions --format json` — and **orphan status always comes from the
  origin host**, never re-derived against the local filesystem (a remote
  path that doesn't exist locally must not be reported as orphaned just
  because this machine can't see it). A remote running 0.7.0 (pre-`confidence`)
  is supported via a compatibility shim that synthesizes `confidence` from
  its `decoded` boolean; a remote older than 0.7.0 is refused with a named
  error pointing at `sessionguard update`. An ssh destination beginning with
  `-` is refused before any process is spawned, closing an argv-injection
  path into a locally-executed command.

### Changed — JSON shape

`sessions --format json` (and the fleet/`--home` variants) now includes a
`confidence` field (`"exact" | "inferred" | "unresolved"`) on every group.
The old `decoded` boolean is **retained for one release** for compatibility
with the dashboard and any scripts already consuming it, and will be removed
in a future release — consumers should migrate to `confidence`.

## [0.7.0] - 2026-07-16

### Added — per-project session census (`sessionguard sessions`)

A new discovery axis: where `inventory` totals each tool's home-dir store,
`sessions` answers *"for each project, which assistants have sessions, how
many, and how fresh?"* — including **orphaned** groups whose project directory
no longer exists (the first control signal for cleaning up stale session
data).

- **`sessionguard sessions [--orphans] [--tool <name>] [--project <path>]
  [--format json]`** — groups sessions by project directory across the three
  home-dir stores: Claude Code (`~/.claude/projects/<encoded>/`, with
  filesystem-validated DFS decoding of the hyphen-ambiguous dir names), Codex
  (`~/.codex/sessions/**/*.jsonl` via the first-line `cwd`), and OpenCode
  (`opencode.db`, read-only SQLite). Undecodable Claude store names are shown
  as `[ENCODED NAME]` rather than hidden; orphan detection is exact for
  Codex/OpenCode (literal paths) and conservative for encoded names.
- **The dashboard's Activity tab now consumes `sessions --format json`** —
  the binary is the single source of truth for store discovery (the tab's
  Python walkers remain only as a fallback for older/missing binaries), and
  orphaned projects get a red `orphaned` pill in the UI.

## [0.6.3] - 2026-07-16

### Fixed — wiring & scaffolding audit

A root-to-leaf wiring audit (every module, command, script, config file, and
cross-file contract) found the code fully wired but several scaffolding
artifacts miswired or stale; all fixed.

- **`install.sh` no longer 404s on ARM Linux.** It advertised
  `aarch64-unknown-linux-gnu`, a target no release builds — it now refuses
  with a `cargo install` hint (matching the updater's behavior). The final
  version check also verifies the binary it just installed instead of
  whatever `PATH` resolves, and the quick-start reflects `init` + background
  `start`.
- **`contrib/sessionguard.service` could never do its job.**
  `ProtectHome=read-only` blocked the reconciler's writes into project
  directories (the daemon's core function), and `ExecStart` pointed at
  `~/.local/bin` while `install.sh` defaults to `/usr/local/bin`. Hardening
  relaxed to `ProtectSystem=full` (with rationale) and the path corrected.
- **`start --daemon` is truthful again** — it was silently ignored; it now
  explicitly means background (the default) and conflicts with
  `--foreground`.
- **`release.toml`** would have produced a release commit cliff's changelog
  doesn't skip (`chore:` vs `chore(release):`) under an invalid `[workspace]`
  header; both corrected.
- **New CI gate: `scripts/check-consistency.sh`.** README/ROADMAP/SECURITY/
  CHANGELOG version stamps and install-target wiring are now verified on
  every push — the "docs sat out N releases" drift (which had recurred three
  times, most recently leaving README at v0.5.0 while 0.6.2 shipped) is now a
  CI failure, not an audit finding.
- Docs re-synced to reality: README status/Shipped/downgrade-guard note,
  ROADMAP current marker + shipped checkboxes (backgrounding, dogfood CI,
  advisories-via-cargo-deny), SECURITY 0.6.x, CLAUDE.md test count + scripts,
  the skill's per-file Verify wording, the dashboard's "until v0.4 lands"
  string, handoff.md's export premise (v2 full-graph) + re-baseline, and the
  future-dated 0.6.2 changelog entry (tagged 2026-07-14).

## [0.6.2] - 2026-07-14

### Fixed / Changed — deep-hardening (audit Wave 3)

The remaining MED/LOW findings from the codebase audit
(`docs/design/hardening-audit.md`), in three batches.

**Reconciler correctness**
- The text adapter now replaces only at path boundaries — an incidental
  `/home/me/code-backup` in an aider chat-history body is no longer corrupted
  by a `/home/me/code` rewrite (M1).
- The event log records the (old, new) pair the adapter ACTUALLY applied, so
  `undo` works for projects under macOS `/var`, `/tmp`, `/private` paths
  instead of silently no-oping (M9).
- Artifacts over 32 MB and non-UTF-8 files are skipped with a warning instead
  of erroring, and one bad artifact no longer aborts a tool's remaining
  fields (M18).

**Daemon / CLI robustness**
- PID-file acquisition is atomic (`O_EXCL`) — two racing daemons can't both
  start, and a losing racer can't delete the winner's PID file (M7).
- HOME-unset environments fall back to a stable absolute per-uid dir instead
  of a cwd-relative `./.sessionguard` (M6).
- `update`'s download workdir is unpredictable and 0700 (M13); install-method
  detection is anchored to real brew prefixes (M15); after an update, a
  still-running pid-file daemon is called out as stale (M14).
- inotify-watch exhaustion now names the `fs.inotify.max_user_watches` fix
  (M16). The event log prunes already-undone events beyond the newest 5,000 —
  pending undo entries are never pruned (M17).
- `stop` no longer claims success when the signal fails (L1); `unwatch`
  reports when nothing matched (L2); `--verbose` beats an ambient `RUST_LOG`
  (L3); `config edit` validates the TOML immediately (L4); failed
  config-backup restores during migrate rollback are surfaced (L8).

**Data integrity**
- `export` writes a versioned bundle including each project's artifact
  mappings; `import` restores the full graph (and still reads the old
  paths-only format). Backups no longer silently drop the core data (M2).
- Migrate's Verify stage compares src/dst **per file** (path + size;
  symlinks by target, remap-aware) instead of totals — a dropped file plus a
  same-size growth elsewhere can no longer cancel out; mismatches are named
  (M3).

## [0.6.1] - 2026-07-12

### Added — CLI quality-of-life

- **`sessionguard init`** — first-run onboarding. Recursively scans your home
  directory for AI-tool projects and writes the directories that contain them to
  `watch_roots`, so the daemon monitors where your projects actually live instead
  of only the conventional `~/{projects,repos,code,dev}`. `--dry-run` previews;
  `--depth` bounds the search. (Closes the empty-config footgun, audit M5.)
- **`sessionguard logs [--lines N] [--follow]`** — tail the background daemon's
  log (`<data-dir>/daemon.log`, new in v0.6.0).
- **`sessionguard scan` is now recursive** (`--depth`, default 4) instead of a
  single level, so nested/monorepo project layouts are discovered; it prunes at
  each detected project and skips VCS/dependency/build dirs. `scan` and `watch`
  now also SIGHUP a running daemon so newly-registered projects are picked up
  live. (Audit M4.)

### Fixed

- A `--config <path>` that doesn't exist yet no longer hard-fails at startup —
  it falls back to defaults (with a note), so `init` can create it on first run.

## [0.6.0] - 2026-07-12

### Fixed / Changed — correctness hardening (audit Wave 2, HIGH items)

The remaining HIGH-severity findings from the codebase audit
(`docs/design/hardening-audit.md`). Several are behavior changes.

- **`start` now actually backgrounds** (H7). `sessionguard start` re-execs a
  detached daemon (new session, logs to `<data-dir>/daemon.log`) and returns
  immediately, instead of printing "not implemented" and running in the
  foreground. `--foreground` keeps the old behavior. Double-start is refused.
- **`watch` is no longer home-locked** (H4). The daemon now watches the
  configured `watch_roots` **plus the parent of every registered project**, so a
  project tracked via `watch` that lives outside a configured root is actually
  monitored. `watch`/`unwatch` send the running daemon `SIGHUP` to reload its
  watch set live — no restart needed.
- **SQLite concurrency** (H1). Every registry/event-log connection sets
  `journal_mode=WAL` + `busy_timeout=5000`, so the daemon writing while the CLI
  reads no longer fails instantly with "database is locked".
- **`stop` can't signal the wrong process** (H3). Liveness checks now verify the
  PID belongs to a sessionguard process (not one that recycled the PID after a
  crash + reboot) before signaling or reporting a running daemon.
- **No orphaned, un-undoable rewrites** (H2). A failed undo-log write is now a
  hard error surfaced to the operator, not a swallowed `warn!`.
- **Migrate resumes on abort** (H5). Any Copy/Verify/Rewrite failure after
  Quiesce now restarts the stopped unit before returning — no silent outage.
- **Tighter rename pairing** (H6). Cookieless (macOS) rename halves pair only
  within a 100 ms window and only when the source path is actually gone,
  preventing two unrelated renames from being fused into a bogus move.

## [0.5.2] - 2026-07-12

### Fixed — data-safety + update-security (audit Wave 1, 3 BLOCKERs)

A recursive whole-codebase footgun audit surfaced three blocking issues; all
fixed here. Full register + remaining waves in `docs/design/hardening-audit.md`.

- **Atomic artifact rewrites.** The reconciler's path rewrites used an in-place
  `fs::write` (truncate-then-write); a crash, power loss, or full disk mid-write
  could leave a user's session file truncated and unrecoverable. Rewrites now go
  through a temp sibling → `fsync` → atomic `rename` (mode preserved) — either the
  old or the new file survives intact, never a torn one.
- **Rollback-safe self-update swap.** `update` removed the working binary before
  the new one was in place; a mid-swap failure (disk full, sudo expiry) could
  leave no `sessionguard` on `PATH`. The swap now stages the new binary, copies
  the current one aside, and atomically renames — `dest` is never absent, and the
  root path runs the whole sequence in one `sudo sh -c`.
- **Closed the update code-execution seam.** `SESSIONGUARD_UPDATE_BASE_URL`
  (which points the self-replacing binary at an arbitrary release) is now honored
  only behind an explicit, hidden `--allow-custom-base` flag used by the offline
  dogfood/tests; production always uses the pinned GitHub release URL.
- **Downgrade + tag guards.** `update --to` validates the tag shape and refuses
  installing an older release than the one running unless `--allow-downgrade` is
  passed.

## [0.5.1] - 2026-07-12

### Testing — close the coverage gaps a deep audit surfaced

A two-track analysis (coverage census + project-state review) drove a
pre-Handoff hardening pass. No user-facing behavior change.

- **The reconcile pipeline dispatcher is now tested.** `daemon.rs`'s
  `handle_session_event` (detector → reconciler → registry → event log) had
  *zero* direct tests — the product's core seam, previously exercised only
  by `dogfood.sh`. Added 4 unit tests: moved-reconciles-and-reregisters plus
  the no-artifacts / partial-move / removed-path arms.
- **`update` install-method branches covered.** `update-dogfood.sh` now
  proves the dev-build (source checkout) *refusal* and the cargo/homebrew
  *deferral* — the guards that stop `update` clobbering a package-managed or
  dev binary.
- Added: `doctor` launcher-health section renders (sandbox); symlink-discovery
  migrate→undo through the real CLI; `tools list --format json` carries
  `binary_status`; `inventory` records a note on a permission-denied dir
  without panicking.
- Fixed the stale `dogfood.sh` Linux message (the daemon cookie-pairs rename
  half-events now; a failure there is a real regression, not an expected gap).

### Changed

- **Split `migrate.rs` (3,798 lines) into a `migrate/` module** —
  `migrate/mod.rs` (production state machine, 1,968 lines) + `migrate/tests.rs`
  (test suite). Pure move, zero behavior change; the module was 40% of the
  codebase and the primary structural debt.
- **CI: non-gating coverage baseline** via `cargo-llvm-cov` (line coverage was
  previously unmeasured). Advisory scanning was already gated by `cargo-deny`.

## [0.5.0] - 2026-06-26

### Features — fleet self-update

`sessionguard update` keeps a machine current with one command — built
after the fedora hub was found four minor versions behind without anyone
noticing.

- **`sessionguard update [--check] [--dry-run] [--to <ver>]`.** Detects how
  the binary was installed and **defers to the package manager** rather than
  fighting it: self-replaces only a standalone (`install.sh`) install,
  prints the right `brew upgrade` / `cargo install --force` for those, and
  refuses a dev build. `--check` is a read-only fleet-drift probe (exits
  non-zero when behind).
- **Integrity-gated, reversible.** The downloaded asset is verified against
  the release `SHA256SUMS` and **refused on mismatch** before anything is
  swapped; the previous binary is kept at `<bin>.bak-<ver>`; a running
  systemd `--user` daemon is restarted. Uses `sudo` only when the install
  dir isn't writable.

### Changed — release integrity (prerequisite)

- Releases now publish a **`SHA256SUMS`** asset alongside the tarballs, and
  `install.sh` verifies the download against it (refuse on mismatch; warn
  and proceed only for older releases that predate the asset). Closes a
  no-integrity-check gap in the curl-pipe installer.

### Testing

- `scripts/update-dogfood.sh` exercises the full self-update path offline
  via a `file://` fake release — swap, `.bak` rollback, and checksum-tamper
  refusal — on both OSes in CI. 133 lib + 28 integration tests passing.

## [0.4.3] - 2026-06-25

A repo-health pass: the engine was already solid, but the docs had drifted
~2 minor versions and the headline feature lacked black-box coverage. This
release makes the docs tell the truth, covers migrate end-to-end, closes a
latent symlink data-loss edge, and corrects a false MSRV claim.

### Fixed

- **Copy now preserves symlinks.** The migrate Copy stage previously skipped
  symlinks silently — a symlinked directory or dangling link in a source tree
  was dropped, and Verify couldn't always catch it. Copy now recreates symlinks
  faithfully; an absolute target pointing into the source root is rebased onto
  the destination so it resolves post-migrate. Relative and external targets are
  recreated verbatim.
- **MSRV corrected to 1.85.** The declared `rust-version = "1.75"` was already
  false — a transitive dependency (`toml_datetime` via `toml`) requires the
  `edition2024` Cargo feature (Rust 1.85+), so 1.75 could not build. Updated
  `Cargo.toml`, `clippy.toml`, the README badge, and CLAUDE.md, and added a CI
  job that pins 1.85 so the claim stays honest.

### Added — test & CI coverage

- Black-box end-to-end tests for `migrate` / `undo --migration` / `migrate-cleanup`
  driving the real binary against a throwaway config-discovery tool, plus a new
  `scripts/migrate-dogfood.sh` wired into CI on both OSes.
- `SESSIONGUARD_CONFIG_DIR` env override (mirroring `SESSIONGUARD_DATA_DIR`) so
  CLI smoke tests never read the operator's real `~/.config` / `$HOME`.
- Unit tests for symlink recreation: relative, absolute-into-source (remapped),
  symlinked-directory, and dangling — including a Verify-clean assertion.

### Changed

- **Docs realigned with v0.4.2 reality.** README status v0.3.2 → current with a
  real "Migrate" usage section; removed the never-built `relocate` command from
  all live docs; ROADMAP marks v0.4 + launcher-health shipped and adds a
  cross-machine-handoff entry; CLAUDE.md module map / test count refreshed;
  SECURITY supported-versions updated; `docs/design/migrate.md` retired to
  `docs/history/` with inline SHIPPED NOTEs.
- Release pipeline: the Homebrew-tap job is now non-fatal (`continue-on-error`)
  and runs after `publish`, so a missing `HOMEBREW_TAP_TOKEN` no longer marks the
  release run failed.

## [0.4.2] - 2026-05-28

### Features — quiesce hooks on the OpenCode and Codex builtins

`sessionguard migrate` can now stop a tool before copying its data and
restart it afterward, baked into the built-in tool definitions instead
of relying on the operator to wire it up.

- **OpenCode and Codex builtins declare their systemd user units**
  (`opencode.service`, `codex.service`). For OpenCode this means migrate
  quiesces the daemon before copying its live WAL SQLite database,
  avoiding a torn copy; for Codex it gives the `CODEX_HOME` env-rewrite a
  unit to drop the override into.
- **A declared-but-not-loaded unit is benign.** Because quiesce units are
  environment-specific (you may run these tools interactively, not under
  systemd), a not-loaded unit no longer aborts the migration — migrate
  classifies it as a new `UnitAbsent` outcome, warns, and proceeds
  without quiescing. Real failures (permission denied, no DBus) still
  abort. Operators can override the unit name via user/project tool
  config.

## [0.4.1] - 2026-05-28

### Features — reclaim space from completed migrations

`sessionguard migrate` never auto-deletes the original: it preserves it
at a `.migrated-<unix>` sidecar (plus any config backups) so a migration
stays reversible. This release adds the operator-driven companion to that
rule — a way to reclaim that space once you're confident the move stuck.

- **`sessionguard migrate-cleanup`.** Reports the reclaimable preserved
  originals (sidecars + consumed config backups) with per-item sizes by
  default; pass `--execute` to delete them. `--migration <id>` scopes the
  cleanup to one migration; without it, all cleanable migrations are
  considered. The live migrated data at the destination is never touched.
- **Cleaning makes a migration un-undoable, by design.** Once cleaned,
  `undo --migration <id>` refuses with a clear message, bare `undo` skips
  the migration, and `log` labels it `(cleaned — not undoable)`.

## [0.4.0] - 2026-05-28

### Features — migrations are reversible

The last deferred piece of v0.4 lands: `sessionguard migrate` is now
fully undoable, completing the migrate feature end-to-end (real
migrations across all three discovery branches + a one-command undo).

- **Migration log.** Every successful real migration is recorded to a
  new `migrations` table in the event log, alongside a self-contained
  JSON undo plan. The table is intentionally decoupled from the migrate
  engine — it stores the plan as an opaque blob so the log has no
  dependency on the state machine's types.
- **`sessionguard undo` reverses a migration.** Inverts the forward
  pipeline in dependency order: quiesce → reverse rewrite (remove
  symlink / restore config backups / uninstall systemd drop-in) →
  restore the source from its `.migrated-<unix>` sidecar → remove the
  orphaned copy at the destination → resume. With no flags, `undo`
  reverses the most recent pending migration if one exists, otherwise
  falls back to reconcile-event undo. `--migration <id>` targets a
  specific migration; `--id` still targets reconcile events.
  `--dry-run` prints every step without touching the system. Because
  the source is never deleted, even a failed undo leaves recoverable
  data.
- **`sessionguard log` lists migrations** with their ids and undone
  state, so operators can find the id to pass to `undo --migration`.

### Changed

- `migrate` now prints `run \`sessionguard undo\` to reverse it` after a
  successful real migration. The stale dry-run notice claiming real
  migration is "gated until stages 5-7" is gone — it shipped two
  releases ago.

### Testing

- New round-trip tests prove a symlink-discovery and a config-discovery
  migration each undo cleanly: the source is restored byte-for-byte and
  the destination copy is removed. Plus dry-run-undo-is-a-no-op,
  `undo_plan()` is `None` for dry-runs, and migration-log CRUD +
  idempotent mark-undone. **117 tests passing total.**
- Verified end-to-end through the real CLI on macOS: live migrate →
  `log` → `undo --dry-run` → `undo` (source + nested files restored,
  dst removed, record marked undone) → re-undo is a clean no-op.

## [0.3.13] - 2026-05-28

### Fixed

- **Dry-run rewrite detail is now discovery-aware.** Previously every
  `migrate --dry-run` invocation reported "would install symlink for
  X discovery" regardless of the actual branch — a small but real lie
  caught during fedora dogfood smoke. Now each branch reports what
  *would* actually happen:
  - **Symlink**: `would install symlink <src> -> <dst> and preserve original`
  - **Config**: `would rewrite N config file(s) [<file> (field …, json), …]`
  - **Env**: `would install systemd drop-in for --user <unit> setting <VAR>=<dst>`
  - **Env without unit**: surfaces `<no unit declared — real run would refuse>`
    so operators see the preflight refusal before they hit it.

### Dogfood

- v0.3.12 was used to migrate devo's real **21.2 GB / 144,385-file
  OpenCode store** from `/home/devo/.local/share/opencode` to
  `/mnt/fastpool/devo/opencode` on fedora — across filesystems, in
  **94 seconds** (~225 MB/s). All 9 stages walked cleanly; verify
  matched byte-for-byte; symlink installed and OpenCode 1.1.53 reads
  through it transparently; original preserved at
  `opencode.migrated-<unix>` per the never-auto-delete rule.
  v0.4's state machine is production-real.

### Testing

- 4 new unit tests verifying each discovery's dry-run detail string,
  including the env-without-unit warning. **110 tests passing total**.

## [0.3.12] - 2026-05-26

### Features — `migrate` runs end-to-end now

v0.4 step 7: Resume + Validate + Retain wired; `NotYetMutating` gate
removed. Real (non-dry-run) `sessionguard migrate` now completes the
full state machine for every supported discovery branch:

```
Preflight → Snapshot → Quiesce → Copy → Verify → Rewrite →
            Resume → Validate → Retain → Done
```

- **Stage::Resume** — calls `Quiescer::resume` to restart whatever
  Quiesce stopped. Failure unwinds Rewrite and surfaces the error.
- **Stage::Validate** — runs `layout.validate.command` with a timeout
  (default 10s, configurable via `validate.timeout_seconds`). Exit 0
  → success. Anything else (non-zero, timeout, spawn failure) →
  full rollback: re-quiesce the unit, undo the rewrite, remove dst.
- **Stage::Retain** — preserves the source per the "never auto-delete"
  design rule:
  - Symlink discovery → no-op (Rewrite already moved src aside).
  - Config / Env discovery → renames src to `<src>.migrated-<unix>`.
  Operators decide when to clean up the sidecar.
- **Stage::Done** — terminal event so dashboards / scripts can stop
  polling.

### Breaking

- **`MigrateError::NotYetMutating` is gone.** Real runs no longer
  refuse — they complete. If you were matching on this variant in
  external code, replace it with success-path assertions.

### Testing

- 7 new unit tests covering: Validate exit-zero success, non-zero
  failure, timeout-and-kill, no-command-declared skip, full-migration
  rollback on validate failure, Retain renaming src for Config/Env,
  Retain no-op for Symlink. End-to-end success tests for all three
  discovery branches now assert the migration COMPLETES (not unwinds).
  **106 tests passing total**.

### Internal

- Driver header docstring rewritten to reflect the now-complete state
  machine (was previously a "what lands in v0.3.6 vs. later" table).
- Event-log integration for migrations is intentionally deferred to
  v0.3.13 (step 7b) — keep this ship focused on the state machine
  itself.

## [0.3.11] - 2026-05-26

### Features

- **Rewrite stage wired for `discovery = "env"`** — v0.4 migrate
  step 6b-env, the third and final discovery branch. Tools that
  read their data-dir location from an environment variable now
  get a systemd drop-in installed at:
  - `~/.config/systemd/user/<unit>.d/sessionguard-migrate.conf` (user scope)
  - `/etc/systemd/system/<unit>.d/sessionguard-migrate.conf` (system scope)
  Containing `[Service]\nEnvironment=<VAR>=<new_data_dir>` and
  followed by `systemctl daemon-reload` in the right scope.
- **Loud refusal when no systemd unit declared** — `discovery = "env"`
  requires `quiesce.systemd_user_unit` or `quiesce.systemd_system_unit`.
  Without one there is no safe automatic place to set the env var,
  and we won't silently edit operator dotfiles. The refusal message
  tells the operator exactly what to declare or to set the var
  manually in their shell rc.
- **`EnvWriter` trait** — pluggable backend mirroring the existing
  `Quiescer` pattern. Production uses `SystemdEnvWriter`; tests use
  `FakeEnvWriter` to verify drop-in contents without touching real
  systemd. Add the new `migrate_with_backends(...)` entry point for
  tests that need both backends fake; `migrate_with(...)` keeps
  working with the real env writer for existing callers.
- **`RewriteOutcome::EnvOverridden { record }`** — carries the
  `EnvOverrideRecord` (scope, unit, drop-in path, env var, value)
  so `undo_rewrite` can remove the drop-in and `daemon-reload`
  again. Round-trip undo proven by tests.

### Discovery branch coverage

All three real discovery branches are now live:
- ✅ `Symlink` — v0.3.9 (step 6)
- ✅ `Config` — v0.3.10 (step 6b-config)
- ✅ `Env` — this release (step 6b-env)

`NotYetMutating` still gates real runs at Stage::Resume — stages
6 (Resume) + 7 (Validate) + 8 (Retain) land in v0.4 step 7.

### Testing

- 6 new unit tests covering: drop-in contents + path; user-over-system
  scope preference; system-only fallback; missing-env-var refusal;
  round-trip undo removes drop-in; end-to-end real migrate with
  `discovery = "env"` reaching the gate and unwinding cleanly.
  **100 tests passing total**.

## [0.3.10] - 2026-05-26

### Features

- **Rewrite stage wired for `discovery = "config"`** — v0.4 migrate
  step 6b-config. Tools that store their data-dir location in a JSON
  or TOML config file (rather than via env var or symlink) can now
  reach Stage::Rewrite on a real run. The driver reuses the
  reconciler's adapter dispatch (`pub(crate) reconciler::rewrite_field`)
  so the same JSON/TOML/text adapters that rewrite in-project
  artifacts also rewrite home-dir config references.
- **Per-file timestamped backups** — each config file is backed up
  to `<name>.sessionguard-backup-<unix>` before rewriting. On any
  per-file failure, every earlier backup is rolled back so the
  operator never sees a half-rewritten config. The backup pairs ride
  on `RewriteOutcome::ConfigEdited { backups }` so `undo_rewrite`
  can restore them later when stages 6–8 land.
- **Loud refusal for misconfigured layouts** — `discovery = "config"`
  with empty `config_files`, a referenced config that doesn't exist,
  or a target field that doesn't carry the source path all fail
  with `MigrateError::StageFailed(Stage::Rewrite, …)` and a precise
  message naming the offending path/field. No silent no-op rewrites.

### Internal

- `reconciler::rewrite_field` is now `pub(crate)` so the migrate
  driver can dispatch directly into the same adapter chain used by
  in-project reconciliation. The function continues to live in
  reconciler.rs — migrate.rs only consumes it.
- `NotYetMutating` gate still sits before Stage::Resume; with Config
  now landing, two of the three discovery branches (Symlink, Config)
  are live. Env is the last holdout, scheduled for the next ship.

### Testing

- 6 new unit tests covering: clean rewrite + backup; missing-field
  refusal; multi-file rollback when a later file fails; round-trip
  undo restoring the backup; end-to-end real migrate with
  `discovery = "config"` reaching the gate and unwinding cleanly;
  empty-`config_files` refusal. **94 tests passing total**.

## [0.3.9] - 2026-05-26

### Features

- **Rewrite stage wired for `discovery = "symlink"`** — v0.4 migrate
  step 6 (partial). After Copy + Verify succeed, the original source
  directory is renamed aside to `<src>.migrated-<unix_seconds>` and a
  symlink is installed at the canonical path pointing to the new
  destination. The tool keeps reading the canonical path; data lives
  at the new location.
- **Rollback that survives Rewrite** — on later-stage failure, the
  Rewrite is undone (symlink removed, preserved sidecar renamed back)
  before any error returns. Source filesystem state is byte-identical
  to pre-migration after a rolled-back attempt.

### Internal

- New `RewriteOutcome` enum: `SymlinkInstalled { canonical, target,
  moved_aside }` / `DryRunSkipped` / `Deferred { reason }`. Tagged
  JSON repr; carried in the event log for `undo` consumption.
- New `rewrite_via_symlink(canonical, target)` — performs the
  rename-aside + symlink-install dance. Refuses when the timestamped
  preserved name already exists (rare but possible if two migrations
  collide in the same second).
- New `undo_rewrite(outcome)` — reverses a SymlinkInstalled outcome.
  Used by the gate-after-Rewrite block; will also be used by
  `sessionguard undo` for stale half-migrates once event-log
  integration lands in step 7.
- The `NotYetMutating` gate moved from BEFORE Stage::Rewrite to AFTER.
  Rewrite now runs on real migrations; the gate sits before Resume
  (which doesn't exist yet) so we still can't complete a migration.
  On gate trip, the just-installed symlink is undone and the dst is
  cleaned up before the error returns.
- `discovery = "config"` and `discovery = "env"` return a typed
  `StageFailed(Rewrite, ...)` error explaining they're slated for
  step 6b; rollback removes the dst.
- Four new unit tests (Unix only, since symlinks): symlink-install
  swaps + preserves, refusal on preserved-name collision, undo
  restores the original, Env-discovery refusal rolls back cleanly.
  114 tests total (was 110).

### Still gated

Resume + Validate + Retain (step 7). The `Env` and `Config`
discovery branches (step 6b). After step 7, real migrations
complete end-to-end and the gate finally comes down.

## [0.3.8] - 2026-05-26

### Features

- **Copy + Verify stages wired** — v0.4 migrate steps 3-4. The Copy
  stage now actually walks the source tree and writes the files to
  the destination; the Verify stage walks both sides and compares
  `{file_count, total_bytes}`. Symlinks are deliberately skipped
  (cycles + off-tree pointers); Unix mode bits are mirrored so
  executables stay executable.
- **Automatic rollback** — when migrate aborts after the Copy stage
  (e.g. because the still-gated Rewrite stage refuses), the partial
  destination is removed before the error returns. Source is left
  untouched. The operator never sees orphan data.

### Internal

- New `copy_tree(src, dst) -> CopySummary` recursive copier with
  symlink skip + Unix mode mirroring. No external deps; uses
  `std::fs::copy` per file.
- New `verify_copy(src, dst) -> VerifyOutcome` — best-effort walk
  of both sides returning mismatched fields when they disagree.
- New `cleanup_partial_copy(dst)` — best-effort rollback. Idempotent
  (no-op when dst doesn't exist).
- The `NotYetMutating` gate moved from BEFORE Stage::Copy to BEFORE
  Stage::Rewrite. Copy + Verify are read-only on source and write
  fully-cleanupable bytes to dst, so they ship as real operations
  now; Rewrite / Resume / Validate / Retain remain gated until
  later steps.
- Nine new unit tests cover copy_tree (basic, refuses-existing-dst,
  skips-symlinks, mirrors-executable-bit), verify_copy (match,
  mismatch-on-removal), cleanup (idempotent), and end-to-end driver
  behavior (real run copies, hits Rewrite gate, rolls back cleanly,
  src untouched). 110 tests total (was 101).

### Still gated

Rewrite / Resume / Validate / Retain — landing in steps 6-7. After
Verify succeeds, the migrate driver removes the dst it just created
and returns `NotYetMutating`. The operator can `migrate --dry-run`
to walk the read-only half, but cannot complete a real migration
until those stages ship.

## [0.3.7] - 2026-05-26

### Features

- **Quiesce stage wired to real systemd** — v0.4 implementation
  step 4 (see `docs/design/migrate.md`). Migration now actually
  stops the service holding the data before the copy stage; future
  Resume stage will start it back up post-rewrite.

### Internal

- New `Quiescer` trait abstracts "stop / start the thing holding
  the data" so unit tests can verify the dispatch logic without
  spawning real `systemctl` processes.
- `SystemdQuiescer` is the production implementation: shells out
  to `systemctl --user stop <unit>` (preferred, no sudo needed)
  or `systemctl stop <unit>` based on the layout's quiesce hook.
- `QuiesceOutcome` enum (`UnitStopped { scope, unit }` /
  `NoUnitWarning` / `DryRunSkipped`) records what actually
  happened in the per-stage event.
- `ResumeAction` enum (`StartUnit { scope, unit }` / `None`)
  records the inverse of Quiesce. Carried in `MigrationResult`
  so the upcoming Resume stage (and `sessionguard undo` for stale
  half-migrates) knows what to bring back up.
- New `migrate_with(tool, src, dst, dry_run, &dyn Quiescer)`
  entry point for tests; `migrate()` stays the production API.
- Six new unit tests cover the wiring: dry-run records DryRunSkipped,
  user-unit picked up correctly, user-vs-system preference order,
  system-only fallback, no-unit warning path, ResumeAction JSON
  tagged-repr shape. 101 tests total (was 95).

### Still gated

Real (non-dry-run) migrations remain blocked by `NotYetMutating`
until Copy + Verify + Rewrite + Resume + Validate land
(steps 5-7 of the implementation order). Dry-run end-to-end works
including the Quiesce simulation.

## [0.3.6] - 2026-05-26

### Features

- **`sessionguard migrate <tool> --to <path> --dry-run`** — the
  read-only half of v0.4 migrate. New `Command::Migrate` wired to
  the state-machine skeleton in `src/migrate.rs`. Walks every
  implemented stage (preflight → snapshot → quiesce → copy → verify)
  and emits the per-stage event log without touching the filesystem.
- **Real (non-dry-run) migration is intentionally gated** — returns
  `MigrateError::NotYetMutating` until stages 5–7 (rewrite / resume /
  validate) land. Refusal text is actionable and points the operator
  at `--dry-run`. This is enforced in the library, not just the CLI,
  so future callers can't accidentally route around it.

### Internal

- New `src/migrate.rs` module:
  - `Stage` enum matching the design doc's eight-stage diagram
  - `MigrationEvent` (per-stage record; flat shape for event-log
    JSON storage when step 6 lands)
  - `MigrationResult` (terminal state + full event trail)
  - `MigrateError` typed variants for every refusal path
  - `migrate(tool, src, dst, dry_run) -> Result` driver
  - Iterative file walker for verify-stage size/count
- `inventory::expand_home()` exposed `pub` so migrate can reuse the
  same `~`-expansion semantics. Both the inventory CLI and the new
  migrate CLI consume tool definitions' `default_path` identically.
- 7 new unit tests cover every refusal path (NoLayout, CompileBaked,
  SourceMissing, DestinationExists, NotYetMutating) and dry-run
  happy paths (full stage sequence + quiesce-intent recording).
  95 tests total (was 88).

### Live data this release surfaces

On the operator's fedora hub, `sessionguard inventory` reports:

```
codex     /home/devo/.codex                    198.4 MB   3571 files    2d ago
opencode  /home/devo/.local/share/opencode      19.8 GB 144385 files  108d ago
```

The OpenCode store is the v0.4 migrate test target named in
`docs/design/migrate.md`. v0.3.6 is the last "read-only" step
before the mutating stages land.

## [0.3.5] - 2026-05-26

### Features

- **`sessionguard inventory`** — pure read-only command that walks every
  registered tool with a `home_dir_layout` declaration and reports
  `{tool, location, size, last_activity}`. The lead-in to v0.4
  `migrate`: answers *"what should I move and how big is it?"*
  - Text mode renders a compact table with human-friendly size + age
    formatting.
  - `--format json` for tooling integration.
  - Walks capped at 200k files per store; result includes a
    `truncated` flag when the cap was hit.
  - Symlinks are skipped (don't follow).
- **`home_dir_layout` schema on `ToolDefinition`** — declarative
  description of where each tool stores user-scoped data and how
  `sessionguard migrate` (v0.4, in flight) should rewrite its
  self-references. Optional; tools without it behave exactly as
  before. Full schema in `docs/design/migrate.md`.
- **Codex** and **OpenCode** builtins populated with home_dir_layout:
  - Codex: `discovery = "env"`, `env_var = "CODEX_HOME"`.
  - OpenCode: `discovery = "symlink"` (default XDG path; no env var
    or config file declares the data dir).

### Internal

- New `src/inventory.rs` module with `inventory_tools_impl()` plus
  9 unit tests (5 inventory module + 3 home_dir_layout schema + 1
  per-builtin assertion). 88 tests total (was 79).
- New `src/main.rs` helpers `fmt_size` and `fmt_ago` for the
  inventory text table, both with unit tests.

### Roadmap

- v0.3.4 docs/design/migrate.md captured the v0.4 contract. v0.3.5
  delivers schema + inventory (steps 1–2 of the implementation
  order). Next: state-machine skeleton (step 3).

## [0.3.4] - 2026-05-26

### Features

- **`sessionguard doctor --clean`** — unregister tracked projects whose
  directory no longer exists on disk. Pure report mode remains the
  default; cleanup is opt-in. Add `--dry-run` to preview without
  writing. Cascades through SQLite's `ON DELETE CASCADE` to drop any
  associated `session_artifacts` rows in one shot.
  - Operator's own Mac registry had ~33 stale entries from sandbox
    test fixture leftovers accumulated over months. One command
    cleared the lot.

### Tests

- Two new sandbox tests cover the new flag:
  - `sandbox_doctor_clean_dry_run_does_not_mutate` — verifies the
    registry survives a `--clean --dry-run` invocation
  - `sandbox_doctor_clean_removes_stale_entries` — registers two
    projects, deletes one, runs `--clean`, asserts the stale entry is
    gone and the live one survives

### Docs

- New `docs/ops/homebrew-tap-token.md` walking through the one-time
  `HOMEBREW_TAP_TOKEN` fine-grained PAT setup that the release
  workflow's `homebrew` job needs. The job has been failing loud (by
  design) on every release since v0.3.2 until the secret is created.
  Cross-referenced from the v0.3.2 changelog entry and the README
  roadmap "Shipped" section.

## [0.3.3] - 2026-04-18

### Features

- **Launcher health checks** — the *visibility* path of the "runtime
  upgrade lost my launcher" problem. When you upgrade Node, Python, or
  any runtime that hosts AI tooling, the global package installs under
  the previous version vanish from PATH; your session data is intact
  but `claude` / `codex` / etc. become "command not found." Sessions
  appear gone — they aren't.
  - New optional `binary` field on `ToolDefinition` declares the
    launcher binary expected on PATH.
  - All 7 built-in patterns populated: `claude_code → claude`,
    `cursor → cursor`, `windsurf → windsurf`, `aider → aider`,
    `gemini_cli → gemini`, `codex → codex`, `opencode → opencode`.
  - New `src/health.rs` module with `check_binary()` that resolves
    against PATH via a built-in `which(1)`-equivalent walker (no
    subprocess, works on minimal Linux images).
  - `BinaryStatus` enum: `Present { path }`, `Missing { binary }`,
    `NotConfigured`. Tagged JSON repr for dashboard consumption.

### CLI

- `sessionguard doctor` now reports a `launcher health` section
  alongside the existing tracked-project check. Missing launchers
  are flagged with a `[WARN]` line that explicitly notes
  *"session data intact; check installer / runtime version"* so
  users don't think their history is lost.
- `sessionguard tools list` gains a `LAUNCHER` column in the text
  output and a `binary_status` field in the `--format json` output.

### Dashboard

- **Tools tab** — per-tool block now shows a launcher status pill
  (`launcher OK` / `launcher missing` / `no launcher configured`)
  with the resolved path or actionable diagnostic.
- **Activity tab** — column headers for stores whose launcher binary
  is missing get a ⚠ badge, so at a glance you can see "this column
  has 14 sessions but the tool can't run."

### Roadmap

- Path B from the v0.3.x launcher-health roadmap entry (active
  *availability* — actually restoring launchers across runtime
  changes via `sessionguard restore-launcher`) remains deferred.
  Path A (visibility, this release) ships first to let real-world
  data inform whether visibility alone is enough.

## [0.3.2] - 2026-04-18

### Features

- **`--format json`** on `tools list`, `log`, and `status`. Emits the
  same structured data the library already serialises internally. Text
  output remains the default; JSON is opt-in via flag. The dashboard
  now consumes this instead of parsing the human-readable text output,
  eliminating a class of fragility (CLI text changes breaking the UI).
- **CI dogfood job** — `scripts/dogfood.sh` now runs in GitHub Actions
  on both `ubuntu-latest` and `macos-latest` after the Check matrix
  completes. Regression gate for the class of bugs that historically
  slipped past unit tests (rename pairing, macOS path aliasing).
- **Homebrew tap auto-update** — a new `release-homebrew.yml` workflow
  fires on `release: published`, downloads the asset tarballs, computes
  SHA256s, renders a fresh `Formula/sessionguard.rb`, and pushes to
  `PilotDevo/homebrew-tap`. Requires repository secret
  `HOMEBREW_TAP_TOKEN` (fine-grained PAT, `Contents: write` on the tap).
  Fails fast with a clear message if the secret isn't configured. See
  [`docs/ops/homebrew-tap-token.md`](docs/ops/homebrew-tap-token.md) for
  the one-time PAT creation walkthrough.

### Changes

- `log` text output now tags undone events with `(undone)` at end of line.
- Dashboard: `list_tools()` consumes `--format json`; stale text-parsing
  fallback removed.

### Tests

- 3 new CLI smoke tests verify that `--format json` produces valid JSON
  for `tools list`, `log`, and `status` (67 tests total).

## [0.3.1] - 2026-04-17

### Features

- **Two new built-in tool patterns**: Codex and OpenCode. Both declared as
  `on_move = "notify"` for now — their session data lives under `~/.codex`
  and `~/.local/share/opencode` respectively, keyed on absolute project
  paths. Home-dir reconciliation is v0.4 `migrate` scope; until then these
  patterns surface the tools' presence (via `AGENTS.md` + per-project
  markers) without touching the home-dir stores. Total built-in count is
  now **7**.
- **Dashboard: Sessions tab** — enumerates home-dir session stores for
  Claude Code, Codex, OpenCode, Cursor, and Gemini CLI. Shows presence,
  item count, aggregate size, and last-modified time. Walks are capped
  at 200k files per store and cached for 30 seconds so polling doesn't
  re-scan multi-GB trees.

### Notes

- Dashboard smoke test on the author's Mac reveals 13 GB of Codex
  rollouts, 1.6 GB of Claude Code projects, and 6 OpenCode sessions —
  exactly the kind of data the v0.4 `migrate` feature will target.

## [0.3.0] - 2026-04-17

### Features

- **`sessionguard undo`** — reverse previously-logged reconciliation actions.
  Routes to the same adapter used during reconciliation with `old_value` /
  `new_value` swapped. Supports `--last N` (default 1), `--id <N>` for a
  specific event, and `--dry-run` for preview-only. Undone events are marked
  via `undone_at` so they're excluded from future `undo` invocations.
- **`sessionguard tools [list] [--verbose]`** — inspect registered tool
  patterns (built-in + user config + project config). `--verbose` shows
  session patterns and path_fields per tool.
- **3 new built-in tool patterns**: Windsurf, Aider, Gemini CLI. Built-in
  count is now 5 (plus any user or project patterns).

### Changes

- **Event log schema**: `format` column (adapter hint for undo) and
  `undone_at` timestamp column added. Fresh DBs get the full schema; pre-v0.3
  DBs are migrated in-place via idempotent `ALTER TABLE ADD COLUMN`.
- **`ReconcileAction`** now carries the `format` field so undo can route to
  the right adapter without needing the tool definition.
- **`ROADMAP.md`** added, capturing v0.3 → v1.0 arc and the v0.4 "migrate"
  thesis shift.

### Internal

- `reconciler::rewrite_field` exposed as `pub(crate)` to support undo reuse
- Schema migration fixed: index creation on new columns now happens AFTER
  `ALTER TABLE` (previously both ran in one batch and the index failed,
  aborting the migration)

## [0.2.3] - 2026-04-17

### Bug Fixes (Critical)

- *(watcher)* Rename pairing buffer — `notify` emits renames as two half-events (`From`/`To` on Linux with cookies, back-to-back `Any` events on macOS FSEvents with no cookies). The watcher now buffers half-events and pairs them into proper `Moved` events by cookie match or FIFO-within-TTL fallback. Before this fix, end-to-end reconciliation never fired on macOS or Linux despite the v0.2.2 claims; dogfooding revealed the gap.
- *(reconciler)* macOS `/private` path aliasing — `notify` reports canonical paths (`/private/var/...`, `/private/tmp/...`), but user tooling stores the short form (`/var/...`, `/tmp/...`). Reconciliation now tries both forms and rewrites with the matching pair's form, so stored paths keep the style the user sees.

### Test

- Added `scripts/dogfood.sh` — end-to-end smoke test that runs the real daemon and verifies reconciliation against a synthetic Claude Code project. Safe to run anywhere; uses isolated config and data dirs.
- Added `examples/notify_dump.rs` — diagnostic tool that prints every raw `notify` event for a watched directory. Used to reverse-engineer macOS FSEvents behaviour.

## [0.2.2] - 2026-04-16

### Bug Fixes

- *(reconciler)* Prefix-safe path replacement — paths like `/foo/code-backup/x` are no longer corrupted when `old_root` is `/foo/code` (#19)
- *(watcher,daemon)* Explicit `RenameMode` classification — Linux inotify renames (separate From/To events) are no longer silently dropped (#19)

### Robustness

- `try_send` in the notify callback — the sync watcher thread can no longer deadlock on a full channel (#19)
- Atomic PID file write with tempfile + rename; refuses to clobber a live daemon (#19)
- RAII `PidGuard` removes the PID file on any exit path, including early errors (#19)
- `shutdown_signal` no longer panics on signal-registration failure (#19)
- `Stop` verifies the daemon is alive before sending SIGTERM; cleans up stale PID files (#19)

### Refactor

- `Scan` canonicalizes paths to match `Watch` (macOS `/var` → `/private/var`) (#19)
- `register_project` is now a single atomic `INSERT ... ON CONFLICT ... RETURNING` (#19)
- `EventLog` orders by `id DESC` instead of `timestamp` (SQLite `datetime('now')` is 1s resolution) (#19)

### Build

- *(deps)* Bump libc from 0.2.183 to 0.2.184 (#15)
- *(deps)* Bump toml from 1.1.0+spec-1.1.0 to 1.1.2+spec-1.1.0 (#16)
- *(deps)* Bump tokio from 1.50.0 to 1.51.1 (#17)
- *(deps)* Bump clap_complete from 4.6.0 to 4.6.1 (#18)

### Miscellaneous

- Add rust-toolchain.toml, PR template, crates.io badge (#13)

## [0.2.1] - 2026-03-30

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


