# MA-VALUE-REVIEWER-FORMAL — protocol

**Status:** protocol · **Opened:** 2026-08-24 · **Not executed · No results claimed**

Successor to [MA-VALUE-REVIEWER-PILOT](MA-VALUE-REVIEWER-PILOT.md), which was
[halted](MA-VALUE-REVIEWER-FINAL.md) because the treatment arm was unobservable
and the control arm was saturated.

**Both preconditions must be satisfied before this protocol runs.** They are
listed as gates below, not as assumptions.

## Question

> Does an independent Reviewer, inserted after implementation, produce better
> final code than single-agent self-verification — and is the extra cost worth
> it?

Not "can a Reviewer find bugs". A Reviewer that finds real defects nobody acts
on has changed nothing about the code that ships.

## Gates

The experiment does not start until all four hold. Each is checkable without
spending a model run.

| # | Gate | Check | State |
| --- | --- | --- | --- |
| G1 | Reviewer contribution is observable | `sub_agent_finished.contribution` is `Some` on the `independent_review` path | ✅ Phase 1 — verified end to end, 5/5 real runs, `contribution_unmeasured = 0` |
| G2 | Unmeasured ≠ zero | observer reports `contribution_unmeasured` separately from `zero_findings` | ✅ `eval/lib/reviewer.py` |
| G3 | Independent `expect` reaches the run record | `verifier.ran` is true for every scored run | ✅ `load_expect_verdicts` |
| G4 | Control arm has headroom | measured control pass rate 50–70 % per case, n ≥ 3 | ❌ **blocking** — see [MA-VALUE-REVIEWER-TASKS](MA-VALUE-REVIEWER-TASKS.md) |

G4 is the remaining blocker. It requires real model runs (calibration) and is
not satisfiable by writing more code.

## Design

Paired, within-case. Same task set, same model, same seed conditions; one
config key differs.

### Control — `--mode self`

```
Single Agent
  ↓
Implementation
  ↓
Self verification
  ↓
Independent expect  (hidden)
```

`agents.independent_review = "off"` in an isolated `LEVELER_HOME`.

### Treatment — `--mode reviewer`

```
Worker
  ↓  implementation lands
Reviewer          (read-only, harness-launched)
  ↓  report_finding
Finding           → adopted into parent ledger at Acknowledged
  ↓  resolve_finding
Fix               → Accepted → Addressed
  ↓
Verification      → Verified
  ↓
Independent expect  (hidden)
```

`agents.independent_review = "always"` in an isolated `LEVELER_HOME`.

### Frozen

Task set · model · scoring · reviewer prompt · reviewer tools · reviewer
permissions (read-only) · trigger mode (`always`) · spawn runtime.

`always` is not the product default (`auto`). This measures "add a Reviewer
after implementation", which is the question. Whether the shape trigger is
well calibrated is a **different** experiment and must not be inferred from
this one's result.

`~/.leveler` is never written. The arm delta must be exactly one line; verify
with a diff of the two isolated `config.toml` before scoring.

## Scale

| | |
| --- | --- |
| Cases | 6–8 admitted (≥ 1 per category) |
| Repetitions | 3 per case per arm |
| Minimum valid | **n ≥ 6 paired runs per arm** |

Below n=6 the result is `insufficient_n` and is not published, regardless of
how the numbers look.

## Metrics

### Primary

**Final correctness** — the independent `expect`, per run, hidden from the
agent. Never an agent's self-report, never an LLM judge.

Estimand: paired difference in pass rate, treatment − control.

### Secondary — Reviewer contribution

From `ChildResultProjection` on `sub_agent_finished`:

```
created → acknowledged → accepted → addressed → verified
                       ↘ rejected (terminal)
```

| Field | Meaning |
| --- | --- |
| `findings_total` | reported and adopted |
| `findings_acknowledged` | reached the parent |
| `findings_accepted` | parent judged relevant |
| `findings_verified` | proven by fresh post-mutation verification |
| `findings_rejected` | parent declined, with a reason — **a contribution** |
| `contribution_unmeasured` | no projection; must be 0 for a valid run |

`useful_findings = accepted`. `noise = generated and never judged`.

**Finding count is not success. A verified useful finding is.**

A rejection is a contribution: the parent read it and decided. What is not a
contribution is a finding nobody ever judged.

Zero findings from a Reviewer is a valid outcome and is reported as a measured
zero, not as a failure.

### Secondary — trap detection

Per [MA-VALUE-REVIEWER-TASKS](MA-VALUE-REVIEWER-TASKS.md), each case names a
`hidden_defect_opportunity`. `trap_found: true | false | null` is a human read
of the finding text against it. Not automated — an LLM judging output from its
own family is not independent. `null` when nobody scored it; `null` is not
`false`.

### Cost

`tokens` · `turns` · `tool_calls` · `wall_time_ms`, from the EventLog.
Missing is `null`, never `0`.

Reported per arm as mean and median. At the pilot's n=1 the control arm's
token mean was inflated by two 19/22-round outliers; medians are the honest
summary at small n.

## Decision rules

### PASS — all four must hold

1. Task success does not drop versus control.
2. A reviewer actually ran on the treatment arm (`reviewer_spawned > 0`).
3. At least one of: useful findings > 0, or higher success rate.
4. Not a noise regression (unjudged findings up, useful findings still zero).

### FAIL

Reported as `no_measured_improvement_<detail>` or `reviewer_did_not_run`.

**Never `reviewer_has_no_value`.** A fail on this task set, at this model, with
`always` triggering, is a fail of that configuration. It does not generalize to
"a second pair of eyes does not help".

### Invalid

`insufficient_n` (n < 6) · any run with `contribution_unmeasured` ·
any run where `verifier.ran` is false · control arm outside the 50–70 % band.

An invalid run is discarded before analysis, and the discard is reported.

## Failure interpretation

| Observed | Reading |
| --- | --- |
| Reviewer finds nothing | Task type has no trap, or the trap is not diff-visible |
| Findings generated, never judged | Noise — the parent did not engage; a regression |
| Findings accepted, success flat | Reviewer is right and it did not matter for `expect` |
| Success up, cost up a lot | H3 fails, H1/H2 hold — a pricing question, not a value question |
| Success flat at ceiling | G4 was violated; the run is invalid, not a null result |

## How to run

```sh
# Framework only, no model calls — verifies arm isolation
leveler eval run --suite multi_agent --experiment MA-VALUE-REVIEWER-FORMAL --mode self
leveler eval run --suite multi_agent --experiment MA-VALUE-REVIEWER-FORMAL --mode reviewer

# Real runs (expensive)
python3 eval/runner/run.py --suite multi_agent --experiment MA-VALUE-REVIEWER-FORMAL \
  --mode self --runs 3 --execute --binary target/release/leveler
python3 eval/runner/run.py --suite multi_agent --experiment MA-VALUE-REVIEWER-FORMAL \
  --mode reviewer --runs 3 --execute --binary target/release/leveler
```

Use a freshly built binary for both arms and record its version string. The
pilot's first attempt used a stale `~/.cargo/bin/leveler` that did not know the
`self`/`reviewer` modes at all.

## Rules for whoever runs this

- Do not modify Reviewer behavior based on intermediate results.
- Do not adjust a case after seeing the treatment arm.
- Do not fake or extrapolate a run that did not happen.
- Do not publish a verdict below n=6.
- If infrastructure fails, stop and report — do not work around it in-flight.

## Related

- [MA-VALUE-REVIEWER-FINAL](MA-VALUE-REVIEWER-FINAL.md) — why the pilot stopped
- [MA-VALUE-REVIEWER-TASKS](MA-VALUE-REVIEWER-TASKS.md) — the G4 case set
- [MA-VALUE-A-FINAL](MA-VALUE-A-FINAL.md) — Explorer, the precedent
