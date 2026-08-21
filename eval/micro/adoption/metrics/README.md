# Metrics (decision benchmark)

Implementation lives in `eval/lib/metrics.py` (shared EventLog observer).
This folder is the adoption-suite contract.

| metric | definition | KEEP? |
| --- | --- | --- |
| **Adoption rate** | P(natural spawn \| offer seen, valid) | KEEP is the complement, not a fail |
| **Decision latency** | `decision_round - offer_round` | KEEP and spawn both count |
| **Shape correlation** | the three rates split by parallel / boundary / single | single spawn = P_over |
| **Value** | mean parent turns and edits, spawn vs KEEP, within a shape | cost, not code-quality success |
| spawn_rate | P(spawn \| valid) including never-offered runs | coarser; do not use as the headline |

Natural spawn = `sub_agent_started` role ≠ reviewer, once per child id.
Do not count `tool_call_started name=spawn_agent` (virtual tool, not emitted).
