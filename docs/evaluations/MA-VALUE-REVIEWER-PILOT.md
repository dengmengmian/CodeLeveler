# MA-VALUE-REVIEWER-PILOT — Independent Reviewer Value

**Status:** framework ready, not executed · **Subject:** `v0.2.0-beta.1` · **Opened:** 2026-08-24

Observer suite. Does not change reviewer permissions, spawn runtime, tool
schema, or prompts. Finding count is not a success metric.

Question: **does an independent Reviewer after implementation produce better
final code than Single Agent self-verification?**

Explorer Value already answered “does parallel exploration help?” (yes, +16 %
relative). This experiment asks the tighter question: does a second pair of
eyes improve the *write-code loop*.

## Hypothesis

- **H1:** Reviewer finds defects the implementing agent’s self-check missed.
- **H2:** Reviewer raises final correctness (independent `expect`).
- **H3:** The extra tokens / turns / wall time are worth that gain.

A fail does **not** mean Reviewer has no value. See [Failure
interpretation](#failure-interpretation).

Zero findings from a Reviewer is a valid outcome. A Reviewer that emits
findings nobody judged is noise, and noise is a regression.

## Experimental design

| | Control (`self`) | Treatment (`reviewer`) |
| --- | --- | --- |
| Isolated `LEVELER_HOME` | `agents.independent_review = "off"` | `agents.independent_review = "always"` |
| Product default (untouched) | `auto` (shape trigger) | `auto` |
| Reviewer tools | n/a | Child Profile `reviewer`: read-only observe-class |
| Model / task / tools / budget | same | same |
| Runtime | unchanged | unchanged |

The only product input that differs is the shipped
`agents.independent_review` key, written the same way MA-VALUE-001 writes
`agents.delegation`. `~/.leveler` is not modified. Reviewer cannot write.

`always` launches after **any product mutation**, not only security /
concurrency / wide-diff paths. Small coding tasks would not fire `auto`.
The experiment measures “add a Reviewer after implementation”, which is
the question, not “does today’s shape trigger help?”.

`off` wins if either global or project config is `off`. Default is `auto`.

### Tasks

Five vendored EvaluationCase files. Independent `expect` is the hidden
verifier. Not audit. Not CRUD.

| ID | Pressure | Source |
| --- | --- | --- |
| `icg-2-bug-fix` | Hidden defect | `evals/icg/` |
| `rust-race-counter` | Concurrency | `evals/hard/` |
| `ts-concurrency-limit` | Async limit | `evals/core/` |
| `ts-redact-secrets` | Secrets | `evals/core/` |
| `icg-3-cross-module` | API / cross-module | `evals/icg/` |

Pointers live in `eval/suites/multi_agent/reviewer_value/cases/`.

### Scale (pilot)

5 tasks × 2 modes × 1 run = **10 runs** (5 pairs).

n < 6 valid runs per arm → `insufficient_n`, not a published verdict.
This pilot is a **pipeline check**. It cannot PASS/FAIL the product.

This phase implements the framework. It does **not** execute the 10
real-model runs (cost). Score existing EventLogs when they exist.

## Metrics

**Not a success metric: finding count.** A verbose reviewer that nobody
judges is noise.

| Family | Fields | Source |
| --- | --- | --- |
| Task success | `task_success` | Independent `expect`. Never the agent’s summary. |
| Reviewer contribution | `reviewer_spawned`, `findings_generated/accepted/verified/rejected`, `useful_findings`, `zero_findings`, `noise` | `sub_agent_started` role=reviewer + `contribution` on finish |
| Quality | `tests_passed`, `regressions`, `missed_issues` | Verification checks. `regressions` / `missed_issues` are human/paired — EventLog does not invent them. |
| Cost | `turns`, `tokens`, `wall_time`, `tool_calls` | EventLog. Missing → `null`, not `0`. |

Lifecycle the scorer must be able to see:

```
Created → Acknowledged → Accepted → Addressed → Verified
                       ↘ Rejected
```

`useful_findings` = accepted (includes addressed / verified). A rejection
is a judgment, not noise. Noise = generated and never judged.

## Pilot validation (before any formal run)

The scorer, not the model, must prove:

1. Reviewer spawn is detected (`role=reviewer`).
2. Findings, including zero, are counted from the contribution projection.
3. Accepted / verified / rejected are distinguishable.
4. Independent `expect` is the quality score.

Those four are locked by `eval/tests/test_reviewer.py`.

## Decision criteria

**Pilot:** always `insufficient_n` at n=5. Do not publish a product verdict.
Do not implement product changes based only on this pilot.

**Formal run later (n ≥ 6 per arm) PASS** when all hold:

1. Task success does not drop versus self-verify.
2. A reviewer actually ran on the treatment arm.
3. At least one of: useful findings, higher success rate.
4. Not a noise regression (unjudged findings up, useful findings still zero).

**FAIL** is `no_measured_improvement_…` or `reviewer_did_not_run`, never
`reviewer_has_no_value`.

## Failure interpretation

- **Task type.** A miss on a tiny unit-test case is not a miss on a
  security change.
- **Assignment.** `always` is a stronger intervention than product `auto`.
  A pass does not prove the shape trigger is well calibrated.
- **Noise.** Reviewer ran, dumped findings, parent never judged them.
- **Cost.** Extra wall time with no quality gain is H3 failing, not H1.

If the pilot’s scorer cannot see spawn / lifecycle / expect, fix the
observer before spending model runs.

## How to run

```sh
# Framework (no model calls) — wires isolated home for the arm
leveler eval run --suite multi_agent --experiment MA-VALUE-REVIEWER-PILOT --mode self
leveler eval run --suite multi_agent --experiment MA-VALUE-REVIEWER-PILOT --mode reviewer

# Pilot execution (expensive): 5 tasks × 1 run per arm
python3 eval/runner/run.py --suite multi_agent --experiment MA-VALUE-REVIEWER-PILOT \
  --mode self --execute
python3 eval/runner/run.py --suite multi_agent --experiment MA-VALUE-REVIEWER-PILOT \
  --mode reviewer --execute
```

## Limitations

- Reviewer remains harness-launched and structurally read-only. This
  experiment does not let the model request `profile=reviewer`.
- `always` is not the product default. A later formal experiment may also
  need an `auto` arm if the question becomes “is the shape trigger enough?”
- n=5 cannot support a published PASS.
- Hidden defects the verifier does not cover stay `missed_issues` (human).

## Recommendation

Do not expand to a large run until a scored EventLog shows (1)–(4) above
on at least one real coding task. If that log exists, the next spend is
n ≥ 6 per arm on the same five cases, still without product changes
driven by the numbers.
