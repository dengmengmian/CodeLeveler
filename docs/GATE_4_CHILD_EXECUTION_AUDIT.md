# Gate 4 · Child execution audit — where the reusable primitive already is

Audited at `2456be7`. Purpose: find the smallest thing to expose so the **harness** can run a
child, without building a second runtime. Conclusion first: **the primitive already exists and is
already role-parameterised.** Gate 4's remaining work is one public entrance and one call site,
not an architecture.

## The ten locations

| # | Concern | Location | Reusable as-is? |
| --- | --- | --- | --- |
| 1 | `spawn_agent` tool schema | `leveler-tools` spawn tool def (`SPAWN_AGENT_TOOL`) | Model-facing only. **Not** on the harness path. |
| 2 | `spawn_agent` dispatch | `drive.rs:1184` (`call.name == SPAWN_AGENT_TOOL`) | Model-facing only. |
| 3 | Child creation / admission | `drive.rs:1780–1895` — parses role/files/model, emits `SubAgentStarted`, pushes to `accepted` | Argument parsing is model-specific; **the emission of `SubAgentStarted` is not** and must be replicated by the harness entrance. |
| 4 | Child `Executor` construction | `executor.rs:1149 child_for_role_on(role, files, model_override)` | **Yes.** Role decides registry (`read_only_subset`), write allowlist, parallelism, prompt suffix. |
| 5 | Child context | inherited inside (4): `runtime`, `tool_context`, `registry`, `model`, approver, ownership fence | **Yes** — nothing to thread. |
| 6 | Limits | `residual_step_limits(...)` + `ParentWallBudget` + `Semaphore` permit, all consumed inside `run_one_sub_agent_on` (`handlers.rs:156`) | **Yes**, including the post-queue wall refresh at `handlers.rs:185`. |
| 7 | Child terminal result | `SubAgentRunResult { text, ok, progress, modified_files }` (`handlers.rs:322`); `ok` derives from `StopReason` | **Yes.** This is also Gate 5's input — a second entrance must produce the *same* struct or Gate 5 has two result shapes to fix. |
| 8 | EventLog child events | `SubAgentStarted` / `SubAgentFinished` emitted by (3) via `observer`; `SubAgentProgress` / `SubAgentActivity` emitted **inside** the primitive via the `progress` channel | Split. Progress/activity are free; **started/finished are the caller's job.** |
| 9 | Parent wait / aggregation | `drive.rs:1896–1960` builds futures, `drop(progress_tx)`, joins, rolls spend into the parent ledger | Fan-out bookkeeping — a single harness child does not need it. |
| 10 | Cancellation | `cancellation.child_token()` per child, passed into the primitive and into `child.run(...)` | **Yes.** |

## The minimal primitive

`Executor::run_one_sub_agent_on` (`handlers.rs:156`, `pub(crate)`, `&self`) already encapsulates
6, 5, 4, 10, the activity re-emission, partial-progress capture on cancellation, the lifecycle
hooks, and the result normalisation of 7. It takes a `task: String` and an `AgentRole` — nothing
in it knows or cares that a model asked for the child.

So the two entrances differ by **exactly three things**: who supplies the task text, who emits
`SubAgentStarted`/`SubAgentFinished`, and who consumes the result.

```
                 ┌── entrance 1: model tool-call ── drive.rs:1184 ─┐
task + role ─────┤                                                 ├──► run_one_sub_agent_on ──► SubAgentRunResult
                 └── entrance 2: harness policy ─── turn.rs ───────┘        (unchanged)
```

## What has to be added

1. `AgentRole::Reviewer` — read-only registry (same branch as `Explorer`), own label and prompt
   suffix. Five match sites total: `executor.rs:750/1158/1170/1402`, `sub_agent.rs:143`.
2. One public entrance on `Executor` that emits `SubAgentStarted`/`SubAgentFinished` around the
   primitive and forwards the `progress` channel into the caller's observer.
3. One call site in `turn.rs` — the file that already declares itself "the ONE place the engine
   drives an `Executor`" — after the main run returns and **before** `drop(events)`, so the
   reviewer's events are persisted in the same turn the audit reads.

## Deliberate non-changes

No new crate, no second scheduler, no second `ToolHost`, no child state machine, no change to
`ExecutorFactory`, no change to the `spawn_agent` schema or its dispatch. `engine.rs`'s N7
downgrade is untouched: it keeps asking "did a review happen?" — the answer just stops being
"no" when policy says one was required.

## One structural obstacle (and its minimal fix)

`turn.rs:242` binds `let mut executor`, then the match arms consume it via
`with_objective(mut self) -> Self`. `run`/`run_conversation`/`resume` all take `&self`, so
hoisting the `with_objective` call above the match keeps the executor alive after it, at the cost
of one `Option<ObjectiveAnchor>` binding. That is the whole ownership change.
