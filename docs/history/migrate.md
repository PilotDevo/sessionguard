# Design: `sessionguard migrate` (v0.4) — RETIRED

> **Status**: SHIPPED in v0.4.0–v0.4.2 and retired to `docs/history/`. This is
> the original design contract, kept for the record. Where the shipped code
> diverged from this draft, the divergences are flagged inline as **SHIPPED
> NOTE**. For current behavior see the `src/migrate.rs` module docs, the README
> "Migrate" section, and `CHANGELOG.md`.
> Originally drafted 2026-05-26 (v0.3.4 baseline).

## Thesis shift

v0.1–v0.3 pitch: *"keeps AI coding sessions intact when your
projects move."* Narrow, passive, reactive.

v0.4 pitch: ***"the tool that moves AI dev environments between
disks without breaking them."*** Bigger market — every developer who
installs Ollama + Claude + Codex + Cursor + OpenCode eventually wants
hot data on a fast pool and runs out of room on `/home`. Today they
hand-edit configs, run `rsync`, cross fingers. Tomorrow they run
`sessionguard migrate`.

The reconciler, registry, event log, tool-pattern catalog, and undo
infrastructure built v0.1–v0.3 are all reusable. v0.4 adds the
*data-move* layer alongside the existing *path-rewrite* layer.

## Concrete first target

The fedora dogfood box has **20 GB of OpenCode session data** under
`/home/devo/.local/share/opencode/` on its root disk. We built a
1.9 TB btrfs RAID-0 fastpool at `/mnt/fastpool/`. The natural test
case for v0.4:

```bash
sessionguard migrate opencode --to /mnt/fastpool/opencode
```

Goal: at the end, OpenCode's daemon opens its DB from the fastpool
location, session history is intact, the old root-disk path is empty,
and the move is reversible via `sessionguard undo`. **Zero data loss
under any failure mode.**

If we can survive that test, we can survive any tool-with-home-dir-
state migration.

---

## Commands

### `sessionguard migrate <tool> --to <path>`

Tool-aware relocation of *that tool's session data* (and any
project-side references to it).

```bash
sessionguard migrate opencode --to /mnt/fastpool/opencode
sessionguard migrate codex    --to /mnt/fastpool/codex \
                              --keep-old  # preserve source until manual cleanup
sessionguard migrate claude_code --to /mnt/fastpool/claude --dry-run
```

> **SHIPPED NOTE:** `--keep-old` was never built — the source is *always*
> preserved (renamed to `.migrated-<unix>`); reclaiming it is the separate
> `sessionguard migrate-cleanup` command. The shipped flags are `--to`,
> `--dry-run`, `--format`.

For each tool definition, a new optional `home_dir_layout` field
describes where the tool stores user-scoped data and how to rewrite
its self-references. Without this field, the tool is "in-project
only" (existing v0.3 behaviour); with it, `migrate` knows how to
operate on the home-dir layer.

### `sessionguard relocate <src> <dst>`

> **SHIPPED NOTE:** `relocate` was **not** built. The tool-centric `migrate`
> covered the real need and the path-centric variant was dropped. This section
> is design-only and describes a command that does not exist.

Path-aware move. Scans **all** registered tool definitions for
references to `<src>`, moves the data, rewrites every reference.
Like `mv` but session-aware.

```bash
sessionguard relocate ~/projects/old-name ~/projects/new-name
sessionguard relocate ~/.codex /mnt/fastpool/codex
```

Difference from `migrate`: `migrate` is *tool-centric* ("move
opencode wherever it lives, to here"); `relocate` is *path-centric*
("move this directory, fix everything pointing at it"). `migrate`
is implemented on top of `relocate` plus tool-definition knowledge.

### `sessionguard inventory`

Enumerate every tracked tool, its data locations, sizes,
last-activity timestamps. The "what should I move?" lead-in.

```
TOOL          LOCATION                                  SIZE      LAST ACTIVITY
claude_code   ~/.claude/projects/                       1.6 GB    2m ago
codex         ~/.codex/sessions/                        13 GB     3h ago
opencode      ~/.local/share/opencode/opencode.db       20 GB     1d ago
aider         ~/.aider/                                 12 MB     14d ago
```

Effectively the Activity tab's data, as a CLI report. Shares the
underlying inventory module so both surfaces stay in sync.

---

## The migrate state machine

Every `migrate` invocation is a small, well-defined state machine.
Each transition is logged to the event log so `undo` can reverse it.

```
                ┌─────────────┐
                │ 0. preflight│
                └──────┬──────┘
                       │
                ┌──────▼──────┐
                │ 1. snapshot │  (btrfs-only; skip on ext4/xfs)
                └──────┬──────┘
                       │
                ┌──────▼──────┐
                │ 2. quiesce  │  stop daemons/services that hold the data
                └──────┬──────┘
                       │
                ┌──────▼──────┐
                │ 3. copy     │  rsync -aHAXS new path
                └──────┬──────┘
       ┌─ SHIPPED NOTE: implemented as a custom recursive `std::fs`
       │  copy (no rsync dependency), not `rsync -aHAXS`.
       │  (As of v0.4.3 it recreates symlinks; earlier 0.4.x skipped them.)
                       │
                ┌──────▼──────┐
                │ 4. verify   │  sizes match, no missing files, no errors
                └──────┬──────┘
                       │
                ┌──────▼──────┐
                │ 5. rewrite  │  update tool configs / symlinks
                └──────┬──────┘
                       │
                ┌──────▼──────┐
                │ 6. resume   │  restart any quiesced services
                └──────┬──────┘
                       │
                ┌──────▼──────┐
                │ 7. validate │  service responsive, opens new data
                └──────┬──────┘
                       │
                ┌──────▼──────┐
                │ 8. retain   │  source kept as .migrated-YYYYMMDD-HHMMSS
                └─────────────┘  (manual cleanup; never auto-delete)
```

### Failure handling per stage

- **Stages 0–4** (read-only on source): rollback is free. Abort, no
  damage.
- **Stages 5–6** (writes to config / restart): rollback restores
  config from backup, restarts service against original source.
- **Stage 7** (validation): if the new location fails health check,
  rollback to source (which is intact — we never deleted it).
- **Stage 8** is *retention*, not deletion. The source directory is
  renamed to `.migrated-<timestamp>` and left in place. Operator
  reclaims space later with an explicit `sessionguard migrate-cleanup`
  (or just `rm -rf`) once they've confirmed everything works.

We **never** auto-delete the source. The lesson from the fastpool
docker migration was clear: keep `/var/lib/docker.preserved-*`
around for hours/days; let the operator nuke it manually when they
trust the new location.

### Rollback equivalent of `undo`

`sessionguard undo` for a `migrate` event:
- Reads the event log entry (`{src, dst, snapshot_id?, config_backups}`)
- Stops services that came up against `dst`
- Restores config from `config_backups`
- Moves `dst` back to `src` (or vice versa if `src` was already
  renamed)
- Restarts services
- Marks event undone

If a btrfs snapshot was taken in stage 1, `undo` prefers the
snapshot path (atomic, instant) over the rsync-reverse path
(time-proportional to data size).

---

## Per-tool `home_dir_layout` schema

Extension to `ToolDefinition`:

```toml
[tool]
name = "opencode"
display_name = "OpenCode"
binary = "opencode"
# ...existing fields unchanged...

[tool.home_dir_layout]
# The canonical location the tool reads/writes its session data from.
default_path = "~/.local/share/opencode"

# How the tool finds its data dir at startup. Either:
#   - "env"      : reads an env var named `env_var`
#   - "config"   : path embedded in a config file (declare in `config_files`)
#   - "symlink"  : just symlink default_path → new location and let the
#                  tool follow the symlink (simplest, but breaks if the
#                  tool resolves symlinks; tested per-tool)
#   - "compile"  : path baked into the binary; cannot migrate without
#                  re-installing. SessionGuard flags this as unsupported.
discovery = "config"

# For `discovery = "config"`, list each file that names the data dir
# and where in it the path lives. Same shape as `path_fields` for
# in-project artifacts — reuses the JSON/TOML/text adapters.
[[tool.home_dir_layout.config_files]]
file = "~/.config/opencode/config.json"
field = "data_dir"
format = "json"

# Optional. If set, `sessionguard migrate` will `systemctl --user stop`
# this unit before the copy and `start` it again after rewrite. If the
# tool isn't running as a unit, leave unset and the operator quiesces
# manually (or migrate is run during a known-idle window).
quiesce.systemd_user_unit = "opencode.service"

# Optional. Validation step 7: after rewrite, run this command and
# expect zero exit. SessionGuard times it out at 10s.
validate.command = ["opencode", "--health"]
```

Tools without a `home_dir_layout` block are skipped by `migrate`
with a clear "no home-dir layout declared — pattern needs an
update" message. This lets us land `migrate` with 1–2 fully-fleshed
tools (OpenCode is the first target) and grow coverage organically.

---

## What I'm explicitly NOT building in v0.4

- **No runtime installer integration.** That's v0.3.x launcher-health
  Path B (the `restore-launcher` reach goal), separately scoped and
  separately controversial. `migrate` is about moving *data*; the
  binary itself stays where it is.
- **No multi-machine migration.** v0.4 is local-only. Cross-machine
  is its own can of worms (rsync over ssh, mtime preservation,
  fsync semantics over network filesystems, …). Phase 2 of the
  original migrate roadmap.
- **No "migrate everything."** Each migrate invocation is one tool
  at a time. A future `sessionguard migrate-all --to /mnt/fastpool`
  meta-command could orchestrate them, but only after we trust the
  per-tool path.
- **No Windows.** notify v8 covers it, but the systemd quiesce step
  and the standard XDG paths don't translate. v0.4 is Unix-only,
  same as the rest of the project at this stage.

---

## Open questions to resolve before code

1. **rsync vs. `mv` vs. btrfs reflink.** On same-filesystem moves,
   `mv` is atomic and free. On btrfs same-subvol, reflink is free
   too. Cross-filesystem we need rsync. The state machine above
   assumes rsync; should it sniff and choose?
2. **What about tools that don't have a quiesce story?** OpenCode
   has a systemd unit; Codex CLI runs ephemerally per shell.
   For ephemeral tools we'd want to acquire an advisory lock on
   the data dir (similar to baton's design) before migrating, but
   that requires the tool itself to honor the lock — most don't.
   Decision: ephemeral tools migrate without quiesce; we warn
   loudly that the operator should not start a new session of
   the tool mid-migrate.
3. **Symlinks vs. config rewrites.** Some tools (notably OpenCode)
   read a config file at startup; some have hardcoded paths and
   we can only paper over with symlinks. Both are first-class
   `discovery` values above. Symlink-based migrations are
   reversible by re-symlinking; config-based migrations are
   reversible by restoring the backup. Both supported.
4. **CI dogfood for migrate.** The existing `dogfood.sh` covers
   the in-project reconcile path. Need a new `migrate-dogfood.sh`
   that exercises the migrate state machine against a fake-tool
   fixture. Probably 100–150 LOC bash.

---

## Implementation order (rough)

A focused dedicated session should fit roughly:

1. **Schema + parser** — extend `ToolDefinition`, parse
   `home_dir_layout`, validate, expose via existing JSON output.
2. **`sessionguard inventory`** — pure-read command, lets us
   sanity-check the schema against real machines before any
   writes land.
3. **State machine skeleton** — implement stages 0–4 (preflight
   through verify) as a `Migration` struct with explicit
   transitions, event log writes per transition. Initially no
   actual writes; just dry-run.
4. **Quiesce + resume** — wire up systemd-user stop/start, file-
   lock acquisition for ephemeral tools.
5. **Config rewrite** — reuse the existing adapter dispatch from
   `reconciler.rs` (JSON, TOML, text fallback). Symlink-based
   discovery doesn't need a rewrite step.
6. **Rollback / undo integration** — wire migrate events into the
   existing event log + undo command. `sessionguard undo` learns
   to reverse a `migrate` event.
7. **btrfs snapshot integration** — opportunistic, behind a
   feature detect. Atomic rollback on supported filesystems.
8. **OpenCode home_dir_layout TOML** — first concrete tool, the
   fedora dogfood target.
9. **migrate-dogfood.sh** — CI gate; lives in `scripts/`.
10. **Codex home_dir_layout TOML** — second tool; flushes out the
    ephemeral-tool gap (no systemd unit).
11. **Documentation pass** — README's `migrate` section, ROADMAP
    update marking v0.4 shipped, this design doc retired into
    `docs/history/`.

Reviewable as ~5–6 PRs (or commits, given the solo-dev push-direct
flow): schema, inventory, state machine, rewrite, undo, OpenCode
plus dogfood. Realistic window: a focused weekend.

---

## Acceptance criteria

v0.4 ships when:

- [ ] `sessionguard migrate opencode --to /mnt/fastpool/opencode`
      moves OpenCode's 20 GB on fedora, completes with the daemon
      coming back up healthy against the new path.
- [ ] `sessionguard undo` reverses the OpenCode migrate (snapshot
      or rsync-back) and OpenCode opens against the original path.
- [ ] `sessionguard inventory` shows accurate sizes and locations
      across all 7 builtin tools.
- [ ] `migrate-dogfood.sh` passes in CI on Ubuntu + macOS against
      a fake-tool fixture.
- [ ] All migrate operations write event-log entries that
      round-trip through `sessionguard log` and `sessionguard undo`.
- [ ] `--dry-run` works on every command and produces no FS or
      registry changes.
- [ ] README has a "Migrate" section with a real example.
- [ ] ROADMAP marks v0.4 done and points the v0.5 arrow at Tool
      Pattern Library.

---

## Why this design is worth committing to before code

Three failure modes I'm specifically guarding against:

1. **Premature implementation lockoff** — building the state
   machine bottom-up risks committing to a quiesce/resume model
   before we know how the data-rewrite layer wants to consume it.
   The schema-first approach in section "Per-tool `home_dir_layout`"
   gets the public interface right before any internal code.
2. **Auto-delete regret** — every previous migration in this
   project (docker data-root, etc.) we kept the source around
   under `.preserved-*` until manual cleanup. This design enshrines
   that as a non-negotiable. We never delete.
3. **No-undo regret** — `sessionguard undo` is the trust feature
   that took v0.3.0 to ship. If `migrate` events aren't undoable
   the same way reconcile events are, v0.4 will reintroduce the
   exact distrust we just eliminated. The state machine and event
   log integration above guarantee they are.

This doc is the contract. Code that contradicts it should bounce
back to this doc, not the other way around.
