# Runner

`run.py` loads `evals/configs/<suite>/<experiment>.yaml`, applies CLI overrides,
and writes `evals/reports/<suite>/<experiment>/{batch.json,report.md}`.

It does not inject prompts or eval_mode.
