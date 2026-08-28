# HC-001 Task Contract

Frozen **before Run #1**. Do not edit acceptance after seeing results.

| Field | Value |
|---|---|
| case_id | `HC-001` / `n3-caller-propagation` |
| source | `evals/navigation/n3-caller-propagation.yaml` |
| repo | `fixtures/repos/navsvc` + overlay in the YAML `files:` block |
| revision | fixture HEAD as cloned, then overlay committed as `eval baseline` |
| timeout | **1200 seconds** wall clock, identical for all 6 runs |
| prompt | the YAML `task:` field, byte-for-byte, no harness-specific suffix |

## Prompt (canonical)

```
A decoder marks a record `Valid = false` when it could read the record's
shape but not its contents — a metric line whose value column is garbage,
for example. Those records currently reach the report and are counted as if
they were real observations, which inflates every number we publish.

Reporting must ignore records that are not valid. This applies to everything
the report package answers about a set of batches, not only the per-metric
summary lines.

Note that the pipeline's `filter` stage is opt-in configuration and users
routinely run without it; reporting must be correct regardless.

Do not change existing tests. `go build ./...` and `go test ./...` must pass.
```

## Why this case

Medium cross-file behavioural bug. A fix that only touches `Summary.Observe`
still compiles and still passes the maintained suite. `report.Distinct` answers
a different question over the same records and must make the same validity
decision; nothing in the compiler points at it. That is the plausible wrong
hypothesis this case exists to catch.

It is not a one-line obvious fix, and not a multi-hour feature. Target duration
is roughly 15–40 minutes; if every harness finishes far under that, the report
must say the duration band was missed.

## Visible obligations (agent may infer from the prompt)

- Invalid records must not be counted in reporting.
- The rule applies to **everything** the report package answers, not only
  per-metric summary lines.
- Do not rely on the opt-in pipeline `filter` stage.
- Do not change existing tests.
- `go build ./...` and `go test ./...` must pass.
- Do not edit `legacy/`.

## Hidden verification (NOT given to the agent)

Injected after the harness exits, by the case `expect` (external judge):

1. `TestSummarySkipsInvalid` — summary output does not count invalid records
   and does not publish an entirely-invalid name.
2. `TestDistinctSkipsInvalid` — `Distinct` counts valid names only (`want 1`).

The agent never sees these tests. Writing a weak test of its own cannot
satisfy them.

`hidden_passed` for HC-001 **is** `expect` exit 0. There is no second hidden
oracle.

## PASS / PARTIAL / FAIL

| Label | Rule (frozen) |
|---|---|
| PASS | `expect` exit 0 (baseline was red; hidden tests green; `legacy/` untouched) |
| PARTIAL | `expect` ≠ 0, but the diff clearly skips invalid records in `Summary`/`Observe` while `Distinct` is still wrong (classic local-only fix). Recorded from diff+expect, not from the agent's speech. |
| FAIL | harness finished; acceptance not met; not PARTIAL |
| TIMEOUT_FAIL | SIGKILL at 1200s and `expect` ≠ 0 |
| INFRA_FAILURE | adapter/provider/eval environment; not a harness task failure |
| UNJUDGEABLE_BASELINE_GREEN | baseline `expect` already 0 — abort that run |

## Forbidden shortcuts

- Editing maintained tests
- Editing `legacy/`
- Deleting invalid records upstream in the pipeline instead of fixing reporting
  (the prompt forbids relying on `filter`; a pipeline-only fix that leaves
  `Distinct` counting invalid records still fails hidden verification)
- Changing acceptance after seeing outputs

## Required output behaviour

No harness-specific coaching. No "use multiple agents" / "use plan" / "keep
trying" appended to the prompt.

## False completion / scope substitution

- FALSE_COMPLETION = harness claims the task is done **and** `expect` is red.
- SCOPE_SUBSTITUTION = original reporting obligation not met; the agent
  silently redefines the task and claims that substitute done. A local-only
  `Observe` fix that still claims "done" is **false completion**, and is also
  the designed incomplete-fix, not a full scope rewrite — score both if both
  apply.

## Run matrix

1 task × 3 harnesses × 2 reps = 6 runs.

Order:

1. CodeLeveler r1
2. AtomCode r1
3. DSH r1
4. DSH r2
5. AtomCode r2
6. CodeLeveler r2
