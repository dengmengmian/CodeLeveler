# Multi-Agent MA4-C — Parent Reasoning Budget Validation

Status: **MA4C=INCONCLUSIVE_DUE_TO_ROUND_LIMIT.** Lowering only the parent's
reasoning effort (`max` → `high`) did not make the parent delegate earlier on
the workloads that ran to completion (S2, S3). On the largest workload (S4) it
did make the first spawn earlier, but the lower-effort parent then used up the
product's 100-round turn ceiling in 3 of 4 runs (`max`: 0 of 4). The one bucket
where the variable changed the timing is therefore censored, and the value
question cannot be answered there. Every runtime truth and ownership counter
is zero.

## 1. Evidence chain

| Step | Document | Result |
|---|---|---|
| MA4 | `MULTI_AGENT_EVAL_AND_ACCEPTANCE.md` | value FAIL: units too small, one natural delegation, slower and 6× the cost |
| MA4-B | `MULTI_AGENT_WORKLOAD_THRESHOLD_VALIDATION.md` | value FAIL at every size; parallel time saved (median 1070 s) > coordination overhead (median 123 s); first spawn 59–755 s; S4 truncated by the 100-round ceiling in 4 of 7 runs |
| MA4-C | this document | INCONCLUSIVE_DUE_TO_ROUND_LIMIT |

MA4-B §17 offered this experiment because the parent's time before the first
spawn varied from 1 to 12.5 minutes. Note, though, what MA4-B had already
measured: median first spawn was 137 s and median parent planning 55 s,
against S2–S4 walls of 400–2500 s. Even an instant spawn could have removed
only about a tenth of the wall.

## 2. Hypothesis

`PARENT_REASONING_BUDGET_TOO_HIGH`: at `max` effort the parent thinks for so
long before delegating that it misses the window where parallel children
would pay.

## 3. Variable lock

```text
MODEL=deepseek-v4-flash (deepseek/deepseek-v4-flash, the MA4-B canonical id)
PROVIDER=deepseek   GATEWAY=taotoken (the user's ~/.leveler/config.toml, byte-identical to MA4-B's copied config)
SUPPORTED_EFFORTS=[low, high, max]   (config, not assumed)
EFFORT_R0=max (parent)   EFFORT_R1=high (parent)
CHILD_REASONING_EFFORT=max in both arms
MODEL_CHANGED=NO  TASK_SET_CHANGED=NO  PROMPT_CHANGED=NO  ROUND_LIMIT_CHANGED=NO
TIMEOUT_CHANGED=NO  CHILD_REASONING_CHANGED=NO  ONLY_PARENT_REASONING_CHANGED=YES
```

R0 and R1 ran **the same binary** (`52f5f3c`); the only difference between the
arms is the environment variable `LEVELER_EVAL_PARENT_REASONING_EFFORT=high`.

## 4. Wiring (product code, eval-only seam)

The product could not set the parent's effort separately: `leveler run` had
no override input, and `ExecutionOverrides.reasoning_effort` (used by
`leveler eval ablate`) applies to every seat, children included.

| Commit | Change | Test (written first, seen red) |
|---|---|---|
| `934504a` | `ExecutionOverrides.main_reasoning_effort`: applies to `ExecutionRole::Main` only | `main_reasoning_effort_lowers_only_the_top_level_seat` (policy); `main_reasoning_override_reaches_parent_requests_but_not_child_requests` (engine, request level) |
| `934504a` | `leveler run` reads `LEVELER_EVAL_PARENT_REASONING_EFFORT`; unset = no overrides; unknown level = error | `run_cmds::parent_reasoning_tests` (3) |
| `934504a` | `model round started` trace carries `reasoning_effort` | verified per run, §5 |
| `52f5f3c` | harness: `name=bin:parent=<effort>` arms, per-lane effort evidence, time to first read, parent requests/tokens before the first spawn, child brief length, round-limit hit | `test_coordination`, `test_multi_agent_ab` (+6) |

Workspace tests 0 failures, clippy clean; main CI `34860110458` attempt 1
success.

## 5. Effort proof (per run)

Each run traces the effort of every model request and joins it to the durable
`model_requests` row by request id, which gives the lane (parent or child).

```text
traced requests, pilot + formal
R1 parent: High ×507, nothing else   R1 child: Max ×741, nothing else
R0 parent: Max ×434, nothing else    R0 child: Max ×649, nothing else
unmatched: Max ×4 (R1 1, R0 3)
```

Four runs show one `unmatched` request, always `Max`: a child request that got
its first byte and was cancelled when the run ended (no finish line, no
durable row). In R1 the parent never sent `Max`, so it is a child request.

The provider honours the difference, but only in the tail. Parent output
tokens per request (reasoning included):

| Arm | median | p75 | p95 | max |
|---|---|---|---|---|
| max | 154 | 981 | 11 531 | 107 375 |
| high | 154 | 564 | 6 431 | 71 344 |

`high` still produces single pre-spawn requests of 30–39k output tokens and
200–250 s (S2 rep 2, S3 reps 0 and 2, S3 pilot).

## 6. Runs

```text
R0_REUSED_RUNS=0   R0_NEW_RUNS=12   R1_NEW_RUNS=12   TOTAL=24
(+ 2 S0 smoke runs used only to validate the effort evidence; S0/S1 controls not re-run)
```

MA4-B runs were **not reused**: parity fails mechanically (build `640c77e` vs
`52f5f3c`, trace env added) and they ran in other hours; the rule from the
2026-09-07 provider drift is that control and treatment run interleaved. Each
slot here ran its R0 and R1 at the same time (`--parallel 2`), one driver per
case, six runs at once.

Pilot: S2/S3/S4 × R0/R1 × 1. It confirmed the effort split, complete
metrics and working fixtures, so the formal batch (3 reps each) started
without changes. Formal (registered, primary) and pilot + formal are both
reported. No invalid attempts.

## 7. Results — formal batch (n = 3 per cell)

| Bucket | Effort | Success | Units | First spawn (median; values s) | Wall median s | Correctly decomposed / delegated | Input / output tokens | Cost | Round limit hits |
|---|---|---|---|---|---|---|---|---|---|
| S2 | max | 2/3 | 8/9 | 118 (41, 195) | 565 | 1/2 | 3.85M / 0.36M | $0.63 | 0/3 |
| S2 | high | 2/3 | 8/9 | 135 (60, 210) | 485 | 1/2 | 3.13M / 0.33M | $0.53 | 0/3 |
| S3 | max | 1/3 | 10/12 | 293 (38, 548) | 1627 | 0/2 | 11.37M / 0.87M | $1.82 | 0/3 |
| S3 | high | 1/3 | 10/12 | 209 (47, 209, 247) | 1154 | 1/3 | 8.57M / 0.95M | $1.45 | 0/3 |
| S4 | max | 0/3 | 20/24 | 285 (124, 445) | 1974 | 0/2 | 18.11M / 1.51M | $2.94 | 0/3 |
| S4 | high | 0/3 | 15/24 | 109 (95, 122) | 1140 (censored) | 0/2 | 26.92M / 1.29M | $4.10 | **2/3** |

Pilot + formal (n = 4 per cell):

| Bucket | Effort | Success | Units | First spawn (median; values s) | Wall median s | Correctly decomposed / delegated | Cost | Round limit hits |
|---|---|---|---|---|---|---|---|---|
| S2 | max | 2/4 | 10/12 | 118 (41, 195) | 616 | 1/2 | $0.86 | 0/4 |
| S2 | high | 3/4 | 11/12 | 60 (28, 60, 210) | 449 | 2/3 | $0.83 | 0/4 |
| S3 | max | 1/4 | 13/16 | 47 (38, 47, 548) | 1238 | 0/3 | $2.47 | 0/4 |
| S3 | high | 2/4 | 14/16 | 228 (47, 209, 247, 260) | 1140 | 2/4 | $1.91 | 0/4 |
| S4 | max | 0/4 | 25/32 | 445 (124, 445, 552) | 2015 | 0/3 | $3.76 | 0/4 |
| S4 | high | 0/4 | 20/32 | 95 (76, 95, 122) | 1122 (censored) | 0/3 | $5.71 | **3/4** |

### Deltas (formal, high − max)

```text
DELTA_TIME_TO_FIRST_SPAWN (median): S2 +18 s, S3 −84 s, S4 −176 s; pooled S2–S4 160 → 122 s
  uncensored buckets S2+S3 pooled: 118 → 209 s (pilot + formal: 47 → 209 s)
DELTA_WALL_CLOCK (median): S2 −80 s, S3 −473 s, S4 −834 s (S4 high censored); not attributed — see §8
DELTA_TASK_SUCCESS: 3/9 → 3/9 (pilot + formal 3/12 → 5/12); units 38/45 → 33/45, all of the loss in S4
DELTA_TOKEN_USAGE: input +16 %, output −6 %, cost $5.39 → $6.08 (+13 %); S2 −16 %, S3 −20 %, S4 +39 %
TIME_TO_FIRST_READ: ~3 s in every run, both arms (the parent starts reading at once)
```

## 8. Reading the results

**S2 and S3 (no censoring in either arm).** First spawn did not move
earlier: across both buckets the `high` parent spawned no sooner than the
`max` parent, and in 3 of its 5 formal delegated runs it spent 200–250 s and
30k output tokens before spawning. The long pre-spawn burst that MA4-B
attributed to the budget happens at `high` too. The wall medians favour
`high`, but with first spawn unchanged and the same success counts, the gap
is within the run-to-run spread MA4-B measured (S3 G3 794–2274 s). It is not
evidence of a delegation benefit.

**S4.** First spawn did move earlier (median 445 → 95 s, pilot + formal), and
parent integration shrank. But the lower-effort parent takes more and shorter
rounds (median 100 parent requests against 59), and 3 of 4 `high` runs ended
`budget_limited` at the 100-round ceiling with the work unfinished. Their
lower wall is the ceiling, not speed. The only uncensored `high` S4 run did not
delegate and passed 2/8 units. With one uncensored run the orchestration
result at S4 cannot be separated from the round ceiling.

```text
ORCHESTRATION_RESULT: S2/S3 no earlier delegation; S4 earlier delegation
ROUND_BUDGET_CENSORING: S4 high 3/4 truncated, S4 max 0/4
CENSORING_AFFECTED_DECISION=YES (the only bucket where the variable moved first spawn)
```

## 9. Delegation and child briefs

Rule (per delegated run, from children's retained writes):
`CORRECTLY_DECOMPOSED` = the registered useful delegation (child work kept in
a run that passed the oracle); `PARTIALLY_USEFUL` = the run failed the oracle
but at least one child-owned unit passed its hidden suite; `HARMFUL` = no
child-owned unit passed; `UNNECESSARY` = delegation in S0/S1.

```text
                     max (formal)  high (formal)   max (pilot+formal)  high (pilot+formal)
DELEGATION_COUNT          6/9           7/9               8/12               10/12
CORRECTLY_DECOMPOSED        1             2                  1                  4
PARTIALLY_USEFUL            5             5                  7                  6
HARMFUL                     0             0                  0                  0
UNNECESSARY                 0             0                  0                  0
CHILDREN                   26            30                 36                 43
CHILD_SUCCESS_RATE       0.88          0.90               0.92               0.93
CHILD_BRIEF_CHARS (median) 4744         4424               4278               4510
CHILD_REWORK_REQUIRED (children the parent rewrote after settlement)  2   4   2   4
```

Briefs did not get shorter or worse at `high` (S4 median 4557 → 3869 chars
formal, 3986 → 4025 pilot + formal). No S4 delegated run in either arm was
correctly decomposed (0/6), so child handoff quality at S4 remains open; this
experiment cannot isolate it from the round ceiling.

## 10. Safety

```text
FALSE_VERIFIED_TOTAL=0
OWNERSHIP_VIOLATION=0
LOST_ACCEPTED_CHILD=0
DUPLICATE_SETTLEMENT=0
OPEN_ORPHAN=0
CHILD_WRITE_AFTER_TERMINAL=0
RECOVERY_DUPLICATION=0
INCORRECT_AND_VERIFIED (quality metric): max 6/9, high 4/9 (pilot + formal 9/12, 4/12)
```

## 11. Decision

```text
ROOT_CAUSE_CLASSIFICATION=E. INCONCLUSIVE_DUE_TO_ROUND_LIMIT
MA4C_PARENT_REASONING=INCONCLUSIVE_DUE_TO_ROUND_LIMIT
PARENT_REASONING_BUDGET_ROOT_CAUSE=NOT_PROVEN (S2/S3 first spawn not earlier at high; S4 censored)
CHILD_CONTEXT_HANDOFF_PROBLEM=NOT_ISOLATED (S4 correctly decomposed 0/6 in both arms)
ROUND_BUDGET_PROBLEM=CONFIRMED (lower parent effort → 3/4 S4 runs budget_limited)
MA4_EVAL_ACCEPTANCE=FAIL (unchanged)
MULTI_AGENT_PRODUCT_VALUE=NOT_PROVEN
MULTI_AGENT_PRODUCT_CLOSURE=BLOCKED
```

## 12. Product policy

No change. The seam is eval-only and off by default.

```text
SINGLE_AGENT_REASONING_POLICY=unchanged (model default, max for deepseek-v4-flash in the user's config)
MULTI_AGENT_PARENT_REASONING_POLICY=unchanged (inherits the model default); do not cap it — at S4 a lower parent effort runs into the 100-round ceiling
CHILD_REASONING_POLICY=unchanged
FINAL_VALUE_RECHECK=NOT_EXECUTED (only runs after a PASS)
```

## 13. Residuals (mechanical)

- S0/S1 controls were not re-run; two S0 smoke runs (one per arm) only
  validated the effort evidence (both passed, no delegation).
- Six runs at once on one machine (16 cores); the two arms of a slot always
  shared the load.
- Delegation adoption is model-chosen, so first-spawn medians rest on 2–4
  delegated runs per cell.
- `parent_planning_s` (the harness's MA4-B metric) is the latency of the last
  parent request before the spawn; the pre-spawn wait it misses is now
  reported as `parent_pre_spawn.model_wait_s`.
- Run data lives in the session scratchpad (`ma4c/{smoke,pilot,formal}`);
  §14 carries every run.

## 14. Every run

| Batch | Case | Effort | Rep | Oracle | Units | Outcome | Wall s | First read s | First spawn s | Pre-spawn parent req / out tok | Parent req | Children | Delegation quality | Round limit | Effort evidence (lane: level×requests) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| pilot | S2 | max | 0 | fail | 2/3 | completed | 836 | 3 | — | — | 31 | 0 | — | no | parent: Max×31 |
| pilot | S2 | high | 0 | pass | 3/3 | completed | 342 | 3 | 28 | 3 / 4133 | 16 | 3 | CORRECTLY_DECOMPOSED | no | parent: High×16; child: Max×61 |
| pilot | S3 | max | 0 | fail | 3/4 | completed | 849 | 2 | 47 | 4 / 6389 | 12 | 4 | PARTIALLY_USEFUL | no | parent: Max×12; child: Max×123 |
| pilot | S3 | high | 0 | pass | 4/4 | completed | 1127 | 3 | 260 | 4 / 39145 | 23 | 4 | CORRECTLY_DECOMPOSED | no | parent: High×23; child: Max×66 |
| pilot | S4 | max | 0 | fail | 5/8 | completed | 2055 | 5 | 552 | 4 / 80801 | 33 | 6 | PARTIALLY_USEFUL | no | parent: Max×33; child: Max×98 |
| pilot | S4 | high | 0 | fail | 5/8 | budget_limited | 940 | 2 | 76 | 6 / 11586 | 100 | 6 | PARTIALLY_USEFUL | YES | parent: High×100; child: Max×92 |
| formal | S2 | max | 0 | pass | 3/3 | completed | 565 | 3 | 41 | 4 / 5216 | 14 | 3 | CORRECTLY_DECOMPOSED | no | parent: Max×14; child: Max×22 |
| formal | S2 | max | 1 | fail | 2/3 | completed | 454 | 3 | 195 | 2 / 28113 | 12 | 3 | PARTIALLY_USEFUL | no | parent: Max×12; child: Max×32 |
| formal | S2 | max | 2 | pass | 3/3 | completed | 667 | 3 | — | — | 53 | 0 | — | no | parent: Max×53 |
| formal | S2 | high | 0 | pass | 3/3 | completed | 413 | 2 | 60 | 3 / 8237 | 17 | 3 | CORRECTLY_DECOMPOSED | no | parent: High×17; child: Max×29 |
| formal | S2 | high | 1 | pass | 3/3 | completed | 645 | 3 | — | — | 10 | 0 | — | no | parent: High×10 |
| formal | S2 | high | 2 | fail | 2/3 | completed | 485 | 2 | 210 | 3 / 30711 | 11 | 3 | PARTIALLY_USEFUL | no | parent: High×11; child: Max×68 |
| formal | S3 | max | 0 | fail | 3/4 | completed | 506 | 2 | 38 | 3 / 5581 | 14 | 4 | PARTIALLY_USEFUL | no | parent: Max×14; child: Max×45 |
| formal | S3 | max | 1 | pass | 4/4 | completed | 1627 | 3 | — | — | 58 | 0 | — | no | parent: Max×58 |
| formal | S3 | max | 2 | fail | 3/4 | completed | 1781 | 2 | 548 | 3 / 5449 | 14 | 4 | PARTIALLY_USEFUL | no | parent: Max×15; child: Max×145; unmatched: Max×1 |
| formal | S3 | high | 0 | fail | 3/4 | completed | 883 | 2 | 209 | 5 / 29948 | 24 | 4 | PARTIALLY_USEFUL | no | parent: High×24; child: Max×69 |
| formal | S3 | high | 1 | pass | 4/4 | completed | 1154 | 3 | 47 | 3 / 7179 | 17 | 4 | CORRECTLY_DECOMPOSED | no | parent: High×17; child: Max×92 |
| formal | S3 | high | 2 | fail | 3/4 | completed | 1191 | 2 | 247 | 6 / 33255 | 35 | 4 | PARTIALLY_USEFUL | no | parent: High×35; child: Max×43 |
| formal | S4 | max | 0 | fail | 7/8 | completed | 1910 | 4 | 124 | 6 / 17364 | 53 | 6 | PARTIALLY_USEFUL | no | parent: Max×53; child: Max×92; unmatched: Max×1 |
| formal | S4 | max | 1 | fail | 6/8 | completed | 1974 | 5 | 445 | 6 / 59916 | 74 | 6 | PARTIALLY_USEFUL | no | parent: Max×74; child: Max×92; unmatched: Max×1 |
| formal | S4 | max | 2 | fail | 7/8 | completed | 2792 | 5 | — | — | 65 | 0 | — | no | parent: Max×65 |
| formal | S4 | high | 0 | fail | 6/8 | budget_limited | 1778 | 3 | 122 | 8 / 17153 | 100 | 6 | PARTIALLY_USEFUL | YES | parent: High×100; child: Max×109; unmatched: Max×1 |
| formal | S4 | high | 1 | fail | 2/8 | completed | 1104 | 5 | — | — | 54 | 0 | — | no | parent: High×54 |
| formal | S4 | high | 2 | fail | 7/8 | budget_limited | 1140 | 3 | 95 | 10 / 10826 | 100 | 6 | PARTIALLY_USEFUL | YES | parent: High×100; child: Max×112 |

## 15. Next

Stop. One experiment, for the user to choose to run: **Long-Task Round
Budget Closure** — the 100-round single-turn ceiling truncated S4 in MA4-B
(4/7) and, at lower parent effort, here (3/4). Until a large task can finish
under the ceiling, no parent or child change can be judged at S4.
