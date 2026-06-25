---
name: sessionguard-migrate
description: >-
  Drive SessionGuard to relocate an AI coding tool's home-directory data
  (OpenCode, Codex, etc.) to a new disk or path safely, and to reverse or
  reclaim those migrations. Use this whenever the user wants to move, relocate,
  or "get off the root disk" a tool's session/data directory (e.g. "my ~/.codex
  is 40GB, move it to /mnt/fastpool", "relocate opencode's data to the NAS"),
  free up space from a past migration, undo a migration, or check what tool data
  is taking up space. Trigger even when the user names the tool or a path but not
  "SessionGuard" — if the request is about moving/reversing an AI tool's data
  directory, this skill owns the workflow and its safety rules.
---

# SessionGuard migrate

SessionGuard relocates an AI coding tool's home-directory data (its session DB,
config, history) to a new location and repoints the tool at it — without the
tool noticing. The whole point of the tool is that this is **safe and
reversible**: the original is never deleted, every real migration is recorded,
and `undo` reverses it.

Your job with this skill is to run that migration workflow with the discipline
the tool was designed around. The CLI will happily do a real migration in one
command; the value you add is *not* skipping the preview, confirming the
destination, and knowing how to back out.

## The non-negotiables

These exist because a migration moves a user's working data between disks. Get
them wrong and you either lose time or scare the user.

1. **Always dry-run before a real migration.** `migrate --dry-run` walks all ten
   stages and mutates nothing. Show the user that plan and the sizes involved
   before you run the real thing. Never go straight to a real `migrate`.
2. **The original is preserved, not deleted.** A successful real migration
   leaves the source at `<src>.migrated-<unix>` and any edited config at
   `<name>.sessionguard-backup-<unix>`. Tell the user this — it's why `undo`
   works. Reclaiming that space is a *separate, explicit* step (`migrate-cleanup`).
3. **`undo` is the escape hatch.** Any completed migration can be reversed with
   `sessionguard undo` (most recent) or `undo --migration <id>` (specific) until
   it has been cleaned up. If a real migration looks wrong, reach for undo.
4. **Cleanup is irreversible — confirm it explicitly.** `migrate-cleanup
   --execute` deletes the preserved original, which makes that migration
   *un-undoable*. Only run `--execute` when the user has clearly said to reclaim
   the space. The report form (no `--execute`) is always safe.
5. **Never print secrets.** Tool data dirs contain credential files (e.g.
   `auth.json`, API keys). When showing command output or inspecting a data dir,
   never cat or echo credential file contents. Inventory/migrate output is
   path/size metadata and is safe.

## Invoking the CLI

Prefer an installed `sessionguard` on `PATH`. Inside this repo during
development, `cargo run --release -- <args>` works. All examples below use
`sessionguard <args>`.

Most commands accept `--format json` (`inventory`, `status`, `log`, `tools`) —
use it when you want to parse results rather than eyeball a table.

## The workflow

### 1. Inventory — what's there and how big

Start here, always. It's read-only and tells you which tools declare a
migratable home-dir layout, where the data lives, its size, and how stale it is.

```
sessionguard inventory
```

```
TOOL           LOCATION                            SIZE      FILES  LAST MODIFIED
codex          /Users/devo/.codex                 2.0 GB     43438        31s ago
opencode       /Users/devo/.local/share/opencode  13.0 MB       108        12d ago
```

Use this to confirm the tool the user means, its current path (the migration
*source*), and its size (so you can sanity-check the destination has room).

### 2. Plan — pin down the destination

The destination is a flag, not positional:

```
sessionguard migrate <tool> --to <destination> [--dry-run]
```

Before running anything, confirm with the user:
- **The destination must not already exist.** SessionGuard refuses to overwrite
  an existing path. Pick a fresh path on the target disk.
- **It's on the disk they intend** (the whole point is usually "off the root
  disk onto the big/fast one"). Cross-check the inventory size against free
  space if you can.

### 3. Dry-run — preview every stage

```
sessionguard migrate codex --to /mnt/fastpool/codex --dry-run
```

This walks the full state machine and changes nothing. Read it back to the user
— especially the Quiesce, Copy, Rewrite, and Retain lines. A healthy dry-run
ends with `final stage: Done  success: true` and `(dry-run only — no changes
were made.)`. See "Reading the stages" below for what each line means.

If the dry-run reports a failure or refuses (e.g. destination exists, env
discovery without a unit), fix that *before* a real run — don't paper over it.

### 4. Execute — the real migration

Only after the dry-run looks right and the user has confirmed:

```
sessionguard migrate codex --to /mnt/fastpool/codex
```

On success, tell the user plainly: the data now lives at the destination, the
tool is repointed (via symlink, config edit, or env override depending on the
tool), **the original is preserved** at the `.migrated-<unix>` sidecar, and the
migration can be reversed with `sessionguard undo`.

### 5. Verify

Confirm the tool reads from the new location. The cheapest check is to re-run
`sessionguard inventory` (the tool's location should now resolve to the new
data) and, for tools with a `validate` command declared, the migration already
ran it as the Validate stage. If the user can, have them launch the tool once.

### 6. Reverse or reclaim

- **Something's off →** `sessionguard undo` (see "Undo" below). This restores the
  original and removes the copy.
- **Confident it stuck and want the space back →** `migrate-cleanup` (see
  "Reclaiming space" below).

## Reading the stages

A migration runs ten stages. In dry-run each prints what it *would* do; in a real
run, what it *did*. The ones worth reading back to the user:

- **Preflight** — validates source exists, destination doesn't, discovery type.
- **Snapshot** — currently stubbed (no btrfs detection yet); safe to ignore.
- **Quiesce** — stops the tool's systemd unit so its data isn't written
  mid-copy. If the unit isn't loaded on this host (the common interactive case),
  it says so and proceeds — that's *benign*, not an error. "No quiesce hook
  declared" is also fine.
- **Copy** — rsync source → destination (shows file/byte counts).
- **Verify** — compares size/count between source and destination.
- **Rewrite** — repoints the tool: installs a symlink (OpenCode), edits a config
  file, or drops a systemd `Environment=` override (Codex / `CODEX_HOME`).
- **Resume** — restarts anything Quiesce stopped.
- **Validate** — runs the tool's declared health check, if any.
- **Retain** — renames the source aside to `.migrated-<unix>` (the preserved
  original).
- **Done** — `success: true` means the migration completed.

## Undo

Reverses a completed migration in dependency order (quiesce → reverse the
rewrite → restore the source from its sidecar → remove the copy → resume).
Because the source was never deleted, even a partial undo leaves recoverable
data.

```
sessionguard undo                      # reverse the most recent pending migration
sessionguard undo --migration <id>     # reverse a specific one (id from `log`)
sessionguard undo --dry-run            # show the steps, change nothing
```

Find migration ids with `sessionguard log` — it lists migrations with their ids
and marks ones that are `(undone)` or `(cleaned — not undoable)`. A migration
that's been cleaned up can't be undone; `undo` will refuse it and say so.

With no flags, `undo` reverses the most recent pending migration if there is one,
otherwise it falls back to undoing the last reconciliation action (the daemon's
project-move bookkeeping — a different feature). If the user means a migration,
prefer being explicit with `--migration <id>`.

## Reclaiming space

The preserved originals are deliberately left behind so migrations stay
reversible. When the user is confident a migration stuck and wants that space
back:

```
sessionguard migrate-cleanup                       # REPORT what's reclaimable (safe)
sessionguard migrate-cleanup --migration <id>      # scope the report to one migration
sessionguard migrate-cleanup --execute             # DELETE the preserved originals
```

Always run the report form first and show the user the reclaimable sizes. Only
add `--execute` on an explicit go-ahead. Remind them: cleaning a migration makes
it un-undoable (the live migrated data at the destination is untouched — only the
preserved original is removed).

## When things go wrong

- **"destination exists / refuses to overwrite"** — pick a fresh path; don't
  delete the existing one without understanding what it is.
- **Codex (env discovery) errors about a missing unit** — the `CODEX_HOME`
  override needs a systemd unit to write into. The builtin declares
  `codex.service`; if the user runs Codex from a plain shell instead, the honest
  fix is to export `CODEX_HOME=<dest>` in their shell rc and re-run with
  `--dry-run` to walk the rest.
- **Quiesce says the unit isn't loaded** — benign. It means the tool isn't
  running under systemd here; the migration proceeds. Just make sure the tool
  isn't actively writing during the copy.
- **A real migration failed partway** — the source is preserved; run `undo
  --dry-run` to see what reversing would do, then `undo` to clean up the
  half-migration.

## Other commands (not this workflow)

`start`/`stop`/`watch`/`unwatch`/`scan` run and feed the background daemon that
reconciles project *moves* — a separate concern from home-dir migration. `doctor`
diagnoses stale registry entries. Mention these only if the user asks; this
skill is about `migrate` / `undo` / `migrate-cleanup`.
