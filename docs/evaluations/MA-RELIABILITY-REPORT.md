# MA-RELIABILITY — Spawn Reliability Gate

**Opened:** 2026-08-23 · **Subject:** `main` @ `0a462660` · **Suite:**
`crates/leveler-agent/tests/spawn_reliability_gate.rs`

## Executive Summary

```
Spawn Reliability:  PARTIAL
```

Lifecycle integrity holds at every width tested, including past the cap. What is
**not** covered is the half of the gate that needs a runtime, not an agent
harness: parent cancellation, background-task failure, and a realistic
long-running baseline. Calling this PASS would claim coverage the suite does not
have.

## How this was measured, and why not with a model

The gate as originally scoped — "run the same task with 1, 2, 4, 8, 16 children"
— cannot be executed through a model. Delegation is opportunity-based: the model
elects to spawn, and forcing it is forbidden by two standing constraints (no
forced delegation; eval observes and does not special-case). The widest fan-out
ever observed in real use is three. Through a model, "eight children" is a coin
landing on eight.

Reliability, though, is a property of the runtime rather than of the model's
willingness. Scripting the fan-out gives exact concurrency, free repetitions —
**five per width instead of the n≥3 a paid run could afford** — and results that
model variance cannot confound.

What still requires a real session is the realistic baseline: completion, wall
time, tokens, child contribution. That is Experiment 4, not yet run.

## Runtime Boundary

The limits are **designed**, and were found by the gate rather than read off a
constant first:

| Limit | Value | Source |
| --- | --- | --- |
| Concurrent children | **4** | `DEFAULT_MAX_CONCURRENT_AGENTS` |
| Total children per run | **6** | `DEFAULT_MAX_TOTAL_AGENTS` |

```
Supported:   up to 6 children per run, at most 4 at once
Refused:     every spawn past 6, explicitly, naming the limit
Unknown:     nothing above 6 — it is not reachable, by design
```

The first run of this gate asked for 8 and got 6. That is the cap working, not a
defect — and asking "does 16 work" is the wrong question, because sixteen will
never work on purpose. The question that decides reliability is what happens
*at* the wall.

**It refuses cleanly.** A surplus spawn produces
`ToolResult { is_error: true }` reading *"Sub-agent limit reached (6 max this
run). Do the remaining work directly."* No child is created, so no child can be
stranded, and the model is told what to do instead. A cap enforced by silence
would be the real defect: the parent would wait on children that never existed —
which is exactly the shape of session `446c71ad`.

## Test Matrix

Five repetitions per width; each asserts every child that started also settled,
no finish arrived without a start, and refusals matched the surplus exactly.

| Case | Children asked | Started | Refused | Ghosts | Orphans | Result |
| --- | --- | --- | --- | --- | --- | --- |
| Normal | 1 | 1 | 0 | 0 | 0 | PASS |
| Normal | 2 | 2 | 0 | 0 | 0 | PASS |
| Normal | 4 | 4 | 0 | 0 | 0 | PASS |
| Boundary | 6 | 6 | 0 | 0 | 0 | PASS |
| Over cap | 8 | 6 | 2 | 0 | 0 | PASS |
| Over cap | 16 | 6 | 10 | 0 | 0 | PASS |

30 runs, 0 ghosts, 0 orphans, 0 double-reports.

## Experiment results

### 1 · Fan-out scaling — PASS

Covered above. Below the cap every child starts and settles; above it the
surplus is refused with a reason and nothing is silently dropped.

### 2 · Event pressure — PASS at this layer, and separately in vivo

At the widest reachable fan-out every child produces **exactly one** terminal —
none lost in the crowd, none duplicated.

The pipeline itself was fixed and verified separately: session `0f5a5d60`
completed a three-explorer run at **288 tool calls and 1.74M input tokens**,
against the 230 calls that overflowed the old 256-slot channel. Root cause and
replay: `beta-001/EVENT_PIPELINE_ROOT_CAUSE.md` in the dogfood-control repo.

### 3 · Failure propagation — PARTIAL

| Case | Status |
| --- | --- |
| Child failure | **PASS** — a child that reaches for a tool it does not have still emits a terminal; the parent is not left waiting |
| Parent cancellation | **NOT COVERED** — engine-level; the turn now settles unfinished children before `TurnFinished` (`0a462660`), but no test drives cancellation mid-fan-out |
| Background task failure | **NOT COVERED** |

The parent-cancellation gap is the important one. The fix exists and its unit
tests pass, but "cancel a parent with three children in flight and prove all
three settle" has not been run.

### 4 · Long-running realistic baseline — NOT RUN

Needs real model calls, n≥3. The single verification run so far (`0f5a5d60`)
completed with three explorers in 6m24s, but one run is not a baseline.

## Failure Analysis

No product failure survived. Two things failed during the work and both were
mine:

1. **`Arc::try_unwrap` on a live observer handle** — harness defect, fixed. Not
   a finding.
2. **A SQLITE_BUSY regression introduced by the batching work** — the batch
   transaction opened with `SELECT MAX(sequence)`, taking a read lock the INSERT
   then had to upgrade; a losing upgrade is SQLITE_BUSY with no retry. Six
   `direct_verification` tests failed with "database is locked". Fixed by making
   the transaction write first. Recorded because the error reads as
   environmental and is not.

## Recommendation

**Multi-Agent can enter the Beta capability list as a bounded capability**, and
the bound should be stated rather than discovered:

> Up to 6 delegated agents per run, at most 4 concurrent. Past that the runtime
> refuses the spawn and tells the agent to do the work itself.

That is a defensible product claim: a reliable 4-to-6-agent system, not an
unstable twenty-agent one.

**Before PASS**, two gaps close:

1. Parent cancellation with children in flight — all children must reach a
   terminal state.
2. Background task failure — represented, never silent.

Both are engine-level and deterministic. Neither needs a model, and neither is
large.

## What this does not claim

Thirty scripted runs prove the lifecycle contract holds under exact concurrency.
They do not prove the runtime survives a real model driving real tools for an
hour. That is Experiment 4, and it is still owed.
