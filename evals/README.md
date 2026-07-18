# Evaluations

CodeLeveler's evaluation harness runs self-contained repository tasks in
disposable worktrees and checks the result with an independent `expect` command.
Each YAML case defines the task, starting files, and acceptance command.

## Suites

- `smoke/` — small, fast checks for local development and pull requests.
- `core/` — Rust, Go, and TypeScript behavior tasks.
- `hard/` — broader debugging and implementation tasks.

## Run

```sh
# Fast smoke suite
leveler eval run --cases evals/smoke

# Run the core suite with a selected model
leveler eval run \
  --cases evals/core \
  --model deepseek/deepseek-chat \
  --json-out evals/baselines/local-run.json

# Compare two configured models under the same cases
leveler eval compare \
  --cases evals/hard \
  --repetitions 3 \
  provider-a/model-a provider-b/model-b \
  --json-out evals/baselines/local-compare.json
```

Use `leveler eval --help` for ablation and repetition options.

## Result handling

Files under `evals/baselines/` are generated local output and are ignored by
Git. They can contain model identifiers, prompts, repository paths, timestamps,
and diagnostic excerpts; inspect them before sharing.

A meaningful comparison should keep the following fixed:

- CodeLeveler revision and configuration.
- Case directory and case definitions.
- Work mode, permission policy, and verification settings.
- Model endpoint and capability profile.
- Repetition count and machine environment.

Report the full run metadata and failed case identifiers rather than publishing
only a single pass-rate number.

## Adding a case

1. Keep the case self-contained and deterministic.
2. Keep the task statement independent from the expected implementation.
3. Prefer offline acceptance commands.
4. Confirm the case fails before the fix and passes with a known-good
   implementation.
5. Do not commit API keys, local paths, external fixture repositories, or
   generated results.
