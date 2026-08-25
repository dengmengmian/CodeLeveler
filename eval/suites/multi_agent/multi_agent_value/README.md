# multi_agent_value

MA-VALUE-001 working tree. Layout matches the eval framework: experiment
YAML stays at `eval/configs/multi_agent/` so `leveler eval run --suite`
resolves it; this directory holds pointers, arm overlays, and methodology.

| path | what |
| --- | --- |
| `cases/` | R005–R010 pointers. Not `EvaluationCase` YAML. No model-visible task. |
| `configs/` | Arm overlays (`single` / `multi`) using the shipped `agents.delegation` key |
| `methodology/` | Pointer to `docs/evaluations/MA-VALUE-001.md` |
| `reports/` | Local notes. Generated batches land in `eval/reports/multi_agent/` |

Do not vendor the third-party repositories or hidden verifiers.
