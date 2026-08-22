# Adoption Micro Eval — decision benchmark

Minute-scale observer for **why the model KEEP vs spawn** after a real
delegation opportunity. Does not change Multi-Agent runtime. KEEP is valid.

First experiment: **task shape only**. No prompt arm. Timing is frozen
(H-C inconclusive).

## Layout

```
eval/micro/adoption/
  tasks/      15 EvaluationCase YAML files (5 parallel, 5 boundary, 5 single)
  runner/     observer runner (isolated LEVELER_HOME + EventLog score)
  metrics/    metric contract
  reports/    EXAMPLE.md + generated reports
  catalog.json   shape + expected disposition (never shown to the model)
```

## Dataset

| shape | n | question |
| --- | ---: | --- |
| parallel | 5 | Does the model treat independent work as worth handing off? |
| boundary | 5 | Split, delay, or KEEP — all can be rational |
| single | 5 | Over-delegation detector |

Prompts never mention spawn or handing work off. The product already injects
the depth-0 hint and the MA-WA1 offer.

`expect: true` is intentional: this suite does not score code quality.

## How to run

```sh
# Full 15-task shape experiment
leveler eval adoption-micro run \
  --provider deepseek \
  --model deepseek-v4-flash \
  --repetitions 1

# One shape (minutes)
leveler eval adoption-micro run --model deepseek/deepseek-v4-flash --shape parallel

# One task
leveler eval adoption-micro run --model deepseek/deepseek-v4-flash --task a01-independent-modules

# Same runner without the CLI wrapper
python3 eval/micro/adoption/runner/run.py run --model deepseek/deepseek-v4-flash --shape single
```

Outputs `batch.json` (full schema + `compact[]` records) and `REPORT.md`
with Dataset / Experiment setup / Metrics / Results / Findings / Next hypothesis.

Offline parser tests (no model):

```sh
python3 -m unittest discover -s eval/tests -v
```

## Compact record

```json
{
  "run_id": "…",
  "task": "a01-independent-modules",
  "shape": "parallel",
  "model": "deepseek/deepseek-v4-flash",
  "offer_seen": true,
  "delegation": {
    "spawn": true,
    "worker_count": 1,
    "decision_round": 8,
    "decision_latency_rounds": 4,
    "disposition": "delegated"
  },
  "execution": { "turns": 12, "edits": 3, "verifier": null },
  "safety": { "violations": 0, "ownership_denied": 0 }
}
```
