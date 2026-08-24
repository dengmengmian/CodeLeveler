# MA-VALUE-A — Explorer Multi-Agent, formal result

**Ran:** 2026-08-24 · 20 runs (10 per arm) · `deepseek-v4-flash` ·
task + scorer frozen and hashed before execution (`c4b8706`, `418eb05`)

## Executive Summary

```
Explorer Multi-Agent:  QUALITY IMPROVEMENT CONFIRMED
                       +2.8 / 26 points, 95% CI [+1.05, +4.55]
                       68% → 79%, a 16% relative gain
                       at 2.5x wall time
```

Parallel exploration finds materially more of what a repository audit should
contain, and the effect survives a paired test at n=10. It is not free: the
treatment arm took 2.5× as long.

**Decision rule matched: Case 2** — quality improves, cost is high but not
prohibitive. Multi-Agent should be *adaptive*, not universal.

## Hypothesis

**H1:** parallel exploration improves repository understanding.
**H0:** it makes no measurable difference.

H0 is rejected. What is *not* claimed is anything about writing code — see
[Limitations](#limitations).

## Experiment design

| | Control | Treatment |
| --- | --- | --- |
| Config | `agents.delegation = false` | product default |
| Model, repo, task, tools, budget | identical | identical |
| Starting state | `git reset --hard 0a462660` before every run | same |

`agents.delegation` is a shipped product key, not an experiment switch. No
forced delegation, no scripted spawn, no `eval_mode`. The treatment arm was free
to delegate or not; it chose to, every time.

Task: a bounded engineering audit of the repository — broad enough to decompose,
scoped to static investigation so build time did not dominate. 26 checkable
facts across architecture, security, testing, dependencies and reliability.

Arm order alternates by repetition, so position does not reveal the arm.

## Validation — run before collecting anything

| Check | Result |
| --- | --- |
| Scorer separates known correct / incomplete / wrong | 26 / 10 / 0 ✓ |
| Spawn counter against a log with an independently known count | 4 / 4 ✓ |
| Task hash matches the piloted version | `d779213b…` ✓ |
| Control really suppresses delegation | **0 spawns in 10/10** ✓ |
| Treatment really delegates | **63 spawns across 10/10 runs** ✓ |

Both instrument checks exist because both instruments were wrong before. V1's
scorer marked correct answers wrong; V2's first spawn counter read zero for runs
that spawned five, and two experiments were discarded on that reading. The
counter check is now a precondition, not a formality.

## Results

### Quality

| | mean | sd | range |
| --- | --- | --- | --- |
| Control | 17.8 / 26 (68 %) | 1.55 | 16–20 |
| Treatment | **20.6 / 26 (79 %)** | 2.59 | 17–24 |

### Paired analysis

Identical task both arms, so pairs are the estimate:

```
differences:  [-2, 0, +3, +3, +4, +6, +3, +6, +2, +3]
mean         +2.80 / 26
95% CI       [+1.05, +4.55]      excludes zero
t(9)         3.63                critical 2.262
positive     8/10   ·  zero 1/10  ·  negative 1/10
```

One pair went the other way (−2) and one tied. The effect is real but **not
uniform** — a single run tells you nothing, which is exactly why V1's "no
difference" reading from underpowered data would have been wrong in both
directions.

### Cost

| | mean | sd |
| --- | --- | --- |
| Control wall time | 468 s | 506 |
| Treatment wall time | **1175 s** | 786 |
| Ratio | **2.5×** | |
| Children per treatment run | 6.3 | |

**Cost-effectiveness: 1.85 rubric points per additional unit of time.**

Note the treatment arm ran at the fan-out cap: 6.3 children per run against a
`DEFAULT_MAX_TOTAL_AGENTS` of 6, i.e. it is saturating the limit rather than
choosing a comfortable number.

### Collaboration

Fully measured: `child_created` (63) and `child_completed` (63, zero ghosts,
consistent with the reliability gate).

**Not measured: whether specific child findings reached the final answer.** The
scorer grades the parent's report, so child contribution is inferred from the
score difference, not traced. That is a real gap and the honest place to close
it is `Structured Child Result` — a typed result makes consumption traceable
instead of inferred.

## Limitations

These bound the conclusion and are not boilerplate.

1. **This is exploration, not coding.** CodeLeveler's job is producing correct
   diffs; this measures the quality of an audit report. Exploration feeds
   coding, but "finds more facts" does not entail "writes better code". The
   experiment that would settle that is the Reviewer arm, and it has not run.
2. **Precision is not measured.** Recall is (score / 26 gold facts). Detecting
   *wrong* claims needs a reliable wrong-claim detector, and the one attempted in
   V1 misfired on correct answers. Reporting a fabricated precision would be
   worse than reporting none.
3. **One repository, one model, one task shape.** Fan-out is model-dependent —
   `k3` fans out where DeepSeek issues one. These numbers describe this pairing.
4. **Wall time, not tokens.** Cost is measured in elapsed time. Token cost was
   measured separately at a CV near 55 %, so the 2.5× ratio should be read as an
   order of magnitude, not a precise multiplier.
5. **The rubric rewards breadth.** 26 facts across five areas favours an approach
   that covers more ground — which is what parallel exploration does. A rubric
   built around depth on one subsystem might not favour it. This is the most
   important caveat: the measure and the treatment share a bias.

## Product recommendation

**Adaptive, and legible — not universal, not hidden.**

1. **Keep delegation opportunity-based.** The model chose to delegate on 10/10
   of these audits without being pushed. It declined on narrow single-subsystem
   tasks (0 spawns in 20 V1 runs). That discrimination is the behaviour worth
   preserving; forcing a higher rate would break it.
2. **The Beta claim can now say more than "reliable".** *"On broad
   investigation tasks, parallel exploration finds about 16 % more of what
   matters"* is defensible — with the audit-shaped caveat attached.
3. **The UX problem is now specific.** A 2.5× wait is the product's real cost,
   and the surface currently shows "等待任务". Given this result, the UI should
   answer **why the wait is worth it** — which children are running, what each
   has found — rather than merely counting agents. That is a stronger brief for
   Background-first UX than "show agent activity".
4. **Fan-out is saturating the cap.** 6.3 children against a limit of 6 means the
   ceiling is binding on this task shape. Whether that limit costs quality is
   untested and worth knowing before it is raised — the reliability gate covers
   safety at 6, not value above it.

## Next engineering priorities

In dependency order, none of them started here:

1. **Structured Child Result** — makes child contribution traceable rather than
   inferred, and closes the one metric this experiment could not collect.
2. **Reviewer arm (Experiment B)** — the coding-outcome question this round
   deliberately does not answer.
3. **Background-first UX** — now with a concrete brief: justify a 2.5× wait.
