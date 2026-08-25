# Long-running Goal Closure — Phase 0 architecture audit

**Status:** audit · 2026-08-25 · **No code changed**

Answers one question before anything is built: *what is actually missing
between CodeLeveler today and a Long-running Goal capability?*

The short answer is less than expected. Work windows, progress accounting,
stagnation detection, turn reaping and evidence-gated completion all exist and
are load-bearing. What is missing is narrower and sharper: **a goal that
outlives the process that was running it has no owner.**

## 1. Current architecture

### The unit of work is a task, and a task is a session

```
tasks            id · session_id (UNIQUE) · owner_runtime_id · owner_epoch
sessions         status · outcome          ← lifecycle lives here
events           append-only, per session
```

`migrations/0016_tasks.sql` is explicit about the boundary it did *not* cross:

> `session_id` is UNIQUE: today every task has exactly one primary session.
> Multi-session tasks are a future migration, not an implicit capability.

And about why lifecycle stayed on `sessions`:

> Moving them here would create a second writer for the same fact.

That restraint is correct and it is also the constraint this phase runs into.

### There is no `Goal` type

`grep` finds no `struct Goal`, no `GoalState`, no goal table. A "goal" is a
turn started with `is_goal = true`, driven by `spawn_direct_goal_turn`, whose
distinguishing property is that a supervisor may open more turns after it.

**A goal is a run-time behaviour, not a durable noun.** Everything durable
about it is reconstructed from the session's events and its progress ledger.

### Work windows are real

`leveler-engine/src/continuation.rs` is the layer that makes long work
possible, and it is well-factored: the engine owns the mechanism, a
`SupervisorPolicy` owns the judgement.

```rust
enum Continuation { Stop, DriveGoalAgain, ExtendBudget(_) }
```

The doc comment states the invariant the whole design rests on:

> The per-turn round ceiling ends a WORK WINDOW, not the goal.

Boundaries a policy cannot weaken — cancellation, hard budgets, the absolute
round ceiling, the eval round limit — are checked by the engine *before* a
policy is consulted. That is the right shape: product judgement is pluggable,
safety is not.

## 2. Existing capabilities — verified, not assumed

| Q | Capability | State | Where |
| --- | --- | --- | --- |
| 1 | Durable goal abstraction | ❌ **absent** | no type, no table |
| 1 | Work window | ✅ | `Continuation::DriveGoalAgain` |
| 2 | Session persistence + event replay | ✅ | `events`, `RawTranscript::load_strict` |
| 2 | Resume | ⚠️ **manual only** | `TaskEngine::resume` |
| 3 | Turn terminal states | ✅ closed set | `TurnOutcome{Completed,Failed,Interrupted}` |
| 3 | Zombie turn reaping | ✅ ownership-aware | `reap_after_restart` |
| 4 | Progress signal | ✅ | `ProgressLedger` |
| 4 | Stagnation detection | ✅ | `stagnation_streak` |
| 4 | Cross-window no-progress | ✅ | `windows_without_progress` / `MAX_NO_PROGRESS_WINDOWS` |
| 5 | Evidence-gated completion | ✅ | no mutation or no gates → never `Verified` |
| 6 | Background process ownership | ✅ | `KillOnDrop`, `kill_scope`, `kill_all` |
| 6 | Unfinished child settlement | ✅ | `unfinished_children()` at turn end |

### Progress truth is better than expected

`ProgressLedger` does not accept an edit as progress:

> Edits alone do NOT reset it (an edit is not progress until a check passes),
> so a "keep editing while the check keeps failing" loop accumulates here and
> is force-stopped.

That is the correct definition. The single most common long-task failure is
not a crash — it is an agent that stays busy without moving, and this ledger
is already built to catch exactly that.

`cumulative_rounds` and `cumulative_model_tokens` are tracked *across*
continues and resumes, so budget accounting already survives a window boundary.

### Completion truth is already evidence-gated

`engine.rs` refuses to claim `Verified` when there is nothing to verify:

```rust
// K19 early short-circuit: no mutation or no gates → never claim Verified
if outcome.modified_files.is_empty() || !spec.coding.verification.has_gates() {
    return … TaskOutcome::CompletedUnverified;
}
```

`TaskOutcome` distinguishes `BudgetLimited` from `Failed`, with the comment:

> Execution stopped at an explicit resource boundary. The task is incomplete
> and resumable; this is not evidence of model failure.

The model saying "done" is not what produces `Verified`. That question is
already answered.

## 3. Missing invariants

Four, in descending order of how much they matter.

### M1 — A goal does not survive the process that ran it

This is the gap.

On restart, `reap_after_restart` finds turns still marked `Running`, confirms
this runtime owns them, and writes their terminal event. **That is cleanup, not
continuation.** Nothing then re-drives them.

`TaskEngine::resume` exists and works, but it is a command someone must issue.
No component asks "which goals were interrupted, and should any continue?"

The invariant that does not hold:

> A goal that was not finished, not blocked, and not cancelled is still owed
> work by whichever runtime owns it.

Today an interrupted goal becomes a durable `Interrupted` record and waits for
a human who may not know it is waiting.

**This is the one thing a "long-running" capability means that CodeLeveler does
not yet do.**

### M2 — There is no goal identity to resume *to*

M1 cannot be fixed cleanly while a goal is only a turn flag.

To resume a goal, a runtime needs to know: what the objective was, which
windows have run, how much budget the goal (not the turn) has spent, and
whether the objective was ever revised. Most of those facts exist —
`ProgressLedger` carries `objective_version` and cumulative spend — but they
are per-session ledger state reconstructed at load, not a queryable goal
record.

`tasks` is the natural home and was deliberately left minimal. The migration
note is the guide here: lifecycle already has a writer, so a goal record must
add *goal* facts, not duplicate session facts.

### M3 — No stated ceiling on a goal's total lifetime

Every bound today is per-window or per-epoch: round ceiling,
`MAX_BUDGET_EXTENSIONS`, `MAX_NO_PROGRESS_WINDOWS`, `cumulative_rounds` within
a task epoch.

There is no answer to *"this goal has been running for six hours across nine
windows — should it still be running?"* Today the no-progress counter is the
only thing that ends a long goal that is not converging, and it measures
*stagnation*, not *cost*. A goal that makes a little progress in every window
can run indefinitely.

Not urgent, but it is the boundary a user will ask about first, and it should
be a stated policy rather than an emergent one.

### M4 — Resource ownership is per-session, not per-goal

`kill_scope` reaps a session's background tasks. Sandbox leases and browser
resources are session-scoped too. If a goal ever spans sessions (M2's likely
shape), those scopes stop matching the unit that owns them.

Nothing is leaking today — the audit found reaping on every exit path
including `Quit`, plus `Drop`-based fallbacks. This is a constraint on M2's
design, not a present defect.

## 4. Proposed minimal changes

Deliberately small. The existing continuation layer is good; this should extend
it rather than replace it.

### Change A — Goal record on `tasks`

Add goal facts only, keeping `sessions` the single lifecycle writer:

```
objective        TEXT      what the user asked, verbatim
goal_state       TEXT      running | interrupted | settled | abandoned
windows_run      INTEGER
opened_at        TEXT
last_window_at   TEXT
```

`goal_state` is **not** a copy of session status. It answers a different
question: *does this goal still owe work?* A session can be `Completed` for
this window while its goal is `running`.

### Change B — Resume decision, behind a policy

Mirror `SupervisorPolicy` exactly. The engine owns the mechanism (find owned
goals in `interrupted`, re-drive); a `ResumePolicy` owns the judgement
(should this one continue?).

The default policy should be conservative. A goal that was interrupted by a
crash mid-mutation is not obviously safe to continue unattended, and this
project's own history says an unattended agent that resumes into a broken
workspace produces confident wrong work.

**Proposed default: report, do not auto-resume.** On startup, surface
interrupted goals and let the user choose. Auto-resume becomes an opt-in
policy once there is evidence it is safe — measured, not assumed.

### Change C — A stated goal lifetime bound

One durable counter (`windows_run`, already implied by Change A) plus a
configured maximum. When it trips, the goal settles as `BudgetLimited`, which
already exists and already means *incomplete and resumable*, not *failed*.

### Explicitly not proposed

- **No new event types for goal state.** `EventLog` is what happened; goal
  state is what is true now, which is the ledger/table side. This is the same
  split the Contribution Inspector settled.
- **No change to `SupervisorPolicy`.** Within-task continuation is solved.
- **No multi-session goals yet.** That is the migration `0016` deferred, and
  it should be driven by a demonstrated need, not by symmetry.

## 5. Implementation phases

| Phase | Content | Gate |
| --- | --- | --- |
| **P1 Goal identity** | Change A: migration + goal record written at goal start and at each window boundary | A goal survives a restart as a queryable record; existing tests unchanged |
| **P2 Interrupted-goal visibility** | On startup, list owned goals in `interrupted`; surface in TUI and CLI | Kill the process mid-goal, restart, the goal is named and offered |
| **P3 Resume policy** | Change B: `ResumePolicy` trait, conservative default | A policy can be swapped without touching the engine; default does not auto-resume |
| **P4 Lifetime bound** | Change C | A goal that never converges settles as `BudgetLimited` rather than running forever |
| **P5 Long-task dogfood** | Real repositories, real interruptions | See gate criteria |

P1 and P2 are the ones that matter. P3–P5 should not start until P2 has been
run against a real interruption, because P2 is what tells us whether the
resume question is even the one users have.

## 6. Release gate criteria

A Long-running Goal capability is closed when all hold:

1. **No ambiguous state.** After `kill -9` mid-goal and restart, every turn has
   a terminal event and the goal has a state. Already true for turns
   (`reap_after_restart`); needs to become true for goals.
2. **An interrupted goal is discoverable.** A user who did not see the crash
   can find out that work is owed, without reading the event log.
3. **Resume is a decision, not an accident.** Whatever the default, it is a
   stated policy with a test, not emergent behaviour.
4. **A non-converging goal terminates.** It settles as `BudgetLimited` at a
   stated bound rather than running until someone notices.
5. **No orphan resources across a goal boundary.** Processes, sandboxes,
   browser contexts and children are reaped by whatever owns the goal.
6. **Verified by interruption, not only by tests.** At least one real-repository
   goal killed mid-flight and recovered, in the shape of the Multi-Agent
   dogfood — which found two defects that 708 green tests did not.

## Method note

The pattern that closed Multi-Agent applies here and is the reason this
document exists before any code:

```
find the real failure mode → state the invariant → build the runtime → verify for real
```

The audit's most useful output is not the list of what to build. It is that
four of the six audited areas are **already done and better than assumed** —
progress truth, completion truth, turn reaping, resource ownership. Building
those again would have been the expensive mistake.

## Related

- [MULTI_AGENT_UX](MULTI_AGENT_UX.md)
- [TUI_MULTI_AGENT_PRODUCT_CLOSURE](TUI_MULTI_AGENT_PRODUCT_CLOSURE.md)
- `crates/leveler-engine/src/continuation.rs` — the work-window layer
- `migrations/0016_tasks.sql` — the boundary this phase runs into
