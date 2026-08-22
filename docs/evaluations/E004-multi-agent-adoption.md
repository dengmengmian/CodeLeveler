# E004 — Multi-Agent adoption micro evaluation

**Status:** landed as observer infrastructure. Does not change MA-WA1 runtime.
**Suite:** `eval/micro/adoption/`
**Estimand:** adoption rate = P(natural spawn | offer seen, valid). KEEP is first-class.

## Why this exists

Formal LONG_A/B/C runs take hours and mix completion with adoption. After
ownership/settlement closed, the remaining product question is whether the
model *takes* the decision surface. That question needs a 10–20 minute loop.

## Decision surface (product, unchanged)

Durable events already exist (`DelegationStage`, `SubAgentStarted`). The
extractor counts natural spawn from `sub_agent_started` (role ≠ reviewer),
not from `tool_call_started` named `spawn_agent` (that tool is virtual and
does not emit a host tool-call event).

Validity: plan registration ≥ 1 **or** parent mutation ≥ 1. A never-engaged
run is excluded from the adoption denominator.

## Protocol

1. Frozen tasks (15): 5 parallel, 5 boundary, 5 single. `max_rounds` 12–18. No delegation vocabulary in the prompt.
2. Isolated `LEVELER_HOME`. User `config.toml` is copied in; `~/.leveler` is not written.
3. First experiment: **task shape only**. No prompt arm. Do not re-run H-C timing.
4. Primary: adoption rate given offer; also decision latency, shape correlation, parent-cost value. n<6 → `insufficient_n`.
5. Single-shape spawn is P_over. KEEP on parallel is the adoption question, not a fail.

CLI: `leveler eval adoption-micro run --model … --shape parallel`.

Promote to LONG_A/B/C only when micro shows a movement large enough to spend hours.

## What this is not

Not a fix for missed reconsideration. Not a second orchestrator. Not
`LEVELER_EVAL_COMMITMENT_NUDGE`. Prompt A/B is two checkouts of the product.
