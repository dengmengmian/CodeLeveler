# Long Goal P1 — Goal Identity

**Status:** implemented · 2026-08-25 · **No execution behavior changed**

Gives a goal an identity that outlives the process running it. Nothing more:
no resume, no scheduler, no background execution.

Prior stage: [LONG_GOAL_CLOSURE_ANALYSIS](LONG_GOAL_CLOSURE_ANALYSIS.md)

## Why a goal exists as a thing

Before this, a goal was `is_goal = true` on a turn. That worked while the
process lived and left nothing when it did not.

```
Process A                      Process B (restart)
  goal → turn → interrupted      ??? — nothing says work was owed
```

The audit's finding, unchanged: **most long-running infrastructure already
exists.** Work windows, progress accounting, stagnation detection, turn reaping
and evidence-gated completion are all built and load-bearing. The one thing
missing is that a goal had no name a second process could ask about.

## Why a table, not columns on `tasks`

The audit proposed goal fields on `tasks`. **That was wrong, and the code says
why.**

A session stays open. The user runs `/goal X`, it settles, they run `/goal Y`.
Both run against the same session, so `tasks.ensure_for_session` returns the
same task id. And `sessions.status/outcome` — the single lifecycle
projection — is overwritten by the second goal.

So today **goal history is lost**, and one task hosts many goals. That is 1:N;
columns cannot express it.

The test that pinned this down: `goal_turn_includes_prior_history_in_model_request`
proves a goal turn runs inside a session that already has history.

## The model

```sql
goals(id, task_id, objective, state, opened_at, settled_at, windows_run)
```

| Column | Answers |
| --- | --- |
| `id` | which goal — stable, process-independent |
| `task_id` | which execution history it belongs to |
| `objective` | what the user actually asked, verbatim |
| `state` | does this still owe work? |
| `windows_run` | how much has already been spent on it |

### Two states, on purpose

```rust
enum GoalState { Running, Settled }
```

**There is no `Interrupted`.** A goal left `Running` by a process that died *is*
the interrupted one, and `tasks.owner_runtime_id` / `owner_epoch` already record
whether a live runtime is driving it. A third state would be a second writer for
a fact ownership already holds — the exact mistake `migration 0016` avoided when
it kept lifecycle on `sessions`.

### The table does not store success or failure

`sessions.outcome` has one writer and keeps it. `goals.state` answers a
different question, and a settled goal points at its session for how it went.

Conflating the two would give the same fact two owners, which is how a
projection and its source drift.

### An unknown state is refused, not defaulted

```rust
"interrupted" => None,   // not a state we wrote; refuse the row
```

A row we cannot interpret must **never** silently become `Settled` and stop
being reported as owed work. This project has read a missing measurement as a
measured zero four times now — in the eval observer, in the profile
aggregation, in the TUI projection, and in a feature-gated test reporting
"0 tests, ok". The same shape, refused here up front.

## Relationship with Task and Turn

```
Session ─── 1:1 ─── Task ─── 1:N ─── Goal
                      │
                      └─── 1:N ─── Turn (work windows)
```

A goal is **not** a layer between Session and Task. Task and Session are 1:1
today, so inserting anything between them would be a duplicate concept.

A goal sits *beside* turns, hanging off the task: turns are executions, a goal
is the intent several of them serve. When multi-session tasks eventually arrive
(the extension `migration 0016` deferred), `goals.task_id` follows the task and
needs no change.

## Wiring

Two call sites in `interactive.rs`, on the goal path only:

```rust
let goal = open_goal_record(&app, &session_id, &objective).await;  // before work
…
settle_goal_record(&app, goal).await;                              // at terminal
```

**Best-effort by design.** A goal whose bookkeeping cannot be written still
runs; `open_goal_record` returns `None` and every caller treats that as
*nothing to settle*, never as *already settled*. Losing the bookkeeping is
worse than refusing the work only if you believe the bookkeeping is the point.

`GoalStore` joins `EngineStores` alongside the existing ports, following
`TaskStore`'s shape exactly: narrow trait, SQLite adapter, `MemoryGoalStore`,
and one contract test run against both.

## The resume boundary

This phase stops at identity. The boundary is asserted, not just described:

```rust
#[tokio::test]
async fn discovering_an_unfinished_goal_does_not_continue_it() {
    …
    assert_eq!(after, before,
        "listing owed work must not advance, settle or re-drive it");
    assert_eq!(after.windows_run, 0, "no window ran");
}
```

If that test ever fails because a window was recorded or a state moved without
a caller asking, **a resume policy was implemented by accident** — the one
outcome this phase must not produce.

### Why the default should stay "do not auto-resume"

A goal interrupted mid-mutation is not obviously safe to continue unattended.
This project's own dogfood history says an unattended agent resuming into a
broken workspace produces confident wrong work.

P2 should make owed work *visible* and let a person decide. Auto-resume becomes
an opt-in policy when there is evidence it is safe — measured, not assumed.

## Tests

`crates/leveler-storage/src/goal_store.rs` — 8, one contract against both
implementations: idempotent settle keeping the first timestamp, many goals per
task, survival across a reconnect, cascade with the session, refused unknown
state.

`crates/leveler-app/tests/goal_identity.rs` — 6:

| # | Assertion |
| --- | --- |
| 1 | A goal has an identity the moment it opens |
| 2 | **An unfinished goal is discoverable after a restart** (real file, reconnect) |
| 3 | Goal → task → session resolves both ways |
| 4 | A session that never opened a goal is untouched |
| 5 | **Discovery does not continue anything** |
| 6 | A second goal does not overwrite the first |

Regression: 19 suites, 360 tests, 0 failed across `leveler-storage`,
`leveler-app` and `leveler-core`.

## What P1 deliberately does not do

Auto-resume · scheduler · background execution · UI · remote workers · workflow
engine · goal lifetime bound · multi-session goals.

The lifetime bound (P4) and the resume policy (P3) both want to exist. Neither
should be built before P2 has been run against a real interruption, because P2
is what tells us whether the resume question is the one users actually have.

## Related

- [LONG_GOAL_CLOSURE_ANALYSIS](LONG_GOAL_CLOSURE_ANALYSIS.md) — the audit
- `migrations/0018_goals.sql`
- `crates/leveler-engine/src/continuation.rs` — the work-window layer P1 records
