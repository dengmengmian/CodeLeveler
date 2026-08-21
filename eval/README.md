# CodeLeveler Eval Framework

Observer layer over the product runtime. It does not change spawn, claim,
ownership, settlement, or orchestration policy. There is no `eval_mode`.

```
eval/
  lib/           EventLog parser, spawn metric, schema, stats, reports
  scripts/       run_micro, score_eventlog, compare_arms, generate_report
  schema/        unified JSON schema
  micro/         minute-scale: adoption, timing, tool-usage
  long-task/     LONG_A/B/C pointers + wrapper (hours)
  safety/        ownership / sandbox / permission pointers
  tests/         offline unit tests (no model)
  runs/          generated (gitignored)
  reports/       generated (gitignored)
```

Existing `evals/` + `crates/leveler-eval` remain the **capability** harness
(code correctness, expect commands). This tree is the **behaviour / adoption /
safety** harness on top of EventLog. Do not merge the two.

## Gates (map)

| id | name | where |
| --- | --- | --- |
| E001 | Agent basic capability | `evals/smoke`, `evals/core` |
| E002 | Tool usage | `eval/micro/tool-usage` → still `evals/smoke` |
| E003 | Long-task completion | `eval/long-task` + frozen verifiers |
| E004 | Multi-agent adoption | `eval/micro/adoption` |
| E005 | Ownership safety | `eval/safety/ownership` |
| E006 | Browser | not in this tree |
| E007 | Recovery / resume | `evals/recovery` |
| E008 | Multi-agent collaboration | FUTURE, after E004 moves |

## How to tell if a change moved spawn

Hold the product default. Vary **one** factor. Compare batches with Fisher
exact on spawn vs no-spawn among valid engaged runs. n<6 per arm →
`insufficient_n`, not a story.

| question | control | treatment |
| --- | --- | --- |
| Did a **prompt** change move spawn? | git SHA A, `--arm control` | git SHA B, `--arm control` |
| Did **timing** move spawn? | `--arm control` | `--arm timing.after_first_edit` |
| Did **model/provider** move spawn? | `--model M1` | `--model M2` |

Timing uses the shipped `agents.offer_timing` knob in an isolated
`LEVELER_HOME`. It is not an eval-only branch in the runtime.

```sh
python3 -m unittest discover -s eval/tests -v

leveler eval adoption-micro run --provider deepseek --model deepseek-v4-flash --shape parallel
python3 eval/micro/adoption/runner/run.py run --model deepseek/deepseek-v4-flash --repetitions 1
python3 eval/scripts/compare_arms.py eval/runs/<a>/batch.json eval/runs/<b>/batch.json \
  --md eval/reports/cmp.md --csv eval/reports/cmp.csv
```

## Non-goals

- Fixing adoption
- Injecting extra coordinator text
- Replacing `leveler eval run` for capability cases
- Vendoring miller/yq/xsv or hidden verifiers
