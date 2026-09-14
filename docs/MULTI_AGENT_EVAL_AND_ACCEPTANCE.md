# Multi-Agent MA4 — Eval & Acceptance

Status: **MA4_EVAL_ACCEPTANCE=FAIL — value not demonstrated.** Truth and
safety gates are all zero; multi-agent does not regress simple tasks; one
natural delegation produced six useful children. But on this model and task
set, delegation brought no latency or cost benefit, and MA4.6 does not allow a
pass on adoption alone. Decision: **MULTI_AGENT_BLOCKED** (as a claimed
product benefit; the capability stays model-chosen and safe — §8).

## 1. Baseline, treatment, parity

| Arm | Binary | Config |
|---|---|---|
| `baseline` | `v0.2.0-beta.2` = `86bc214` (built clean from the tag) | product default |
| `single` | `78fc372` (MA1–MA3 + eval, clean build) | `agents.delegation = false` |
| `multi` | `78fc372` | product default (delegation available) |

Same machine, same provider gateway, `deepseek/deepseek-v4-flash` with the
user's real model config (`reasoning_effort = "max"`,
`max_parallel_tool_calls = 1`), isolated `LEVELER_HOME` per run, fresh
workspace per run, `leveler run --collaboration goal --auto-approve
--max-rounds <case>`. Arm order rotates every slot; batches ran sequentially
with nothing else calling the provider.

Parity notes: `v0.2.0-beta.2` already ships delegation, so the frozen
baseline is not single-agent; `single` is the single-agent control on the
same build as `multi`. `78fc372` predates two runtime fixes found later
(`06714ea` chat-turn steering, `3544129` cancel-handle ordering); neither is
on the `leveler run` goal path these runs use.

## 2. Tooling (E1–E6)

| Gap (MA0) | Now |
|---|---|
| E1 per-child grouping | `evals/lib/child_lifecycle.py`: children by typed `outcome` and `stop`; untyped (pre-MA1) terminals counted as unknown, never as success |
| E2 safety counters | duplicate settlement, open orphans, lost children, interruptions/resumes, ownership violations (a successful child write outside its admitted or claimed scope, or any write by a read-only child; legacy `[scope: …]` task markers honoured); the run record's `safety.violations` reads them |
| E3 usage | requests, input/output/cached tokens, cost per parent and child lane; one unpriced request makes cost unknown |
| E4 A/B | `evals/scripts/multi_agent_ab.py` (run / rescore / report), `evals/lib/ab.py` |
| E5 task set | `evals/cases/multi_agent_closure/` + `evals/suites/multi_agent/product_closure/` |
| E6 useful delegation | from facts, per child (§5) |

Independent review of the tooling found 2 high / 4 medium issues (baseline
logs misread as violations or failures, a report crash on an errored run,
call-id collisions, recovery kill-point bias, orphan grandchild processes)
and oracle over-specification; all fixed in `c6a3b66`, and every recorded
run was re-scored under the fixed metrics and oracles (task text unchanged;
the first verdict is kept beside the new one). Re-scoring flipped one run:
`baseline / ma-review-ratelimit / 0` had rewritten the test file the task
said not to modify.

## 3. Task set

Batch 1 — seven cases, one per MA4.2 category, 3 reps × 3 arms = 63 runs.
Batch 2 — two scale-up cases declared after batch 1 showed zero adoption
(`58139b2`, before any scale-up run), 2 reps × 3 arms = 12 runs. Task text
never mentions sub-agents. Each case fails its independent `expect` at start
and passes with a reference solution.

| Case | Category |
|---|---|
| `ma-simple-clamp` | SIMPLE (delegation unsuitable) |
| `ma-multifile-timeout` | MULTI_FILE |
| `ma-research-inventory` | PARALLELIZABLE_RESEARCH (4 packages) |
| `ma-parallel-impl` | PARALLELIZABLE_IMPLEMENTATION (4 packages) |
| `ma-review-ratelimit` | REVIEW_HEAVY |
| `ma-long-kvstore` | LONG_GOAL |
| `ma-recovery-parallel` | RECOVERY (SIGKILL once, then `run --resume`) |
| `ma-scale-research` | PARALLELIZABLE_RESEARCH_LARGE (10 packages, 50 files) |
| `ma-scale-impl` | PARALLELIZABLE_IMPLEMENTATION_LARGE (8 packages) |

## 4. Results

### 4.1 Batch 1 (63 runs)

| Metric | baseline | single | multi |
|---|---|---|---|
| runs | 21 | 21 | 21 |
| task success (independent expect) | 19/21 | 21/21 | 21/21 |
| false verified (verified while its own checks fail) | 0 | 0 | 0 |
| incorrect and verified (verified, oracle fails) | 2 | 0 | 0 |
| completed but incorrect | 2 | 0 | 0 |
| delegation adoption | 0 | 0 | 0 |
| children | 0 | 0 | 0 |
| wall mean / median (s) | 58.6 / 40.0 | 73.3 / 41.2 | 72.4 / 51.0 |
| model requests (mean) | 6.6 | 7.6 | 7.2 |
| input / output tokens | 1.92M / 140k | 2.12M / 182k | 2.16M / 180k |
| cached input tokens | 1.54M | 1.74M | 1.77M |
| cost (USD) | 0.305 | 0.345 | 0.350 |

Per category (median wall s · mean requests · cost µUSD):

| Category | baseline | single | multi |
|---|---|---|---|
| SIMPLE | 11.2 · 4.3 · 22,300 | 11.5 · 4.0 · 19,231 | 11.3 · 4.0 · 20,487 |
| MULTI_FILE | 20.7 · 6.0 · 33,529 | 21.7 · 6.0 · 31,950 | 19.8 · 5.3 · 29,790 |
| PARALLELIZABLE_RESEARCH | 20.6 · 4.0 · 25,198 | 28.1 · 7.3 · 40,597 | 16.7 · 3.7 · 22,744 |
| PARALLELIZABLE_IMPLEMENTATION | 40.0 · 5.7 · 34,987 (2/3) | 41.2 · 5.7 · 33,722 | 52.3 · 5.7 · 38,463 |
| REVIEW_HEAVY | 147.0 · 13.0 · 91,862 (2/3) | 155.2 · 12.0 · 78,291 | 167.6 · 12.0 · 84,567 |
| LONG_GOAL | 103.9 · 7.3 · 60,595 | 127.4 · 11.7 · 89,613 | 173.8 · 13.3 · 108,390 |
| RECOVERY (all killed at parent progress; no child existed) | 60.7 · 5.7 · 36,693 | 80.6 · 6.7 · 51,441 | 68.1 · 6.7 · 45,237 |

The two baseline failures are model quality, not runtime truth: one
implementation missed a stated edge (`Format(500ms)` must be `"0s"`), one
rewrote the test file the task said to leave alone; in both the product's own
checks really passed.

### 4.2 Batch 2, scale-up (12 runs)

| Metric | baseline | single | multi |
|---|---|---|---|
| task success | 3/4 | 3/4 | 4/4 |
| false verified | 0 | 0 | 0 |
| incorrect and verified | 1 | 1 | 0 |
| delegation adoption | 0/4 | 0/4 | 1/4 |
| children (success / partial) | — | — | 6 (6 / 0) |
| useful delegation | — | — | 1/1 run, 6/6 children `independent_subtask` |
| wall mean (s) | 76.9 | 76.7 | 71.8 |
| requests mean | 5.8 | 11.3 | 18.3 |
| cost (USD) | 0.059 | 0.111 | 0.156 |

The delegated run (`multi / ma-scale-impl / 0`): the parent read all 16
files at 5 s, planned until 70.6 s, spawned six background default-role
workers (one package each) and wrote the other two packages itself. Each
worker claimed its file through `claim_write_scope` (0 violations), four ran
concurrently (`max_concurrent_agents`), each finished in 15–21 s, all six
settled `completed_with_findings / completed` exactly once, every file they
wrote is in the final tree, and the hidden-test oracle passed. The parent
used `wait_task` on all six ids (answered informationally). Wall 119.9 s;
cost 109,170 µUSD (children 70,271). The same case without delegation took
93.0 s / 17,983 µUSD in the same arm, and 81.6–128.8 s in the other arms.

### 4.3 Totals (75 runs)

| | baseline | single | multi |
|---|---|---|---|
| task success | 22/25 | 24/25 | 25/25 |
| incorrect and verified | 3 | 1 | 0 |
| false verified | 0 | 0 | 0 |
| ownership violation · lost child · duplicate settlement · open orphan | 0 · 0 · 0 · 0 | 0 · 0 · 0 · 0 | 0 · 0 · 0 · 0 |
| delegation adoption | 0/25 | 0/25 | 1/25 |

## 5. Useful delegation

Counted per child from facts, never from a spawn:

| Label | Rule |
|---|---|
| `independent_subtask` | a writing child ended `completed`, and a path it wrote is in the final change of a run that passed `expect` |
| `evidence_not_redone` | a read-only child ended `completed_with_findings`, and the parent re-read fewer of its distinct files than it read |
| `independent_review` | a harness reviewer ended `completed` (not model delegation) |

A run with a non-reviewer child and no useful label, or any spawn on
SIMPLE, is unnecessary delegation. Observed: 1 delegating run, 6/6 children
`independent_subtask`, unnecessary delegation 0, no spawn on SIMPLE in any
arm.

## 6. Real dogfood (MA4.7)

Nine real runs at `78fc372`, `deepseek-v4-flash`, fresh `navsvc` fixture per
run. These tasks name `spawn_agent` on purpose: they check what the runtime
does once children exist, not adoption (S1 is the no-instruction control).
Counters from the event log; ownership from `child_lifecycle`.

| Scenario | Children (outcome / stop) | Result |
|---|---|---|
| S1 simple, no instruction | 0 | answered directly |
| S2 background explorer | 1 with_findings / completed | `SINKS.md` written from the settled report |
| S3 foreground dependency | 1 with_findings / completed | parent's edit landed; checks passed |
| S4 parallel explorers | 2 with_findings / completed | `PACKAGES.md` from both |
| S5 parallel disjoint workers | 2 with_findings / completed | both `NOTES.md`, 2 child writes in scope |
| S6 child stopped by its round cap | 1 incomplete_partial / budget | parent reported the gap |
| S7 harness reviewer | 1 reviewer with_findings / completed | change landed; checks passed |
| S8 overlapping workers | 1 admitted, 1 spawn refused (overlap) | only the admitted worker wrote |
| S9 restart: background worker, `SIGKILL` after 2 child tool results, `run --resume` | 1 interrupted → resumed → with_findings / completed | same child id continued and settled once; its file written |

Totals: duplicate settlement 0, open orphans 0, lost children 0, ownership
violations 0 (4 child writes, all in scope), unattributed child writes 0.
Cancel of one running child was accepted end to end from a phone in MA5 §4.

## 7. Failures and residuals

- **Value.** Natural adoption is 1/25 runs; the only delegation cost 6× the
  non-delegated run of the same case and was not faster: the parent's own
  reasoning before spawning (65 s at `reasoning_effort = max`) dominated,
  while the children took 15–21 s each. One run is not a rate.
- RECOVERY in the A/B never had a child to kill (the model did not delegate);
  child restart is covered by dogfood S9 and MA1 §9.2.
- `single` arm incorrect-and-verified 1 (scale-impl rep 0): model quality,
  the same class as the baseline failures.
- Eval matrix is small (2–3 reps per cell); per-category differences in wall
  are within the spread of the reps and are not claimed.

## 8. Decision

MA4.6 asks for a clear benefit on parallelizable tasks without paying for
it in success, and forbids passing on adoption when cost rises and latency
does not improve. The measurements:

| MA4.6 question | Answer |
|---|---|
| success held? | yes: multi 25/25 vs single 24/25 vs baseline 22/25 |
| simple tasks without needless overhead? | yes: SIMPLE 11.3 s vs 11.5 s, no spawn |
| clear benefit on parallelizable tasks? | **no**: adoption 0/15 on the parallelizable categories of batch 1, 1/4 at scale; the delegated run was slower than or equal to non-delegated runs and cost 6× |

```text
MULTI_AGENT_BLOCKED
```

Meaning, precisely: the evidence does not support claiming multi-agent as a
product benefit on `deepseek-v4-flash` for this task set. It does not mean
the capability is unsafe or should be removed — every truth and ownership
counter is zero, delegation is model-chosen and rare, and when it happened it
produced correct, retained work.

## 9. Gates

```text
MULTI_AGENT_TASK_SUCCESS=ACCEPTABLE
FALSE_VERIFIED_TOTAL=0
INCORRECT_AND_VERIFIED=0 (multi arm; single 1, baseline 3 — model quality)
OWNERSHIP_VIOLATION=0
LOST_CHILD=0
DUPLICATE_SETTLEMENT=0
ORPHAN_OPEN_EDGE=0
USEFUL_DELEGATION_PROVEN=YES (qualitative: 1 natural run, 6/6 children; no rate)
SINGLE_AGENT_SIMPLE_TASK_REGRESSION=NO
PARALLELIZABLE_BENEFIT=NOT_DEMONSTRATED

MULTI_AGENT_ACCEPTANCE=FAIL
MA4_EVAL_ACCEPTANCE=FAIL
```

Data: runs, sessions and summaries live in the local scratch directory of
this session (`ma4/ab`, `ma4/ab-scale`, `ma4-dogfood`), not in the repository;
the summaries quoted above are reproduced by
`multi_agent_ab.py report --out <dir>`.
