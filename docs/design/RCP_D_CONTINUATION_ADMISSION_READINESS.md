# RCP-D — Unified Continuation Admission: implementation readiness

Read-only audit. Nothing here is implemented; RCP-D changes whether the next
model call happens at all, which is the one thing in this program that can move
GOOD, and it should be read before it is built.

Written against `02e64aa`, with RCP-A/B/C landed. Every claim below was read
off the code at that commit.

## The question RCP-D answers

> Can every path that causes another main-loop model call be made to ask the
> same authority for permission?

Not "can one function do everything." The gates themselves are local and should
stay local. What is scattered is the *decision that follows a gate*: whether the
runtime spends another model call on the result.

## D1 — Every model-call admission point

`starts a call?` means: does taking this path cause another **main-loop** model
request. Advisory and fold calls are listed separately because they are the
runtime's own overhead, not the loop's next move.

### Inside one drive (`leveler-agent/src/executor/drive.rs`)

| # | Path | Owner | Input it reads | Bound today | Starts a call | Should stay local |
|---|---|---|---|---|---|---|
| 1 | Tool batch → next round | round loop (`loop {}` at 709) | nothing — falls through | round ceiling (100), budgets, thrash guards | yes | **no** — this is the one that matters |
| 2 | `update_goal` refused | gate at 1853 | plan/evidence state | none of its own | yes | gate local, follow-up **no** |
| 3 | Blocking finding open | gate at 1802 | evidence ledger | none of its own | yes | gate local, follow-up **no** |
| 4 | Outstanding children | gate at 1740 | child registry | none of its own | yes | gate local, follow-up **no** |
| 5 | Reconciliation refused | judge at 2194 | judge verdict | none of its own | yes | judgment local, follow-up **no** |
| 6 | Closeout nudge | `closeout::decide` | quiet round + phase | `CLOSEOUT_NUDGE_BUDGET = 2` | yes | already bounded; report to admission |
| 7 | Observe-thrash forced answer | round loop | no-progress streak | once per drive | yes | already bounded |
| 8 | Policy-blocked directive | round loop | `policy_blocked_streak` | `policy_blocked_rounds = 3`, hard stop at 2× | yes | already bounded |
| 9 | Plan soft nudge / engagement advisory | round loop | explore rounds without a plan | once each | yes | already bounded |
| 10 | Steering message | `SteeringSource` | user input | user-driven | yes | **yes** — a human boundary |
| 11 | Child settlement notice | `settle_finished_children!` | child results | child count | yes | gate local, follow-up **no** |
| 12 | Decode retry | round loop | malformed tool JSON | `MAX_DECODE_RETRIES = 2` | yes | **yes** — protocol repair |
| 13 | Length continuation | round loop | `finish_reason: length` | `MAX_LENGTH_CONTINUATIONS = 2` | yes | **yes** — protocol repair |
| 14 | Empty `tool_calls` retry | round loop | provider glitch | bounded | yes | **yes** — protocol repair |
| 15 | Deadline re-entry | round loop | cancellation + expiry flag | one pass to the boundary | no | **yes** |
| 16 | Compaction fold | context budget | context tokens | context budget | (own call) | **yes** |
| 17 | Contract derivation | `completion_contract::derive_contract` | original task | once per goal, retried once | (own call) | **yes** |
| 18 | Reconciliation judge | `reconciliation::reconcile_completion` | claim + evidence | per close attempt | (own call) | **yes** |

### Across windows (`leveler-engine/src/engine.rs::supervise`)

| # | Path | Owner | Input it reads | Bound today | Starts a call |
|---|---|---|---|---|---|
| 19 | `DriveGoalAgain` | `SupervisorPolicy::after_turn` + engine clamp | stop reason, progress ledger, `WindowState` | `MAX_SUPERVISED_TURNS = 32`, `MAX_NO_PROGRESS_WINDOWS = 2`, remaining task total | yes — a whole window |
| 20 | Round extension | `round_extension_earned` | segment baseline vs now | `TaskRoundBudget::max_extensions` | yes — converts Stop into a window |
| 21 | `ExtendBudget` | `after_turn` | budget exhaustion dimension | `MAX_EXTENSIONS = 2` | yes — resumes the drive |
| 22 | Repair turn | verify loop | verification verdict | `DIRECT_REPAIR_ATTEMPTS = 1` | yes — a whole turn |
| 23 | Closure reviewer | `closure_review_stage` | change shape | one per run | (child) |

**The shape of the finding.** Rows 19–22 already pass through something that
looks like an admission authority: `after_turn` plus the engine's own clamps,
now reading a durable `WindowState`. Rows 1–5 and 11 do not pass through
anything at all. A refused `update_goal` writes a tool result and the loop
simply comes round again. Nothing anywhere asks whether another model call is
worth it — that is the gap, and it is where C3's 190 root rounds came from.

## D2 — What must stay local

**`update_goal` rejection.** The validation is tool-level and belongs in the
tool path: the model asked to close, the gate says why not, and the answer is a
tool result. Do not move that. What is *not* tool-level is what happens next —
today, unconditionally, another model call.

**Reconciliation.** The judgment needs in-flight context (the claim, the recent
evidence, the freshness of the last verification) that only the drive holds.
Keep it there. But "the judge refused; spend another round?" is an admission
question, and today it is not asked.

The rule that falls out: **a gate decides whether an action is allowed; it must
not decide whether the runtime spends another model call.** Those are two
questions and the second has one right owner.

## D3 — A terminating tool result

Pi's `ToolResult.terminate` is the seam CodeLeveler lacks: a deterministic tool
result that says *no follow-up model call is needed*.

Where it would pay here:

- A read-only tool whose result the model already has in context.
- `update_goal(complete)` **accepted** — the task is over; the round after it is
  pure overhead.
- A refusal whose remedy is mechanical and already stated in the result.

Recommendation: **do not** model it as a boolean on every tool result. Model it
as one field on the round's verdict — the drive already computes a round verdict
(`round_verdict.rs`) — saying whether this round produced anything the model has
not already seen. That keeps the tool registry ignorant of continuation policy,
which is the property worth protecting.

Not implemented. Measure first: instrument how many rounds end with every tool
result already-known before granting anyone the power to skip them.

## D4 — Proposed `AdmissionInput`

Everything below exists and is durable after RCP-A/B/C. Nothing here is a model
judgment.

```
TurnOutcome            stop reason, stop detail, structured budget exhaustion
WindowState            window_index, extensions, round_extensions,
                       windows_without_progress, segment baseline   (RCP-C)
CompletionDebt         summary only — never re-derived here          (unchanged)
Workspace/Evidence     modified files, mutation ops, fresh-verify mark
RuntimeUsageProjection tokens, cost, and the estimated share         (RCP-A)
Deadline               remaining wall clock
HumanBoundary          steering pending, approval denied, cancellation
Outstanding            unconsumed child settlements, live children, reviewer
```

Deliberately absent: any model-scored "is this going well". The whole point of
this program is that the runtime already has the facts.

## D5 — Proposed output

```
ContinueCurrentWindow { reason }
StartNextWindow       { rounds }
Closeout              { nudge }
StopIncomplete        { detail }
StopBlocked           { detail }
Settle
```

`Closeout` is separate from `ContinueCurrentWindow` on purpose: the closeout
nudge is already budgeted, and folding it into a generic continue would lose
that bound.

## D6 — The critical question

> Can every path that leads to another main model call be made to require
> permission from one admission authority?

**Almost. Three exceptions, each for a reason, each nameable.**

1. **Protocol repair** (rows 12–14). A malformed tool call, a truncated
   response, a `tool_calls` finish with no calls: these are not the runtime
   choosing to spend a round, they are one logical round failing to complete.
   They are already tightly bounded (2 each, consecutive-only). Routing them
   through admission would mean asking "is this task worth continuing" about a
   retry of a call that has not happened yet.

2. **Human boundary** (row 10). A user's steering message is not a runtime
   decision. Admission may not veto it.

3. **Runtime overhead calls** (rows 16–18). A fold, a contract derivation, a
   reconciliation judge — these are the runtime's own spend, not the loop's next
   move. They belong under **spend** admission, which RCP-A has already unified,
   not under continuation admission.

Everything else — rows 1–9, 11, and 19–22 — can and should ask one authority.

So the honest target is:

```
ONE_MODEL_CALL_ADMISSION_AUTHORITY = achievable
  for every runtime-chosen continuation,
  with protocol repair, human input, and runtime overhead
  named as explicit non-participants rather than forgotten ones.
```

## What to decide before building this

1. **Row 1 is the whole game.** Rows 2–5 are cheap. Putting the plain tool-batch
   follow-up behind admission is what changes C3, and it is also what can break
   GOOD if the predicate is wrong. It should land alone, behind its own
   behavioural proof, before anything else in D.

2. **The first predicate should be conservative to the point of boring.** The
   only case with evidence behind it today: a round whose every tool result the
   model has already seen, in a drive already past its closeout budget. Start
   there and measure.

3. **C3 has not been re-measured.** RCP-A/B/C changed no continuation policy, so
   C3 should still be 203 requests. Confirming that before D starts is what
   makes any later reduction attributable.
