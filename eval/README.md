# CodeLeveler Eval Framework

Observer layer. No `eval_mode`. Does not change spawn, claim, ownership,
settlement, prompts, or tool schema.

```
eval/
  configs/     experiment YAML (M-3 baseline, M-2 budget stub, …)
  runner/      unified runner
  metrics/     metric contract (code in lib/)
  reports/     generated per experiment
  suites/      pointers: adoption / capability / safety / multi_agent
  lib/         EventLog parser, stats, schema
  micro/       adoption task YAML (kept in place for compatibility)
  tests/
```

Capability cases stay in `evals/` (plural). Methodology: `docs/eval-methodology.md`.

## Run

```sh
python3 -m unittest discover -s eval/tests -v

# M-3 baseline (task shape, product default)
leveler eval run --suite adoption --experiment m3-baseline

# Multi-agent value (R005–R010, isolated home, no runtime change)
leveler eval run --suite multi_agent --experiment MA-VALUE-001 --mode single
leveler eval run --suite multi_agent --experiment MA-VALUE-001 --mode multi

# Independent Reviewer vs self-verify (pilot, isolated home)
leveler eval run --suite multi_agent --experiment MA-VALUE-REVIEWER-PILOT --mode self
leveler eval run --suite multi_agent --experiment MA-VALUE-REVIEWER-PILOT --mode reviewer

# Overrides
leveler eval run \
  --suite adoption \
  --experiment m3-baseline \
  --provider deepseek \
  --model deepseek-v4-flash \
  --runs 3 \
  --output eval/reports/adoption/m3-baseline

# Compatibility
leveler eval run --cases evals/smoke
leveler eval adoption-micro run --model deepseek/deepseek-v4-flash --shape parallel
```

Reports land at `eval/reports/<suite>/<experiment>/report.md`.
