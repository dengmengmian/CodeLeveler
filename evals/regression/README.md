# Regression suite

Failed or flaky acceptance cases live here so they re-run on a **dedicated
entry point** without inventing a second harness.

## Entry point (real path)

```sh
# From repo root, with a configured provider:
./target/debug/leveler eval run --cases evals/regression \
  --model deepseek/deepseek-v4-pro \
  --json-out evals/history/regression-<gitsha>.json
```

Same runner as `quick` / `daily` / `release` (`leveler eval` → `run_eval`).

## When to add a case

1. A real-path acceptance run fails (expect fail, false completion, loop, or
   incomplete-with-bad-outcome).
2. Copy or promote the YAML here (keep a stable `id`).
3. Prefer cases that **fail without the fix** and pass with a known-good agent
   or after the product fix.

Do **not** duplicate-load this directory inside `daily`/`release` while the same
`id` still lives under `core`/`hard` — `EvaluationCase::load_dir` rejects
duplicate ids *within* one load, and tier loads merge directories. Run
regression as its own gate after fixing P0/P1.

## Current seeds (2026-07-24 baseline)

Ids use a `reg-` prefix so recursive `evals/` loads stay unique (core keeps the
original ids; this suite is a **standalone** gate).

| Case id | Why |
|---------|-----|
| `reg-go-batch-boundaries` | daily partial: incomplete / runtime while expect green |
| `reg-go-copy-map` | daily partial: incomplete |
| `reg-go-normalize-email` | daily partial: failed |
| `reg-recovery-compile-fail` | recovery path must stay green |

These mirror known weak points; promote more failures as they appear.
