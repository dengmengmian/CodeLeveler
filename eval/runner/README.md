# Runner

`run.py` loads `eval/configs/<suite>/<experiment>.yaml`, applies CLI overrides,
and writes `eval/reports/<suite>/<experiment>/{batch.json,report.md}`.

It does not inject prompts or eval_mode.
