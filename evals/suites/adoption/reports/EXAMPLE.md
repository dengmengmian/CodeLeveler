# Adoption Micro Eval Report

SYNTHETIC example — not a live model run. Generated from fixture EventLogs so
the report shape is reviewable without a provider.

KEEP is a first-class outcome. This report does not treat KEEP as a failure.
Natural spawn = `sub_agent_started` with role ≠ reviewer, once per child id.

## 1. Dataset

- batch: `synthetic-shape-demo`
- n runs: 15 (one per task)
- n with offer seen (valid): 15

| task | shape | expected | n | spawn | keep |
| --- | --- | --- | ---: | ---: | ---: |
| `a01-independent-modules` | parallel | spawn | 1 | 1 | 0 |
| `a05-three-cli-commands` | parallel | spawn | 1 | 1 | 0 |
| `a09-two-cli-tools` | parallel | spawn | 1 | 0 | 1 |
| `a10-sort-and-search` | parallel | spawn | 1 | 1 | 0 |
| `a12-parse-and-count` | parallel | spawn | 1 | 0 | 1 |
| `a04-parser-and-goldens` | boundary | either | 1 | 0 | 1 |
| `a06-typed-pipeline` | boundary | either | 1 | 0 | 1 |
| `a07-format-and-readme` | boundary | either | 1 | 1 | 0 |
| `a08-new-module-plus-glue` | boundary | either | 1 | 0 | 1 |
| `a13-helper-and-callers` | boundary | either | 1 | 0 | 1 |
| `a02-shared-record-exporters` | single | keep | 1 | 0 | 1 |
| `a03-single-file-keep` | single | keep | 1 | 0 | 1 |
| `a14-off-by-one` | single | keep | 1 | 0 | 1 |
| `a15-clamp` | single | keep | 1 | 0 | 1 |
| `a16-match-arm` | single | keep | 1 | 0 | 1 |

## 2. Experiment setup

- factor: **task shape** (parallel / boundary / single)
- prompt arm: none (product default coordinator hint + MA-WA1 offer only)
- model: `example/model` (synthetic)
- arm: `control` (task_shape=product_default)
- runtime: unchanged spawn/claim/ownership/settlement

## 3. Metrics

Primary: **adoption rate** = P(spawn | offer seen, valid).
Secondary: decision latency, shape correlation, value (parent turns/edits).
Micro `expect` is `true` — value is cost, not code-quality success.

## 4. Results

- adoption rate: 27% (4/15)
- KEEP given offer: 11
- mean decision latency: 3.0

### Task shape correlation

| shape | offer seen | spawn | KEEP | adoption | mean latency |
| --- | ---: | ---: | ---: | ---: | ---: |
| parallel | 5 | 3 | 2 | 60% | 4 |
| boundary | 5 | 1 | 4 | 20% | 3 |
| single | 5 | 0 | 5 | 0% | 2 |

### Value (parent cost, not success)

- **parallel**: spawn mean turns 11 vs KEEP 18 (synthetic)
- **boundary**: spawn mean turns 14 vs KEEP 16
- **single**: no spawn rows

- P_over (KEEP-labelled spawn): 0% (0/5)

## 5. Findings

- Parallel adoption 60% vs single 0% in this fixture. The interesting KEEP
  cases are the two parallel tasks that did not spawn (`a09`, `a12`) — those
  are the DELEGATION_ADOPTION question, not a runtime block.
- Single-shape KEEP 5/5 is the over-delegation control holding.
- n=1 per task is illustrative only (`insufficient_n` for a real verdict).

## 6. Next hypothesis

Do not change offer timing (H-C was inconclusive). After a live `--shape parallel`
batch, compare KEEP transcripts on a09/a12-class tasks: coupling story vs
overhead story vs ignored offer. Then, and only then, a prompt SHA A/B.
