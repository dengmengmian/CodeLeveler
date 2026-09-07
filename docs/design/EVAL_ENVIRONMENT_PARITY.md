# Eval environment parity — what the wall-clock numbers were actually measuring

Provider latency roughly doubled between 2026-09-06 and 2026-09-07. Every
control run in the FAST/GOOD/CHEAP work was recorded before that; every
treatment run after it. The wall-clock and timeout differences those
comparisons showed are therefore not attributable to any treatment.

This document records the drift, re-scores the two affected experiments, and
sets the parity rules the next one has to follow.

## The measurement

Median latency over MAIN-TASK model requests only — reviewer, child and
advisory calls carry different prompt sizes and would blur it. Percentiles, not
means: a handful of 60–120s requests dominates a mean and hides the shift that
matters.

| run | started | reqs | p25 | **median** | p75 | p95 | model wait | timed out | mech |
|---|---|---:|---:|---:|---:|---:|---:|---|---|
| control formal C3 | 09-05 14:28 | 159 | 1.8 | **2.3** | 4.4 | 85.3 | 88.5% | no | PASS |
| control post-c C1 | 09-06 13:35 | 110 | 1.8 | **2.2** | 4.6 | 31.7 | 54.4% | no | PASS |
| control post-c C2 | 09-06 14:13 | 106 | 1.8 | **2.2** | 4.4 | 76.2 | 87.5% | no | PASS |
| control post-c C3 | 09-06 14:47 | 190 | 1.9 | **2.1** | 2.9 | 60.0 | 74.2% | no | PASS |
| control pre-D C3 | 09-07 07:03 | 127 | 3.1 | **4.8** | 13.4 | 56.3 | 50.4% | no | PASS |
| batching C1 r1 | 09-07 11:57 | 39 | 3.1 | **4.9** | 26.8 | 52.9 | 82.1% | no | PASS |
| batching C2 r1 | 09-07 12:28 | 88 | 3.1 | **4.1** | 12.9 | 83.0 | 92.3% | no | PASS |
| batching C3 r2 | 09-07 10:30 | 127 | 4.0 | **6.4** | 15.9 | 126.5 | 75.1% | **yes** | PASS |
| batching C3 r3 | 09-07 11:36 | 53 | 3.8 | **5.3** | 11.2 | 85.1 | 87.0% | no | PASS |
| reoffer C3 r1 | 09-07 15:47 | 145 | 3.0 | **4.3** | 11.1 | 124.8 | 84.2% | **yes** | PASS |
| reoffer C3 r2 | 09-07 16:50 | 157 | 3.1 | **4.4** | 21.5 | 96.6 | 95.1% | **yes** | FAIL |
| reoffer C3 r3 | 09-07 17:52 | 158 | 2.8 | **4.2** | 13.7 | 103.3 | 93.5% | **yes** | FAIL |

Median roughly doubled (2.1–2.3s → 4.1–6.4s). p75 rose 3–6× (2.9–4.6s →
11–27s). p95 rose from 32–85s to 53–127s.

**The drift is not a treatment effect.** `control pre-D C3` carries no
treatment at all — it is the untouched `47c2d7daaa93` binary — and it already
shows the day's highest median at 4.8s, hours before any experimental build ran.

```
ENVIRONMENT_DRIFT_CONFIRMED = YES  (medians, p75 and p95 agree; not a mean artifact)
MODEL_PARITY   = YES   deepseek-v4-flash throughout
GATEWAY_PARITY = YES   the same deepseek provider route throughout
MACHINE_PARITY = YES   one machine, runs serial
MODEL_SERVING_REVISION_VISIBLE = NO   the provider exposes no revision, so
                                      "same model string" is not proof of
                                      "same serving behaviour"
```

## The runner timeout stopped being adequate

At the 09-06 regime a 190-request C3 spent 2250s waiting on the model and
finished in 3034s. At the 09-07 regime, `reoffer C3 r2` spent 95.1% of a full
3600s budget waiting on the model with 157 requests — roughly 3400s of pure
model wait before a single tool ran.

```
C3_RUNNER_TIMEOUT_ADEQUATE_AT_CURRENT_PROVIDER_LATENCY = NO
```

Four of six treatment C3 runs hit the wall. Zero of four controls did. That
difference is the clock, not the code, and it means a timeout cannot be read as
a GOOD failure without checking which regime the run was in.

The timeout is deliberately NOT changed here. A frozen evaluation constant is
not something to quietly move mid-investigation, and the diagnostic and formal
budgets probably want to differ. That is a decision to take explicitly.

## Run outcome classification

A timeout is not one thing. These are now distinguished:

- `MECHANICAL_FAIL` — the tree is wrong; the oracle says so.
- `TIMEOUT_ENVIRONMENT_CAPACITY` — killed by the clock while the model-wait
  share alone exceeded the budget for that regime.
- `TIMEOUT_PRODUCT_BEHAVIOR` — killed by the clock at a latency regime where
  the budget was adequate, so the run genuinely did too much work.
- `INVALID_INFRA` — auth, provider outage, harness error.

## Re-scored: delegated investigation reconsideration

Not confounded — latency does not change what the model *chooses*.

| run | reconsideration | decision | children | mech | classification |
|---|---|---|---:|---|---|
| C3 r1 | offered (window_boundary) | KEEP | 0 | PASS | TIMEOUT_ENVIRONMENT_CAPACITY |
| C3 r2 | never reached the trigger | — | 0 | FAIL | TIMEOUT_ENVIRONMENT_CAPACITY |
| C3 r3 | offered (window_boundary) | KEEP | 0 | FAIL | TIMEOUT_ENVIRONMENT_CAPACITY |

```
DELEGATION_ADOPTION_SIGNAL         = 0/2
DELEGATION_TIMING_HYPOTHESIS       = WEAKENED   (2 valid reoffers, not the 3 the design asked for)
DELEGATION_RECONSIDERATION_VALUE   = NOT_PROVEN
DELEGATION_TIMING_AS_FAST_LEVER    = DEPRIORITIZED
```

Nothing about GOOD or wall is claimed from these runs. The adoption number is
the only thing they measured cleanly, and it says reopening the question at a
window boundary did not change the answer.

## Re-scored: L1 independent-inspection batching

The earlier reading — "control 7/7, treatment 5/7, GOOD dropped, reject" — put
controls from the 2.1–2.3s regime against treatments from the 4.1–6.4s one. It
does not support a causal claim about safety.

What survives, because it is observable inside each run independently of how
fast the provider answered:

```
L1_BATCHING_EFFECTIVENESS   = SUPPORTED    multi-tool turns 14.5% -> 21.2% (medians)
L1_BATCHING_REQUEST_SIGNAL  = SUPPORTED    rounds down ~40% on all three tasks
```

What does not:

```
L1_BATCHING_FAST_SIGNAL   = PROMISING_BUT_NOT_CAUSALLY_FROZEN
L1_BATCHING_CHEAP_SIGNAL  = PROMISING_BUT_NOT_CAUSALLY_FROZEN
L1_BATCHING_SAFETY        = INCONCLUSIVE_DUE_TO_ENVIRONMENT_DRIFT
```

**Reverting L1 was still the right action.** An unproven treatment does not
belong in the product. Only the stated reason needs correcting: it was not
shown to hurt GOOD, it was never cleanly measured.

Cost is somewhat steadier than wall — pricing, cache pricing and the model
route are unchanged — but it is not immune: serving behaviour can shift
without a version string changing, and cost follows the model's behaviour.
Contemporaneous controls are required for cost too.

## Parity contract for every future FAST/GOOD/CHEAP experiment

1. Control and treatment run in the same environment window. Same model,
   gateway, machine, task pins, verifier, timeout, and latency regime.
2. **Interleaved, not blocked.** `C T T C` or randomised — never all controls
   one day and all treatments another. That single ordering choice is what
   made two experiments unreadable.
3. Paired per task, with `PAIR_TIME_GAP` recorded.
4. Latency reported as median/p25/p75/p95 over main-task requests. Never a bare
   mean.

## Parity threshold

No arbitrary "within 5%" rule is invented here. The regimes observed so far
are 2.1–2.3s (09-05/06) and 4.1–6.4s (09-07) — a clean separation with no
overlap, and within-regime spread of about ±10%. Until more regimes are
observed, classify by that separation and say which regime each run sat in:

```
ENVIRONMENT_PARITY = FULL        both arms in one regime, gap < within-regime spread
                   = ACCEPTABLE  both arms in one regime, larger gap, stated
                   = CONFOUNDED  arms in different regimes
```

## Pre-flight before any expensive cohort

Check, and hold if any fails: binary identity (never `-dirty`), model, gateway,
provider latency regime, auth, disk, machine load, and timeout adequacy at the
measured regime. A cohort that takes hours should not start against an unknown
environment.

`env_parity.py <run_dir> [label]` in `dogfood/eval/phase-c/` reports the run
side of this from durable logs alone.
