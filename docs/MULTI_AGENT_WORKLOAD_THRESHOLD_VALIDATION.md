# Multi-Agent MA4-B — Workload Size & Parallelism Threshold Validation

Status: **MA4B_WORKLOAD_THRESHOLD=FAIL — value not proven for any workload
size.** No bucket showed a stable, repeated benefit from delegation. Medium
workloads (3 independent units) show a latency and cost signal with equal
quality, but delegation was not repeated there and the fastest run did not
delegate; large and parallel-native workloads are high-variance, and at the
largest size delegated runs were faster but less correct. Every runtime
truth and ownership counter is zero. Stopped after 29 of 45 accepted runs by
user decision (§6).

## 1. Original MA4 result

`docs/MULTI_AGENT_EVAL_AND_ACCEPTANCE.md`: 75 runs; task success baseline
22/25, delegation off 24/25, delegation on 25/25; one natural delegation,
which was slower (120 s vs 93 s) and 6× the cost. The tasks finished in
seconds to a couple of minutes. That result stands; this document adds
evidence, it does not replace it.

## 2. Hypothesis

Multi-agent pays only when the serial work it removes exceeds the
coordination it adds. MA4's units were too small for that to happen. MA4-B
varies only workload size and the number of independent units.

## 3. Experiment design

| | |
|---|---|
| Groups | G1 `v0.2.0-beta.2` (86bc214); G2 current build, `agents.delegation = false`; G3 current build, product default. Primary comparison G2 vs G3 (same build) |
| Current build | `640c77e` (clean release); product code unchanged during the experiment |
| Task ladder | registered and pushed before any run (`c06249f`), never edited after |
| Oracle | hidden test suites written into the tree by `expect`; `expect` refuses edited visible tests; per-unit verdicts scored on registered packages only |
| Execution | `leveler run --collaboration goal --auto-approve`, isolated `LEVELER_HOME`, fresh workspace; accepted batch ran the three groups of one slot at the same time (`--parallel 3`) |
| Harness | `evals/scripts/multi_agent_ab.py`, `evals/lib/{ab,child_lifecycle,coordination}.py` |

### Model lock

```text
MODEL=deepseek-v4-flash
PROVIDER=deepseek (taotoken gateway, the user's configuration)
REASONING_EFFORT=max
MODEL_CHANGED_DURING_EXPERIMENT=NO
```

### Gate definitions (user decision, 2026-09-14)

`FALSE_VERIFIED` (the product claimed verification that its own checks did
not give) is the runtime-truth hard gate. `INCORRECT_AND_VERIFIED` (the
product's visible checks passed, the hidden oracle failed) is a per-group
quality metric: hidden tests are stricter than the checks the product runs,
so it is non-zero by design in every group.

## 4. Task ladder

| Case | Bucket | Independent work units | Expected parallelism |
|---|---|---|---|
| `mat-s0-word-count` | S0 tiny | 1 function | none |
| `mat-s1-port-setting` | S1 small | 1 feature across 2 files + README | none |
| `mat-s2-three-parsers` | S2 medium | semver, ini, cron | medium |
| `mat-s3-four-engines` | S3 large | expr, jsonpath, textdiff, toposort | high |
| `mat-s4-eight-engines` | S4 parallel-native | the S3 four + ratewindow, glob, mdtable, lruttl | high |

Each package states its whole contract in doc comments and has a hidden
suite of roughly 40–250 assertions; every case fails at start and passes with a
reference solution kept outside the repository.

## 5. Pilot

G2 and G3, one run per bucket (10 runs, sequential). Sizes held:
single-agent S2 took 17 min, S3 46 min, S4 40 min (and did not finish), so
no case was too small and none was replaced.

## 6. Accepted runs

Planned 3 reps × 5 cases × 3 groups = 45. Stopped at **29** (rounds 1 and 2
complete except round-2 S4 G2, which was running and is not recorded) when
the user judged that the data already answered the question. The stop is
recorded, not hidden; the driver can resume the missing runs.

```text
G1_RUNS=10  G2_RUNS=9  G3_RUNS=10  (+ pilot G2 5, G3 5)
TOTAL_RUNS=39
```

## 7. Metrics

### 7.1 Decision matrix — accepted runs

| Bucket | G2 pass | G3 pass | G2 units | G3 units | G2 median wall | G3 median wall | Cost G2 → G3 (median, USD) | Token ratio G3/G2 | G3 delegated | Useful children | Decision |
|---|---|---|---|---|---|---|---|---|---|---|---|
| S0 | 2/2 | 2/2 | 2/2 | 2/2 | 15 s | 14 s | 0.009 → 0.009 | 0.97 | 0/2 | — | NEUTRAL |
| S1 | 2/2 | 2/2 | 4/4 | 4/4 | 23 s | 30 s | 0.011 → 0.012 | 1.10 | 0/2 | — | NEUTRAL |
| S2 | 2/2 | 1/2 | 6/6 | 5/6 | 819 s | 437 s | 0.240 → 0.108 | 0.43 | 1/2 | 0/3 | INSUFFICIENT_EVIDENCE |
| S3 | 0/2 | 1/2 | 3/8 | 6/8 | 1180 s | 1534 s | 0.534 → 0.399 | 0.69 | 1/2 | 3/4 | INSUFFICIENT_EVIDENCE |
| S4 | 0/1 | 0/2 | 4/8 | 7/16 | 1727 s | 1278 s | 1.270 → 0.914 | 0.69 | 2/2 | 0/12 | MULTI_AGENT_NEGATIVE (quality) |

### 7.2 Accepted + pilot (G2, G3)

| Bucket | G2 pass | G3 pass | G2 units | G3 units | G2 median wall | G3 median wall | Cost G2 → G3 | G3 delegated | Useful children |
|---|---|---|---|---|---|---|---|---|---|
| S0 | 3/3 | 3/3 | 3/3 | 3/3 | 14 s | 14 s | 0.008 → 0.009 | 0/3 | — |
| S1 | 3/3 | 3/3 | 6/6 | 6/6 | 25 s | 39 s | 0.011 → 0.014 | 0/3 | — |
| S2 | 2/3 | 2/3 | 8/9 | 8/9 | 1036 s | 479 s | 0.225 → 0.176 | 2/3 | 3/6 |
| S3 | 1/3 | 1/3 | 7/12 | 9/12 | 1294 s | 2002 s | 0.737 → 0.446 | 2/3 | 3/8 |
| S4 | 0/2 | 0/3 | 10/16 | 14/24 | 2060 s | 1491 s | 1.337 → 1.303 | 2/3 | 0/12 |

G1 (beta.2, historical reference): S2 1/2 (5/6 units), S3 1/2 (7/8), S4
0/2 (11/16); it delegated in 5 of its 6 runs at S2–S4.

### 7.3 Delegated vs not, within G3

| Bucket | Delegated G3 runs (wall, units, cost) | Non-delegated G3 runs |
|---|---|---|
| S2 | 528 s 3/3 $0.18 · 479 s 2/3 $0.18 | 395 s 3/3 $0.04 |
| S3 | 2002 s 3/4 $0.56 · 794 s 4/4 $0.45 | 2274 s 2/4 $0.35 |
| S4 | 1491 s 3/8 $1.30 · 1065 s 4/8 $0.52 | 2484 s 7/8 $1.47 |

## 8. Delegation behaviour

```text
DELEGATION_ADOPTION_BY_BUCKET (G3, accepted+pilot):
S0 0/3  S1 0/3  S2 2/3  S3 2/3  S4 2/3
UNNECESSARY_DELEGATION_REGRESSION=NO (no spawn in S0/S1 in any group)
```

The model's adoption tracks size the way a policy should: none on tiny and
small work, about two thirds on medium and larger work. Useful children
(retained, correct work in a run that passed the oracle) appeared in only
two G3 runs: S2 pilot (3/3) and S3 accepted round 1 (4/4). A child's work
in a run that failed the oracle is not counted as useful, by the registered
definition.

## 9. Coordination overhead (G3 delegated runs)

| Run | First spawn | Parent planning | Child critical path | Serial child work | Parallel time saved | Parent integration | Coordination overhead |
|---|---|---|---|---|---|---|---|
| S2 pilot | 164 s | 149 s | 274 s | 704 s | 430 s | 89 s | 239 s |
| S2 accepted r0 | 166 s | 44 s | 278 s | 617 s | 339 s | 35 s | 79 s |
| S3 pilot | 755 s | 36 s | 1149 s | 2267 s | 1118 s | 98 s | 135 s |
| S3 accepted r0 | 108 s | 95 s | 664 s | 1931 s | 1267 s | 21 s | 116 s |
| S4 accepted r0 | 73 s | 55 s | 1342 s | 2866 s | 1524 s | 0 s | 131 s |
| S4 accepted r1 | 59 s | 55 s | 985 s | 2007 s | 1022 s | 20 s | 75 s |

```text
MEDIAN_TIME_TO_FIRST_SPAWN=137 s
MEDIAN_PARENT_PLANNING=55 s
MEDIAN_PARENT_INTEGRATION=28 s
MEDIAN_COORDINATION_OVERHEAD=123 s
MEDIAN_CHILD_CRITICAL_PATH=825 s
MEDIAN_PARALLEL_TIME_SAVED=1070 s
```

Parallel time saved exceeds coordination overhead in every delegated run
from S2 up — the MA4 hypothesis (units too small) is confirmed as far as it
goes. It does not turn into a reliable product win because of what the
overhead formula does not capture:

- **Exploration before the first spawn** varies from 1 to 12.5 minutes (S3
  pilot: 755 s before any child existed).
- **Child quality**: at S4 the delegated runs got 3/8 and 4/8 units right
  against 7/8 for the non-delegated G3 run; children implement from a brief,
  without the parent's full reading of the contracts.
- **Round and budget ceilings**: 4 of the 7 S4 runs stopped at the product's
  **100-round single-turn ceiling** (`budget_limited`, even with
  `--max-rounds 400`): both G2 runs, one delegated G3 run and one delegated G1
  run; the other three finished within 21–95 parent requests. One S4 child
  stopped on its share of the parent's budget. S4 therefore partly measures
  what fits under that ceiling.

## 10. Break-even analysis

```text
FIRST_POSITIVE_BUCKET=NONE (S2 closest: latency −47…−54%, cost lower, quality equal)
BREAK_EVEN_PROVEN=NO
MINIMUM_INDEPENDENT_WORK_UNITS=3 (where parallel time saved first exceeds overhead)
APPROX_SERIAL_DURATION_THRESHOLD≈10 min of serial child work (S2: 617–704 s)
```

S2 has the right shape for a benefit but not the evidence: one of its two
accepted G3 runs did not delegate and was the fastest of all, so the
latency gain cannot be attributed to delegation, and useful delegation was
not repeated.

## 11. Safety

```text
FALSE_VERIFIED_TOTAL=0
OWNERSHIP_VIOLATION=0 (after fix 1866c3c; 30 child writes whose path the log truncated are unattributed, 0 suspect)
LOST_ACCEPTED_CHILD=0
DUPLICATE_SETTLEMENT=0
OPEN_ORPHAN=0
CHILD_WRITE_AFTER_TERMINAL=0
RECOVERY_DUPLICATION=0
INCORRECT_AND_VERIFIED (quality metric): G1 2/10, G2 3/14, G3 5/15
```

G3 has the highest incorrect-and-verified rate: when children implement a
package that the visible tests accept but the hidden contract rejects, the
parent's closing verification does not catch it any better than a single
agent does.

## 12. Eval harness

| Change | Commit | Affected runs recalculated |
|---|---|---|
| coordination metrics, suite switch, per-case timeout, bucket grouping | `c06249f` | n/a (before runs) |
| per-unit quality | `3da2593` | yes (rescore) |
| slot-parallel runs | `ab3e92c` | n/a |
| **bug**: truncated tool argument read as an ownership violation (pilot showed 2 and 3) | `1866c3c` | yes, all pilot and accepted runs |
| **bug**: a scratch package the agent left behind counted as a work unit | `18889ae` | yes, all runs |

Every fix was test-first; no JSON was edited by hand; no verdict flipped on
rescoring.

## 13. Decision

```text
MA4B_WORKLOAD_THRESHOLD=FAIL
MULTI_AGENT_PRODUCT_VALUE=NOT_PROVEN
MA4_EVAL_ACCEPTANCE=FAIL (unchanged)
MULTI_AGENT_PRODUCT_CLOSURE=BLOCKED
```

## 14. Product policy

```text
TINY=single-agent (the model already never delegates; keep it)
SMALL=single-agent
MEDIUM=model decides (current behaviour); the only bucket with a positive signal — not a claimed benefit
LARGE=model decides; no reliable benefit; do not promote
PARALLEL_NATIVE=no benefit under current limits; delegated runs were less correct; bounded by the 100-round turn ceiling and the child budget split
DEFAULT_MULTI_AGENT_POLICY=keep delegation available and model-chosen, as shipped; do not force, raise or advertise it as faster or better
```

## 15. Supplemental: model variant

`deepseek-v4-flash-vision-exp` (official endpoint, which resolves it to the
same `deepseek-flash` model): 9 runs of the MA4 small cases, 9/9 passed, no
delegation; stopped when the experiment design changed.
`USED_FOR_AUTHORITATIVE_DECISION=NO`.

## 16. Residuals (mechanical)

- Round-2 S4 G2 not recorded (early stop); S4 G2 has n=1 accepted.
- The 100-round single-turn ceiling truncated 4 of 7 S4 runs, across groups.
- Pilot ran sequentially; the accepted batch ran three groups at once on one
  machine (same load for the groups of a slot).
- Main CI failed on `640c77e` (and `7a0b4b2`) in the Windows grandchild
  job-object canaries, which also failed on older heads `be6e8d4` and
  `2d1cf19`; every later head (`c06249f` … `18889ae`) is green.

## 17. Next

Stop. The model stays fixed and reasoning stays as configured. Candidate
experiments, for the user to choose:

- **A. Parent Reasoning Budget Validation** — the parent's time before and
  around spawning, not the children, dominates the variance.
- **B. Stronger Orchestrator Model Validation** — child briefs and
  integration decided quality at S4.
