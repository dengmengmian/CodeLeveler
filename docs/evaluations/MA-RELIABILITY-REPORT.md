# MA-RELIABILITY — Spawn Reliability Gate

**Opened:** 2026-08-23 · **Closed:** 2026-08-23 · **Subject:** `main` @
`ce5b7d8b` · **Suite:** `crates/leveler-agent/tests/spawn_reliability_gate.rs`
· **Live sessions:** `9c4f6d0c` `705c5b3e` `466934ee`

## Executive Summary

```
Spawn Reliability:  PASS
```

Lifecycle integrity holds at every width tested, past the cap, under
cancellation, and across three real long-running Multi-Agent sessions. **Zero
ghost children in 34 scripted runs and 3 live ones.**

The gate opened as PARTIAL with two gaps — parent cancellation and
background-task failure — and one unrun experiment. All three are now closed.

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

What still required a real session is the realistic baseline — completion,
fan-out, settlement, tokens. That is Experiment 4, and it has now been run at
n=3.

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

30 runs across the widths above, plus a wide-fan-out check, a child-failure case,
a cancellation case and one background-failure case: **34 scripted runs, 0
ghosts, 0 orphans, 0 double-reports.**

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

### 3 · Failure propagation — PASS

| Case | Status |
| --- | --- |
| Child failure | **PASS** — a child that reaches for a tool it does not have still emits a terminal; the parent is not left waiting |
| Parent cancellation | **PASS** — cancelling a parent with children in flight strands none of them |
| Background task failure | **PASS** — a non-zero background exit surfaces as a tool error carrying its exit code |

The cancellation test is written to fail rather than flatter: if the cancel
lands after every child has already reported, nothing about stranding was
tested and a naive test would still go green. So it asserts the run actually
returned `Cancelled` **and** that children had started, and it fires the cancel
from a stream hook rather than a wall-clock timer — racing three children on a
loaded machine is how a test passes locally and fails in CI.

Background failure turned out to be implemented correctly already; what was
missing was anything holding it there. The Multi-Agent shape that makes it
matter is a parent fanning out children while a verification command runs
behind them: if that failure arrives as `ok`, the parent synthesises on top of a
check that never passed.

### 4 · Long-running realistic baseline — PASS (n=3)

Same frozen goal (90 bytes, sha256 `49bd2321…`), same model
(`deepseek-v4-flash`), same repository at `0a462660`, `git reset --hard` between
runs so every run starts identically. Sessions `9c4f6d0c`, `705c5b3e`,
`466934ee`.

| | run 1 | run 2 | run 3 |
| --- | --- | --- | --- |
| Session status | **completed** | **completed** | **completed** |
| Children spawned | 4 | 3 | 4 |
| Children settled | **4/4** | **3/3** | **4/4** |
| **Stuck / ghost** | **0** | **0** | **0** |
| Tool calls | 332 | 163 | 409 |
| Tool failures | 18 | 0 | 4 |
| Input tokens | 2.69 M | 0.90 M | 3.36 M |
| Model requests | 42 | 19 | 46 |

**3/3 completed. 11 children spawned, 11 settled, 0 ghosts.**

Two children across the three runs terminated `INCOMPLETE_PARTIAL (stopped: it
hit the round ceiling)`. That is the **right** outcome, not a failure of the
gate: the child ran out of budget and *said so*. An honest incomplete is what
this whole line of work is for — the defect class was children that stop and
never report, not children that report having stopped.

The load matters. The session that originally overflowed the pipeline made 230
tool calls. These made 332, 163 and 409, with up to 3.36 M input tokens, and all
three completed.

**Wall time is not reported as an agent metric.** Runs took 1060 s, 1205 s and
1020 s, but the clone had no build cache — every `cargo test` the agent issued
was a cold compile of the whole workspace. That number measures the toolchain,
not the runtime. Fan-out, settlement, completion and tokens are unaffected.

**Variance is large and worth saying so.** Tool calls ranged 163–409 and tokens
0.90–3.36 M on an identical starting state: a 2.5× and 3.7× spread across three
runs. Reliability was invariant; *cost* was not. Any future claim about
Multi-Agent efficiency needs far more than n=3.

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

**Multi-Agent enters the Beta capability list as a bounded capability**, and the
bound is stated rather than left to be discovered:

> Up to 6 delegated agents per run, at most 4 concurrent. Past that the runtime
> refuses the spawn and tells the agent to do the work itself.

That is a defensible product claim: a reliable 4-to-6-agent system, not an
unstable twenty-agent one.

## What this does not claim

- **Nothing above 6 children.** It is unreachable by design, so it is untested
  and should stay unclaimed.
- **No efficiency claim.** Identical inputs produced 163–409 tool calls and
  0.90–3.36 M tokens. Reliability was invariant across that spread; cost was
  not, and n=3 cannot resolve it. Whether Multi-Agent is *worth* its cost is the
  next question and a different experiment.
- **One task shape, one model.** Every live run asked the same repository
  question of `deepseek-v4-flash`. Fan-out behaviour is model-dependent — the
  cross-model work already showed `k3` fanning out where DeepSeek issues one —
  so these numbers describe this pairing, not the runtime's whole envelope.
- **No wall-time figure.** See Experiment 4: the clone had no build cache, so
  the clock measured cold compiles.
