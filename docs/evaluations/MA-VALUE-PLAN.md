# MA-VALUE — Multi-Agent Value Evaluation, design and power

**Opened:** 2026-08-23 · **Status:** design + pilot · **Subject:** `main` @ `ce5b7d8b`

The question is not whether Multi-Agent should exist. That decision is made, and
Spawn Reliability closed PASS ([`MA-RELIABILITY-REPORT.md`](MA-RELIABILITY-REPORT.md)).
The question is **where it creates measurable coding value, and at what cost**.

## The finding that shapes everything else

Before designing sample sizes, the pilot measured what identical inputs actually
produce. Three Multi-Agent runs — same frozen goal, same model, same repository,
`git reset --hard` between runs:

| | run 1 | run 2 | run 3 | CV |
| --- | --- | --- | --- | --- |
| Input tokens | 2.69 M | 0.90 M | 3.36 M | **54.8 %** |
| Tool calls | 332 | 163 | 409 | **41.8 %** |

**A coefficient of variation near 55 % on identical inputs.** That is the noise
floor any cost comparison has to clear, and it decides what an experiment can
possibly conclude:

| n per arm | Smallest detectable token difference | …tool-call difference |
| --- | --- | --- |
| **3** | **125 %** | 95 % |
| **10** | **69 %** | 52 % |
| 20 | 49 % | 37 % |
| 30 | 40 % | 30 % |
| 53 | 30 % | 23 % |
| 118 | 20 % | 17 % |

*(two-sample, 80 % power, α = 0.05)*

**The suite as configured — 6 tasks × 2 arms × 3 runs — is powered to detect
nothing on cost.** At n=3 the Multi-Agent arm would have to be **2.25× more
expensive** before the difference clears the noise. Roughly 83 M input tokens
would be spent to learn that.

This is standing constraint 4 arriving in practice: *state the detectable effect
before spending the runs*.

## What this does NOT mean

It does not mean the experiment is worthless. It means **cost is the wrong
primary outcome**, because cost is where the variance lives.

Three outcome families, and their statistical prospects:

| Outcome | Noise | Prospect |
| --- | --- | --- |
| **Cost** (tokens, calls) | CV ≈ 55 % | Needs n≈53/arm for a 30 % effect. Secondary at best. |
| **Success** (completed, verified) | binary | n=10/arm distinguishes only large gaps (e.g. 10/10 vs 5/10). Usable if the effect is big. |
| **Quality** (findings correct, findings missed) | judged | Lowest noise *if* the rubric is fixed in advance and applied blind. Most promising. |

The primary question — *does parallel exploration make the answer better* — is a
**quality** question, and quality is the one place the signal may exceed the
noise at an affordable n.

## Design

### Pairing, not independent samples

Every task runs both arms. Analysis is **per-task paired differences**, which
removes between-task variance entirely — the largest and least interesting
source. Only within-task run-to-run noise remains, which is what the table above
measures.

### Arms

| | Control | Treatment |
| --- | --- | --- |
| Config | `agents.delegation = false` | product default |
| Everything else | identical | identical |

`agents.delegation` is a **shipped product key**, default `true`, not an
experiment-only switch. Setting it false stops `spawn_agent` being advertised.
Verified: a control run mentions `spawn_agent` zero times. Written into an
isolated `LEVELER_HOME`; `~/.leveler` is never modified.

This satisfies constraint 5 — eval observes, it does not special-case. No
forced delegation, no scripted spawn, no `eval_mode`.

### Experiments

**A · Explorer value** — repository-understanding tasks (explain architecture,
locate a subsystem, identify risks, map dependencies). The shape that occurs
naturally: in the reliability baseline the model elected 3–4 explorers on every
run, so the treatment arm reliably *has* a treatment.

**B · Reviewer value** — implement, then either self-verify (control) or be
reviewed by an independent agent (treatment). Measures bugs found, valid
findings, false positives, final verification.

**C · Worker parallelism** — only after A and B, and only if either shows value.
Parallel editing is not assumed to help; ownership safety and conflict detection
are part of what is measured, not assumed.

### Metrics

Collected per run: `completed`, `verified`, correctness against a rubric fixed
before the run; input/output tokens, tool calls, turns, failures; and the
collaboration chain —

```
child_created → child_completed → child_result_available
              → child_result_consumed → child_contribution
```

**Spawn rate is not a success metric.** Zero delegations on a task that did not
need one is a pass. An intervention that raises spawning while raising
*unnecessary* spawning is a regression.

## Cost, stated honestly before anything is spent

At the measured ~2.3 M input tokens per Multi-Agent run:

| Design | Runs | ≈ input tokens | What it can conclude |
| --- | --- | --- | --- |
| Suite as configured (6 tasks × 3) | 36 | ~83 M | **Cost: nothing.** Success: only a huge gap. |
| 2 tasks × 10 per arm | 40 | ~92 M | Cost: 69 % effects. Success: large gaps. |
| 2 tasks × 30 per arm | 120 | ~276 M | Cost: 40 % effects. |
| 1 task × 53 per arm | 106 | ~244 M | Cost: 30 % effects, on one task shape. |

None of these is cheap, and the top row is the only one that is *also*
uninformative. Spending more is not automatically better — spending on the wrong
outcome is.

## Recommendation

1. **Lead with quality, not cost.** Fix a rubric before running; judge blind to
   arm. Report cost as an observation with its confidence interval, never as a
   headline.
2. **Do not run the 36-run configuration as-is.** It is the worst
   cost-per-conclusion on the table.
3. **Report effects with intervals.** With CV ≈ 55 %, any point estimate of
   "Multi-Agent costs X % more" from a handful of runs is noise wearing a
   number.
4. **One task shape, one model is not the envelope.** Fan-out is model-dependent
   — `k3` fans out where DeepSeek issues one. Conclusions bind to the pairing
   tested.

## Status

- [x] Framework inspected; the existing `eval/suites/multi_agent` is reused, not duplicated
- [x] Control arm verified to actually suppress delegation
- [x] Treatment-arm variance measured (n=3)
- [ ] Control-arm variance measured (n=3, running)
- [ ] Paired power recomputed with both arms
- [ ] Rubric fixed and hashed before any scored run
- [ ] Scored experiment — **scale is a spending decision, and the table above is what it buys**
