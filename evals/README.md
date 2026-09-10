# Evaluations

One eval system, two entry points, one rule: **eval observes the product; it
does not special-case it.** Nothing under `evals/` changes spawn, claim,
ownership, settlement, prompts, or tool schema. There is no `eval_mode`.

A second rule follows from `docs/ARCHITECTURE.md` §1.1: **eval does not reward
model equalization.** Results are reported for the named model and
configuration as observed. A capability limit of that model is not
automatically a harness defect, and closing a gap between two models is not a
result. The questions are whether a runtime or harness change improved the
product, whether a tool improved agency or efficiency, and whether correctness
regressed.

```
evals/
  cases/        what is evaluated: capability cases (EvaluationCase YAML)
  suites/       how each behaviour suite is run: adoption / safety / long-task / multi_agent / …
  configs/      experiment YAML, one file per <suite>/<experiment>
  runner/       observer runner (`leveler eval run --suite …` shells here)
  lib/          EventLog parser, metrics, stats, report, JSON schema (lib/schema/)
  comparative/  CodeLeveler vs other agents (HC-001 / HC-002)
  fixtures/     evaluation repositories and per-obligation oracles
  scripts/      generators, integrity checks, offline analyzers, report tools
  tests/        framework unit tests (offline, no model)
  reports/      generated per experiment (gitignored except README/EXAMPLE)
  runs/         per-batch isolated LEVELER_HOME + batch.json (gitignored)
  baselines/    recorded capability results
```

`fixtures/repos/` holds the repositories cases run against. They are generated
or cloned, never committed: `evals/scripts/fetch_eval_repos.sh` clones the real
ones at pinned refs, and `gen_nav_fixtures.py` / `gen_scale_repos.py` /
`gen_e2_fixture.py` build the synthetic ones deterministically.

## Metrics

Implemented in `evals/lib/metrics.py` and `evals/lib/schema.py`; there is no
separate metrics package.

- adoption rate (spawn | offer seen)
- spawn statistics
- Wilson interval
- mean / median / variance
- compact JSON record (`run_id`, `task`, `model`, `delegation`, `execution`, `safety`)
- MA-VALUE-001 value metrics (`evals/lib/value.py`): task success, efficiency,
  child consumption. Spawn rate is diagnostic only.
- Profile effectiveness (`profile_effectiveness`): per-profile findings / bugs /
  accepted changes. Old EventLogs fall back to `role`.
- Reviewer value (`evals/lib/reviewer.py`): useful findings, verified findings,
  noise. Finding count is not a success metric.

| entry point | question | implementation |
| --- | --- | --- |
| `leveler eval run --cases evals/cases/<suite>` | Capability: did the agent produce a correct tree? Independent `expect`. | Rust, `crates/leveler-eval` |
| `leveler eval run --suite <suite> --experiment <id>` | Behaviour: delegation decision, timing, safety counters, long-task EventLog. | Python, `evals/runner/run.py` |

## Capability cases (`cases/`)

Self-contained repository tasks run in disposable worktrees and checked with an
independent `expect` command. Each YAML case defines the task, starting files,
and acceptance command.

- `smoke/` — small, fast checks for local development and pull requests.
- `core/` — Rust, Go, and TypeScript behavior tasks.
- `hard/` — broader debugging and implementation tasks.
- `navigation/`, `edit/`, `recovery/`, `context/`, `scale/`, `icg/`,
  `realtask/`, `realrepo/`, `scenarios/`, `reviewer*/`, `regression/` —
  focused suites; each directory README states what it discriminates on.

`EvaluationCase::load_dir` walks a directory recursively and requires every
`*.yaml` below it to parse as a case. Keep non-case YAML (configs, suite
pointers, manifests) out of `cases/`.

```sh
# Fast smoke suite
leveler eval run --cases evals/cases/smoke

# Run the core suite with a selected model
leveler eval run \
  --cases evals/cases/core \
  --model deepseek/deepseek-chat \
  --json-out evals/baselines/local-run.json

# Compare two configured models under the same cases
leveler eval compare \
  --cases evals/cases/hard \
  --repetitions 3 \
  provider-a/model-a provider-b/model-b \
  --json-out evals/baselines/local-compare.json
```

Use `leveler eval --help` for ablation and repetition options.

## Behaviour experiments (`suites/`, `configs/`, `runner/`)

```sh
python3 -m unittest discover -s evals/tests -v

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
  --output evals/reports/adoption/m3-baseline

# Adoption micro runner without the framework wrapper
leveler eval adoption-micro run --model deepseek/deepseek-v4-flash --shape parallel
```

Reports land at `evals/reports/<suite>/<experiment>/report.md`.

## Tool surface baseline

`evals/scripts/tool_surface_baseline.py` parses the registry and every
`impl Tool for` block to say what the model can see, then reads the persisted
`tool_call_started` / `tool_call_finished` events in each `LEVELER_HOME`
session database to say what it actually called. It adds no instrumentation
and emits aggregates only.

```sh
python3 evals/scripts/tool_surface_baseline.py --repo . --json out.json
```

The T0 run and the A/B contracts it freezes are in
[`evals/baselines/tool-surface-t0-e623f53/`](baselines/tool-surface-t0-e623f53/README.md).

## Result handling

Files under `evals/baselines/`, `evals/reports/`, and `evals/runs/` are
generated local output. They can contain model identifiers, prompts, repository
paths, timestamps, and diagnostic excerpts; inspect them before sharing.

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
