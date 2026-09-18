# Observability: making "it did nothing" visible

**Status:** design approved 2026-09-18, implementing.
**Scope:** local-only. No data leaves the machine — no endpoint, no phone-home,
no third party. Everything here is readable by the operator on the box it
happened on.

## Why this exists

SessionGuard's core promise — your AI sessions survive a project move — was
false for Claude Code, Codex and OpenCode for months, and **nothing in the
product said so**. The v0.9.0 re-key work fixed the capability. This document
is about the second half of that failure: why it stayed invisible.

Here is the shape of it, from `reconciler.rs` before this work:

```rust
ReconcileStrategy::Notify => {
    info!(tool = %tool.name, "notify-only strategy, no paths rewritten");
    ReconcileResult { actions_taken: vec![], success: true, error: None }
}
```

`success: true` with zero actions. Every time the daemon declined to do
anything, it recorded a success. **"Worked" and "did nothing" were the same
value in the data model**, so no amount of log-reading could have
distinguished them. The `info!` line was even *there* — it told nobody
anything, because a healthy daemon and a totally inert one produced
indistinguishable output.

That is the failure class this design targets. A second one, chosen
alongside it: a daemon that is dead, watching the wrong roots, or silently
several releases behind (the `fedora` host sat on 0.7.0 across four releases
without anything noticing).

Explicitly **not** in scope: growth/orphan trend sampling, and duration
histograms. Nothing today suggests either is hurting, and both can be added
later against the same table.

## The core idea: outcome is a type, not a bool

A boolean cannot express "succeeded and did nothing", so it gets used for
"didn't fail", which is not the same thing. Replace it:

```rust
pub enum Outcome {
    /// Did work. Carries how much, so "acted on 0 things" is unrepresentable.
    Acted { actions: usize },
    /// Deliberately did nothing, and why. NOT a failure — but not a success
    /// either, and it must be counted separately from `Acted`.
    NoOp(NoOpReason),
    /// Declined on purpose, to protect data (destination store exists, DB
    /// locked). The operator needs to act.
    Refused { reason: String },
    /// Tried and broke.
    Failed { error: String },
}
```

`Acted { actions }` is constructed only with a real count, so the exact v0.9
shape — success with an empty action list — cannot be written. The
distinction is then carried all the way to the surface: a `NoOp` is reported
*as* a no-op, never folded into a success tally.

`NoOpReason` is an enum, not a string, because the reasons are a closed set
worth matching on (`ToolDeclaresNotify`, `NoArtifactsFound`,
`NoStoreForProject`, `PathAlreadyCurrent`, …). A string reason would have
let the v0.9 case hide as prose.

## Where it is recorded

One store (`event_log.rs`), with tables separated by **durability class** —
this is the design's main structural decision:

| table | class | retention |
| --- | --- | --- |
| `events`, `migrations`, `rekeys` | **undo-critical** — these back `sessionguard undo` | never auto-pruned |
| `activity` (new) | **observability** — disposable | bounded by age + row count |

Mixing the two would mean either pruning records that `undo` depends on, or
never pruning at all and letting the log grow without bound. Splitting by
durability lets each get the policy it needs, while keeping one database, one
connection, and one place to look.

`activity` records one row per *decision*:

```sql
CREATE TABLE activity (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp    TEXT NOT NULL DEFAULT (datetime('now')),
    kind         TEXT NOT NULL,   -- reconcile | rekey | daemon
    tool_name    TEXT,            -- NULL for daemon-level rows
    project_path TEXT,
    outcome      TEXT NOT NULL,   -- acted | noop | refused | failed
    reason       TEXT,            -- NoOpReason / refusal / error detail
    actions      INTEGER NOT NULL DEFAULT 0
);
```

No-ops are recorded **on purpose**. That is the entire point: a store of
actions taken cannot answer "why did nothing happen?", which is the question
that went unanswered for months.

### Retention

Pruned on daemon startup and after each write batch: keep rows newer than
`activity_retention_days` (default 30) **and** at most
`activity_max_rows` (default 50 000), whichever is tighter — both
configurable in `config.toml`.

This lands the prune/vacuum machinery that hardening item **M17** (unbounded
event-log growth) calls for, but applies it only to `activity`. Wiring it to
`events` is a deliberate follow-up: those rows back `undo`, so pruning them
is a semantic decision about how long undo stays available, not a storage
one. M17 is therefore *partially* addressed here, not closed.

## Health is derived, never stored

A stored health record is a lie the moment the process dies. Everything
health-related is computed at query time:

| signal | source |
| --- | --- |
| daemon alive | PID file + the existing identity check (`is_sessionguard_process`) |
| version | `CARGO_PKG_VERSION` of the running binary |
| watch roots: configured vs real | `config.watch_roots` vs `is_dir()` on each |
| last activity | newest `activity` row |
| ever acted? | newest `activity` row with `outcome = 'acted'` |
| launcher health | existing `health.rs` |

"Configured vs real" catches the watch root that was renamed out from under
the config — a silent way to become inert. "Ever acted?" is the direct
antidote to the v0.9 failure: a daemon that has been running for a month with
zero `acted` rows is reported as **inert**, loudly, rather than as healthy.

Surfaced via `sessionguard status --deep` (text + `--format json`), and in the
dashboard.

## Logging

Tracing today is 48 call sites, 23 of them `warn!` — we mostly log when
unhappy, so there is no record of normal operation. Worse, the modules that
mutate data are the quietest: `rekey.rs` has 2 sites, `migrate/mod.rs` has 1,
and `main.rs` has none (it uses `println!`).

- Every mutating operation logs at `info!` with structured fields
  (`tool`, `project`, `outcome`, `actions`) — never bare prose.
- Refusals log at `warn!` with the reason as a field.
- `main.rs` keeps `println!` for *user-facing output* (that is the UI), and
  gains `tracing` for anything diagnostic. The two are not the same channel;
  stdout stays clean for `--format json` consumers, as it already does.

## Fleet scope (explicitly deferred)

Local-only was chosen, so **each machine answers for itself** — there is no
`stats --all-hosts` here. Cross-machine drift stays with the existing
read-only `sessions --host` ssh transport, which already ships and already
carries version checking (`check_remote_version`). A host's own
`status --deep --format json` is readable over that same transport if we want
fleet health later; nothing in this design blocks it.

## Test isolation defect (fixed as part of this work)

`handle_session_event` resolves `$HOME` through `config::home_dir()` to
re-key home-dir stores. The `daemon.rs` unit tests never set `HOME`, so they
read the operator's **real** stores — measured at 2.63 s vs 0.02 s on one
test, walking a real 4.8 GB Codex tree. It violates the convention `CLAUDE.md`
states outright, taxes every run, and is a latent mutation hazard: a temp path
that collided with a real project key would rename a real store directory.

CI never caught it because runners have empty homes — it only bites on a
developer's machine.

Fixed by **dependency injection, not environment fiddling**: the census root
is resolved once in `daemon::run()` and passed down to
`handle_session_event`. Setting `HOME` in tests would work but is
process-global, and tests run in parallel threads — a data race, and `set_var`
is `unsafe` in the 2024 edition. Passing the path is also faster: resolved
once at startup rather than per event.

A `scripts/rekey-dogfood.sh` lands alongside it, gated in CI next to the
other three: re-key is the newest mutating operation and the only one
without an end-to-end smoke test.

## Testing

- `Outcome` makes the regression unrepresentable; a test asserts the
  `Notify` strategy yields `NoOp(ToolDeclaresNotify)`, not a success.
- A test asserts `status --deep` reports **inert** for a daemon with activity
  rows but no `acted` row — the v0.9 scenario, caught.
- A test asserts retention prunes `activity` and leaves `rekeys`/`migrations`
  untouched, since that boundary is the design's load-bearing decision.
- `daemon.rs` tests pass an explicit temp root; a test asserts no real-home
  read by pointing the root at a temp dir and checking the store is untouched.
- `rekey-dogfood.sh` drives re-key → undo end to end against a throwaway home.
