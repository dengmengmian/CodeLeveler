# MA-PRODUCT-CLOSURE task set

Seven public, self-contained Go cases in `evals/cases/multi_agent_closure/`,
one per category (`catalog.json`). They answer MA4's question — does the
current multi-agent build beat the frozen `v0.2.0-beta.2` baseline and a
single-agent control on suitable tasks, without losing truth or safety — and
also parse as ordinary `EvaluationCase` YAML.

| Case | Category | Why it is in the set |
|---|---|---|
| `ma-simple-clamp` | SIMPLE | delegation unsuitable: a spawn here is unnecessary by definition |
| `ma-multifile-timeout` | MULTI_FILE | one coherent change across three files |
| `ma-research-inventory` | PARALLELIZABLE_RESEARCH | four independent packages to read; graded exactly |
| `ma-parallel-impl` | PARALLELIZABLE_IMPLEMENTATION | four independent stubs |
| `ma-review-ratelimit` | REVIEW_HEAVY | a fix whose edge cases a visible test does not cover |
| `ma-long-kvstore` | LONG_GOAL | two packages built from a long contract |
| `ma-scale-research` | PARALLELIZABLE_RESEARCH_LARGE | scale-up: the inventory task over ten packages, fifty files |
| `ma-scale-impl` | PARALLELIZABLE_IMPLEMENTATION_LARGE | scale-up: the eight packages of the two small implementation cases in one module |
| `ma-recovery-parallel` | RECOVERY | killed once mid-run (after a child made progress, or after parent progress if none spawned), then resumed |

The two scale-up cases were added after batch 1 of the seven base cases
recorded zero natural delegation in every arm (those tasks finished in 5–19
tool calls). They were declared before any scale-up run and follow the same
rules. The first seven cases are unchanged.

Rules:

- Task text never mentions sub-agents. Whether to delegate is the model's.
- `expect` is independent: most cases write hidden tests before running
  them, so passing the visible tests is not enough.
- Each case was checked to fail `expect` at its starting state and pass with
  a reference solution (the solutions are not committed, so they cannot leak
  into a run).

Run and report with `evals/scripts/multi_agent_ab.py` (see its docstring).
Metrics: `evals/lib/ab.py` (truth, useful / unnecessary delegation,
aggregates) over `evals/lib/child_lifecycle.py` (typed child terminals,
duplicate settlement, open orphans, lost children, ownership violations,
requests / tokens / cached tokens / cost per lane).

Useful delegation is counted from facts, per child:

| Label | Rule |
|---|---|
| `independent_subtask` | a writing child ended `completed`, and a path it wrote is in the final change of a run that passed `expect` |
| `evidence_not_redone` | a read-only child ended `completed_with_findings`, and the parent re-read fewer of the child's files than the child read |
| `independent_review` | a reviewer ended `completed` (harness-launched, not counted as model delegation) |

A run that spawned a non-reviewer child with no useful label, or any spawn on
a SIMPLE case, is unnecessary delegation.
