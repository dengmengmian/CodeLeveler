# MA-VALUE-REVIEWER — Pilot halted, formal run not started

**Status:** pilot executed · infrastructure FAIL · **not expanded to n ≥ 6**
**Subject:** `v0.2.0-beta.1` (`08f5d721`-dirty) · **Run:** 2026-08-24

The pilot ran both arms on real model calls. It did **not** produce a Reviewer
verdict, and it was never going to: three measurement defects and one
case-selection defect each independently blocked the primary question. Per the
protocol's step 4 (*"If pilot infrastructure fails: stop and report"*), the
formal run was not launched.

Three of the four are now fixed and re-verified against real runs. The fourth —
a saturated control arm — needs new eval cases and still blocks.

**This document does not say Reviewer has no value. It says the experiment as
specified cannot measure it.**

## What ran

| | Control | Treatment |
| --- | --- | --- |
| `--mode` | `self` | `reviewer` |
| `agents.independent_review` | `"off"` | `"always"` |
| Tasks | 5 vendored `EvaluationCase` | same 5 |
| Runs | 1 per task | 1 per task |
| Model | `deepseek/deepseek-v4-flash` | same |
| Binary | `target/release/leveler` `08f5d721-dirty` | same |

Config diff between arms was exactly one line. Task set, model, scoring,
reviewer permissions, and trigger mode stayed frozen. No product behavior was
changed at any point.

## Result

| Metric | Control (`self`) | Treatment (`reviewer`) |
| --- | ---: | ---: |
| Independent `expect` passed | 5/5 | 5/5 |
| Reviewer spawned | 0/5 | **5/5** |
| Findings lifecycle observable | n/a | **0/5** → **5/5** after the fix |
| Turns (mean) | 11.0 | 7.0 |
| Tokens (mean) | 301,183 | 172,486 |
| Wall time (mean) | 40.3 s | 62.1 s |

Verdict: `insufficient_n`. Not a product finding.

## Four blockers

### 1. Observer never joined the independent verifier — FIXED

`leveler eval run --json-out` had already executed each case's `expect` and
recorded `expect_passed` per case. `eval/lib/runner.py:score_home()` read only
the session DBs and never opened `eval_result.json`, so every run scored
`verifier.ran = false`, `task_success = null`. The quality signal — the
experiment's primary estimand — was discarded after being computed.

Fix: `load_expect_verdicts()` joins the verdict by case id; `score_home()`
takes `verdicts=` and passes `verifier_ran` / `verifier_passed` /
`verifier_command` into the run record. A verifier that did not run stays
`passed=None` (unscored), never `False`. Locked by
`eval/tests/test_expect_join.py`.

### 2. `contribution: null` was read as zero findings — FIXED

The first reviewer-arm report claimed **"zero-finding reviewers: 5"**. That was
fabricated. All five reviewers finished with
`status: COMPLETED_WITH_FINDINGS`, and `ts-redact-secrets` recorded
`Structured findings adopted: f-1 — judge each with resolve_finding`.

The runtime was honest. `crates/leveler-engine/src/turn.rs:753` emits
`contribution: None` deliberately:

> The review stage runs outside the executor's ledger, so no projection is
> available here. `None` says "not measured", which is the truth; a zeroed
> projection would read as "contributed nothing".

`eval/lib/reviewer.py` then did exactly what that comment warns against —
`finished.get("contribution") or {}` collapsed "not measured" into `0`.

Fix: an unmeasured projection now yields `contribution_unmeasured: True` with
`findings_* = None`, and is excluded from `zero_findings` and `noise`. The
report prints an explicit "lifecycle not observable" block instead of a number.
Locked by `eval/tests/test_contribution_unmeasured.py`.

### 3. The lifecycle had no data source on the treatment arm — FIXED 2026-08-24

This was the blocker that stopped the formal run.

The protocol scores Reviewer contribution from
`sub_agent_started role=reviewer` joined to `sub_agent_finished.contribution`.
That join holds for executor-spawned children. It does **not** hold for
`agents.independent_review`, which runs as a separate review stage outside the
executor ledger — the code path that emits `contribution: None` by design.

So on the `always` arm:

```
Created → Acknowledged → Accepted → Addressed → Verified
   ?            ?            ?           ?          ?
```

Every stage was unobservable. `useful_findings`, `verified_findings`, and the
`noise` check — three of the four `REVIEWER_SUCCESS_METRICS` — had no source.
Running n ≥ 6 would have changed the sample size, not the availability of the
measurement.

**Resolved.** The diagnosis above was half wrong: the review stage *does* adopt
its findings into the parent ledger and persists the snapshot. The projection
was unavailable only because `ledger` was scoped inside the
`if !result.findings.is_empty()` branch and could not be read at the finish
event one block later. The comment describing the path as ledgerless described
the scope, not the architecture.

`turn.rs` now holds the ledger across the finish event and always emits a
projection: a measured zero when nothing was reported, counts when something
was. `ContributionSource` stamps which mechanism produced it, so an
independent-review contribution is distinguishable from an executor child with
the same role.

Re-run on the same five cases with the fixed binary:

```
contribution UNMEASURED (runtime emitted null): 0/5      (was 5/5)
zero-finding reviewers: 4        ← measured zeros, not fabricated
noise (unjudged findings): 1     ← one finding the parent never judged
```

That last line is a fact about the product that no report could previously
state. n=1; it is an observation, not a result.

### 4. The control arm is saturated — NOT FIXED, still blocking

Control passed **5/5** with `quality_score_100 = 100` and
`failed_case_ids = []`.

A control at ceiling makes "Reviewer raises final correctness" (H2, the primary
estimand) unmeasurable in principle. `higher success rate` — one of the two
ways the protocol allows a PASS — is arithmetically unreachable. The other way,
`useful findings`, was blocked by defect 3 and is now available — but a
saturated control still leaves the primary estimand unmeasurable.

These five cases do not pressure `deepseek-v4-flash`, and neither does anything
else in `evals/`: every recorded baseline for this model is at ceiling
(`icg-integrated` 15/15, `c4-self-healing` 24/24, `c2-bv2-navigation` 24/24,
`c3-edit-reliability` 24/24). This needs new cases, not a different subset.

## Cost, and why it is not a conclusion

Treatment used fewer turns and tokens but more wall time. At n=1 per cell with
19/22-round outliers on the control arm's two `icg` cases, this is sampling
noise. Do not read a cost signal out of it.

## Decision

**Do not run MA-VALUE-REVIEWER formal.** One blocker remains:

1. ~~Make the lifecycle observable on the independent-review path.~~
   **Done 2026-08-24.** See defect 3 above.
2. **Replace the case set with tasks the control arm actually fails.** Target
   a control pass rate with headroom (~50–70 %), not 100 %. Without headroom
   there is no room for the treatment to improve anything. Every recorded
   baseline for this model is at ceiling, so this needs new cases, not a
   different subset — see
   [MA-VALUE-REVIEWER-TASKS](MA-VALUE-REVIEWER-TASKS.md).

Neither is a Reviewer change. The freeze on reviewer prompts, tools,
permissions, and trigger mode stays in force.

## What this pilot did establish

- Reviewer spawns reliably under `always`: 5/5, correct `role=reviewer`.
- Reviewer stays structurally read-only; no ownership grants, no violations.
- Arm isolation works: one-line config delta, `~/.leveler` untouched.
- The independent `expect` runs and now reaches the run record.
- Adding a reviewer did not drop task success (5/5 → 5/5) — weak evidence, at
  ceiling.
- After the Phase 1 fix, the contribution half is verified end to end: 5/5
  reviewers report a projection carrying profile, capabilities and source.
- One reviewer finding was adopted and never judged by the parent — the
  protocol's definition of noise, and a fact the product could not previously
  report.

## Changes made

No reviewer behavior, no prompts, no permissions, no trigger policy, no spawn
runtime. The runtime change is observability: a projection that was computed
and then dropped is now emitted.

| File | Change |
| --- | --- |
| `eval/lib/runner.py` | `load_expect_verdicts()`; `score_home(verdicts=)` |
| `eval/runner/run.py` | join verdicts in `run_reviewer_value` |
| `eval/lib/reviewer.py` | `contribution: null` → unmeasured, not zero |
| `eval/lib/report.py` | report unmeasured count + explicit warning block |
| `eval/tests/test_expect_join.py` | new, 5 tests |
| `eval/tests/test_contribution_unmeasured.py` | new, 6 tests |

Runtime changes (Phase 1, after this report was first written):

| File | Change |
| --- | --- |
| `crates/leveler-lifecycle/src/findings.rs` | `ContributionSource`; `with_source` |
| `crates/leveler-engine/src/turn.rs` | hold the ledger; always emit a projection |
| `crates/leveler-agent/src/executor/drive.rs` | stamp `ExecutorChild` source |
| `crates/leveler-engine/tests/direct_test.rs` | 2 tests locking the trace |

`python3 -m unittest discover -s eval/tests` → 114 passed.

## Artifacts

- `eval/reports/multi_agent/MA-VALUE-REVIEWER-PILOT/{self,reviewer}/report.md`
- `eval/reports/multi_agent/MA-VALUE-REVIEWER-PILOT/compare.json`
- `eval/runs/MA-VALUE-REVIEWER-PILOT-self-20260824T102416Z-b290b1/`
- `eval/runs/MA-VALUE-REVIEWER-PILOT-reviewer-20260824T103018Z-2f2a76/`

Both arms were rescored from the existing EventLogs after the observer fixes.
No additional model runs were spent.
