# Micro timing

Causal test of **when** the keep-vs-delegate surface is raised.

The product already exposes this as `agents.offer_timing`:

| arm | config | behaviour |
| --- | --- | --- |
| control | unset (default) | offer at plan registration |
| treatment | `offer_timing = "after_first_edit"` | hold offer until a mutating edit tool succeeds |

This directory has no separate tasks. Use the adoption suite:

```sh
python3 evals/scripts/run_micro.py run --model M --arm control
python3 evals/scripts/run_micro.py run --model M --arm timing.after_first_edit
python3 evals/scripts/compare_arms.py evals/runs/<a>/batch.json evals/runs/<b>/batch.json
```

The isolated `LEVELER_HOME/config.toml` is the only difference. Wording, tool
schema, ownership, claim, and settlement stay byte-identical.

Long-task H-C (LONG_A/B/C, 12 runs/arm) remains the confirmatory experiment
under `evals/suites/long-task/`. Run micro first; promote only if the spawn-rate
delta is large enough to spend the hours.
