# Suite: multi_agent

Value evaluation, not adoption. MA-WA1 already showed the runtime is safe
and that spawn frequency ≠ value. This suite asks whether allowing spawn
improves real coding tasks relative to a single-agent control.

Experiment: `MA-VALUE-001` (`eval/configs/multi_agent/MA-VALUE-001.yaml`).
Cases: pointers to Real Usage R005–R010 under `multi_agent_value/cases/`.
Methodology: `docs/evaluations/MA-VALUE-001.md`.

```sh
leveler eval run --suite multi_agent --experiment MA-VALUE-001 --mode single
leveler eval run --suite multi_agent --experiment MA-VALUE-001 --mode multi
```

`--mode single` writes the shipped `agents.delegation = false` key into an
isolated `LEVELER_HOME`. `--mode multi` uses the product default. Neither
path changes spawn lifecycle, child runtime, tool schema, or prompts.

Reviewer value (pilot): `MA-VALUE-REVIEWER-PILOT`. `--mode self` /
`--mode reviewer` writes `agents.independent_review`. Finding count is not
a success metric. See `reviewer_value/`.
