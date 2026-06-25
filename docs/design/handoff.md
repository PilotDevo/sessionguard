# Design: `sessionguard handoff` — cross-machine session portability (v0.5?)

> **Status**: Design draft. No code yet. Land this doc first, take a
> beat, then implement — same cadence as `migrate.md` preceded v0.4.
> Reviewers: feedback as GitHub issues. Last revised: 2026-06-25
> (v0.4.2 baseline).

## Thesis: the third axis

SessionGuard has solved two kinds of "move" so far, both **on one
machine**:

| Axis | What moves | Shipped |
| --- | --- | --- |
| Project path changes | `~/proj/old` → `~/proj/new` | v0.3 reconcile |
| Tool data relocates | `~/.local/share/opencode` → `/mnt/fastpool` | v0.4 `migrate` |
| **Session resumes elsewhere** | **machine A → machine B** | **this doc** |

`migrate.md` explicitly punted on this: *"No multi-machine migration.
… Cross-machine is its own can of worms … Phase 2 of the original
migrate roadmap."* This is that phase 2, scoped on its own terms.

The pitch: **"pick up the exact AI coding session you left on one box,
on another box."** You're mid-conversation with Claude Code / Codex /
OpenCode on the MacBook; you walk to `DOLOAMD`; you keep going — same
history, same context — instead of starting cold.

None of the tools we run do this. `claude --resume`, OpenCode's session
DB, and Codex's `~/.codex/sessions/` are all **local-only**: resume
reads *this machine's* files. The cloud web apps have server-side
sessions, but that doesn't carry a CLI session across a fleet. The gap
is real.

### Why this is SessionGuard's problem and not a sync tool's

`rsync`/Syncthing/git move *bytes*. They don't know that a session
artifact **embeds absolute paths and a per-tool key** that are wrong on
the destination machine. That re-keying + path-remapping is exactly what
`reconciler.rs` + the `ToolRegistry` already do for same-machine moves —
cross-machine just widens the rewrite to span home dirs, usernames, and
OS path conventions. SessionGuard owns the *correctness* layer; the
fleet's existing fabric (10G NAS, the `fedora` hub, git) owns transport.

### Relationship to `droco-code-mem`

The hub already ingests session files cross-machine for **recall** —
making past sessions queryable. Handoff is **live resumption**, a
different thing: rehydrate a *resumable* session into the target tool's
own storage so the tool opens it natively. We are not rebuilding recall;
we are filling the "and now actually continue it" gap recall doesn't
cover. Where the hub already holds the session files, `handoff` can pull
its source bundle from there instead of from machine A directly.

## What `export` / `import` do today (the starting point)

Current `export` writes the registry's project list (paths only) as
JSON; `import` re-registers those paths. **Bookkeeping, not content** —
no artifacts, no keys, no rewriting. Handoff is the content-bearing,
re-keying superset. The existing commands stay as-is for registry
backup; handoff is a new, focused pair (see Commands).

## Concrete first target

A real cross-machine pair on the fleet: **MacBook (`/Users/devo`,
macOS) → `DOLOAMD` (`/home/devo`, Linux)**, tool **Claude Code**, this
very repo.

```bash
# On the MacBook, mid-session:
sessionguard handoff pack claude_code \
  --project ~/Droco/side-projects/ai-session-track \
  -o ait.sgbundle

# Carry ait.sgbundle over the 10G NAS (or the fedora hub).

# On DOLOAMD:
sessionguard handoff apply ait.sgbundle \
  --to /home/devo/Droco/side-projects/ai-session-track
# → then `claude --resume` opens the same session, history intact.
```

Claude Code is the right first tool: its sessions are newline-delimited
JSON (`.jsonl`) under a path-derived directory name — tractable with the
adapters we already have, no new storage engine. **Goal: zero context
loss, the original machine's session untouched, the apply reversible via
`sessionguard undo`.** Survive this and the model generalizes.

---

## Commands

### `sessionguard handoff pack <tool> --project <path> -o <bundle>`

Capture *one tool's session artifacts for one project* into a portable,
self-describing `.sgbundle`. Read-only on the source. Records source
machine conventions in a manifest so `apply` knows what to rewrite.

```bash
sessionguard handoff pack claude_code --project ~/work/api -o api.sgbundle
sessionguard handoff pack codex --project ~/work/api -o api-codex.sgbundle
```

Optionally `--from-hub` to pull the source artifacts from the
`droco-code-mem` namespace instead of the local tool dir.

### `sessionguard handoff apply <bundle> --to <project-path>`

Rehydrate a bundle into *this* machine's tool storage: re-key for the
target project path, remap embedded paths to this machine's conventions,
place artifacts where the tool expects them, register the project, and
record an undoable event. Refuses to clobber an existing same-key session
without preserving it first (`--pre-handoff` backup).

```bash
sessionguard handoff apply api.sgbundle --to /home/devo/work/api
sessionguard handoff apply api.sgbundle --to /home/devo/work/api --dry-run
```

### `sessionguard handoff inspect <bundle>`

Read-only: print the manifest — source machine, tool + version, project
path, artifact list, what would be rewritten, and what was excluded
(secrets). The "is this safe to apply here?" lead-in. Mirrors how
`inventory` leads into `migrate`.

---

## The `.sgbundle` format

A single file (tar+zstd) with a manifest plus the captured artifacts:

```
api.sgbundle
├── manifest.json
└── artifacts/
    └── <tool>/<relative-paths-as-on-source>
```

```jsonc
// manifest.json
{
  "sgbundle_version": 1,
  "tool": "claude_code",
  "tool_version": "1.x.y",          // for the skew guard
  "source": {
    "os": "macos",
    "home": "/Users/devo",
    "username": "devo",
    "project_path": "/Users/devo/Droco/side-projects/ai-session-track"
  },
  "session_key": {                   // how the tool keys this session
    "kind": "path_derived_dir",      // see re-key matrix
    "value": "-Users-devo-Droco-side-projects-ai-session-track"
  },
  "artifacts": [
    { "path": "projects/<dir>/<uuid>.jsonl", "bytes": 41234,
      "rewrites": ["project_path", "home"] }
  ],
  "secrets_excluded": ["auth.json"], // never travels in the bundle
  "created_unix": 1782400000
}
```

The manifest is the contract `apply` reads. Everything `apply` rewrites
or refuses is driven by it — no hidden machine-specific assumptions in
code.

---

## Re-key matrix (per tool)

The crux of feasibility. Each tool keys sessions differently; `apply`
must translate the source key to the target.

| Tool | Storage | Key | Re-key on apply | Difficulty |
| --- | --- | --- | --- | --- |
| **Claude Code** | `~/.claude/projects/<sanitized-cwd>/<uuid>.jsonl` | dir name = cwd with `/`→`-` | recompute dir name for target cwd; rewrite embedded cwd/home inside each JSONL line | **low** (per-line JSON via text/JSON adapter) |
| **Codex** | `~/.codex/sessions/…` JSON | `cwd` field | rewrite `cwd` + any embedded abs paths | **low** (JSON adapter) |
| **OpenCode** | `~/.local/share/opencode/…` SQLite (WAL) | absolute project path columns | `UPDATE` path columns in the DB | **high** — needs a new SQLite-aware adapter; **defer to handoff phase 2** |

Land Claude Code first, Codex second (both reuse existing adapters);
OpenCode waits on a DB adapter. A tool with no declared handoff mapping
is skipped with a clear message — same graceful-degradation pattern as
`home_dir_layout`.

---

## The apply state machine

Mirrors `migrate`'s shape so it inherits the same event-log + undo
guarantees:

```
0. preflight   bundle readable, manifest valid, tool known here
1. skew check  target tool version vs manifest.tool_version (warn/refuse)
2. preserve    if a same-key session exists here, rename .pre-handoff-<unix>
3. unpack      extract artifacts to a staging dir
4. re-key      compute target key (path-derived dir / cwd / …)
5. remap       rewrite embedded paths: home, username, project_path, OS sep
6. place       move re-keyed artifacts into the tool's real storage
7. register    register project in the local registry
8. validate    artifacts parse; (optional) tool can list/open the session
9. record      write an undoable handoff event to the event log
```

### Failure handling

- **0–5** operate on staging only — abort is free, target untouched.
- **6–7** are the writes; an event records exactly what was placed.
- **8** failure → roll back step 6 (remove placed artifacts, restore any
  `.pre-handoff` backup). The source machine is **never** involved in a
  rollback — its session was read-only throughout pack and is unaffected.

### `undo` for a handoff

Reads the event, removes the placed artifacts, restores the
`.pre-handoff-<unix>` backup if one was made, unregisters the project if
it wasn't tracked before. Symmetric with migrate undo; same trust model.

---

## Secrets

Tool data dirs hold credentials (`auth.json`, API tokens). **They never
travel in a bundle.** `pack` excludes a per-tool denylist of credential
files and lists them in `manifest.secrets_excluded`; `apply` warns the
operator to authenticate the tool on the target machine. A bundle is
shareable-by-default safe — losing one leaks session history, not keys.
(Even history can be sensitive; `pack --redact` for a future content
scrub is noted under open questions.)

---

## Concurrency: one-directional by design

Handoff is a **deliberate A→B operator action**, not live sync. We do
**not** attempt bidirectional merge — that's a distributed-systems
problem and contradicts the fleet doctrine that **`fedora` is the single
sync writer**. If you edit the same session on two machines you get two
divergent sessions; last `apply` wins and the displaced one is preserved
as `.pre-handoff-<unix>`. A future "claim/lease via the hub" could add
soft mutual exclusion, but v1 stays explicit and dumb-on-purpose.
*Simple and auditable beats clever.*

---

## What I'm explicitly NOT building in handoff v1

- **No OpenCode** until the SQLite adapter exists (phase 2).
- **No live/continuous sync.** One-directional, operator-invoked only.
- **No Windows.** Unix path conventions only, same as `migrate`.
- **No conflict merge.** Last apply wins; displaced session preserved.
- **No transport.** SessionGuard emits/consumes a file; NAS / hub / git
  / scp carry it. We don't build a network protocol.
- **No secret transport.** Credentials are excluded, never shipped.

---

## Open questions to resolve before code

1. **Command naming.** `handoff pack`/`apply`/`inspect` vs. extending
   `export`/`import` with a `--session` mode vs. top-level
   `pack`/`unpack`. Leaning `handoff` (groups cleanly, matches the
   "Migration Assistant" framing), but it adds surface.
2. **How much of a tool's dir is "the session"?** Claude Code keeps
   todos, shell snapshots, and per-project files beyond the `.jsonl`.
   Pack the whole project dir, or just the transcript + minimum to
   resume? Start minimal, widen if resume is lossy.
3. **JSONL path rewriting fidelity.** Claude Code transcripts embed cwd
   in many record types. Do we rewrite every occurrence, or only the
   keys the tool reads at resume? Over-rewriting risks corrupting
   historical content; under-rewriting risks a broken resume. Needs a
   real capture to characterize.
4. **Version skew policy.** Warn-and-proceed vs. refuse on
   `tool_version` mismatch. Probably warn for patch/minor, refuse on
   major, with a `--force`.
5. **`pack --redact`.** A future content scrub (strip secrets/PII from
   *inside* the transcript, not just exclude credential files) — out of
   scope for v1 but the manifest should leave room.
6. **Dogfood fixture.** A `handoff-dogfood.sh` that packs a fake-tool
   session under one home/OS layout and applies it under another
   (simulated via env-overridden `$HOME`), asserting the re-keyed
   session resolves. ~100–150 LOC bash, CI gate.

---

## Implementation order (rough)

1. **`.sgbundle` format + manifest** — define the schema, the tar+zstd
   writer/reader, `handoff inspect` (pure read) to sanity-check manifests
   before any writes land.
2. **Per-tool handoff mapping on `ToolDefinition`** — a `session_layout`
   block (sibling to `home_dir_layout`) declaring key kind + which
   artifacts + which fields are paths. Claude Code first.
3. **`handoff pack`** — read-only capture into a bundle; secret
   exclusion; manifest population.
4. **apply state machine skeleton (dry-run)** — stages 0–5 on staging
   only, event-log writes per transition, no real placement yet.
5. **Re-key + remap** — reuse `reconciler.rs` adapter dispatch for the
   path rewrites; implement the path-derived-dir re-key for Claude Code.
6. **place + register + undo integration** — the writes, wired into the
   event log so `sessionguard undo` reverses a handoff.
7. **Codex `session_layout`** — second tool, flushes out the cwd-key
   path vs. dir-name path difference.
8. **`handoff-dogfood.sh`** — CI gate with simulated cross-home apply.
9. **Docs** — README `handoff` section; retire this doc to
   `docs/history/` when shipped.

Reviewable as ~5–6 commits, mirroring the migrate push-direct flow.

---

## Acceptance criteria

handoff v1 ships when:

- [ ] `handoff pack claude_code --project <p> -o b.sgbundle` on macOS
      produces a bundle whose manifest correctly records source home/OS
      and the path-derived session key.
- [ ] `handoff apply b.sgbundle --to <p2>` on a Linux box (or simulated
      via `$HOME` override) re-keys + remaps so `claude --resume` opens
      the session with history intact.
- [ ] The source machine's session is byte-identical after pack
      (read-only proof).
- [ ] `sessionguard undo` reverses an apply: placed artifacts removed,
      any `.pre-handoff` backup restored.
- [ ] No credential file ever appears in a bundle; `inspect` lists what
      was excluded.
- [ ] `--dry-run` on `apply` makes zero FS/registry changes.
- [ ] `handoff-dogfood.sh` passes in CI (Ubuntu + macOS) against a
      fake-tool fixture across two home/OS layouts.
- [ ] README has a "Handoff" section with the Mac→DOLOAMD example.

---

## Why commit to this design before code

Same three failure modes `migrate.md` guarded against, plus one new:

1. **Interface-first.** The `.sgbundle` manifest is the public contract;
   getting it right before code avoids baking machine-specific
   assumptions into the engine.
2. **Never destroy the displaced session.** Apply preserves a same-key
   session as `.pre-handoff-<unix>` — the cross-machine echo of the
   never-auto-delete rule.
3. **Undoable like everything else.** A handoff that isn't reversible
   reintroduces the distrust `undo` exists to kill.
4. **(New) Secrets never travel.** The one-way door that, if we got it
   wrong, would turn a convenience feature into a credential-leak vector.
   Enshrined as non-negotiable here, before any code can compromise it.

This doc is the contract. Code that contradicts it bounces back here.
