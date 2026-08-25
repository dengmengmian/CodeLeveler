# Metrics

Implementation: `eval/lib/metrics.py`, `eval/lib/schema.py`.

- adoption rate (spawn | offer seen)
- spawn statistics
- Wilson interval
- mean / median / variance
- compact JSON record (`run_id`, `task`, `model`, `delegation`, `execution`, `safety`)
- MA-VALUE-001 value metrics (`eval/lib/value.py`): task success, efficiency, child consumption. Spawn rate is diagnostic only.
- Profile effectiveness (`profile_effectiveness`): per-profile findings / bugs / accepted changes. Old EventLogs fall back to `role`.
- Reviewer value (`eval/lib/reviewer.py`): useful findings, verified findings, noise. Finding count is not a success metric.
