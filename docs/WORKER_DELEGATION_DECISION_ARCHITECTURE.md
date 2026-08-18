# Worker delegation decision architecture

**Baseline:** `cdc5887f3492` · **Prior candidate:** `8298d35` (not merged) · **Gate:** Worker Delegation Decision Closure (MA-WA1)

Answers the gate's first question: does Main ever form an explicit KEEP/DELEGATE decision before implementation?

## Answer: ABSENT

On both `cdc5887` and `8298d35` there is **no decision point, no disposition state, and no disposition observability**. Worker is available, and Main simply proceeds with implementation unless it spontaneously remembers to spawn. KEEP is never *chosen* — it is what happens by default. Concretely:

- The only delegation input the model ever receives is a **round-1 user message** (`multi_agent_steer_hint`, keyword-gated on `cdc5887`, unconditional at depth 0 on `8298d35`). It arrives before the model knows anything about the task, and is never re-anchored at the moment decomposition becomes known.
- Nothing in the harness marks the moment "decomposition known, implementation not yet committed" — even though that moment **already exists structurally**: the plan gate (`gates::plan_gate`) forces complex tasks to register a ModelExplicit plan via `update_plan` before any mutation.
- No durable event records whether delegation was evaluated, kept, or taken. `SubAgentStarted` records spawns; the KEEP path leaves zero trace. A run with 0 spawns is indistinguishable from a run where delegation was never representable.

This matches the Probe #2 (xsv) evidence: policy visible from round 1, first effective edit round 18, 112 rounds, 42 files, 0 spawns. The model passed through the commitment boundary (rounds ~15–18) with the delegation policy 17 rounds behind it and nothing at that boundary re-presenting the choice.

## Decision path today (depth-0 Main)

```
Frozen Goal
  → Task contract injection (round 0, user msg)
  → [8298d35: keep-vs-delegate hint, round 0, user msg]   ← only delegation touchpoint
  → explore rounds (nav tools open; soft plan nudge after 2 rounds)
  → plan gate: complex task ⇒ update_plan required before mutation
      └── update_plan success: plan_state=ModelExplicit, PlanUpdated event
          ← DECOMPOSITION NOW KNOWN — no delegation semantics attached (defect)
  → first mutation (note_tool_side_effects → EvidenceLedger)
      ← IMPLEMENTATION COMMITMENT — no delegation semantics attached (defect)
  → … implementation … (spawn_agent stays in catalog, never re-anchored)
  → completion (update_goal gates)
```

Already ruled out by prior evidence (MA-PROBE-01/02/03): tool visibility, capability visibility, keyword gating of the *tool*, admission failure, too-late initial guidance.

## Delegation opportunity model (product concept, model-judged)

A delegation opportunity exists when the current task contains a subtask with all of: meaningful implementation value; bounded goal; explicit safe file/module scope; enough independence; a useful result contract; benefit plausibly exceeding overhead. The harness does **not** classify this statically — it only guarantees the model is asked exactly once at the moment the model's own decomposition makes the judgment possible.

## Repair design: one decision point + behavioral disposition

### Decision point (fires at most once per goal epoch)

Eligibility: `depth == 0 ∧ allow_delegation`. Trigger, whichever comes first:

- **T-plan:** first successful ModelExplicit `update_plan` with more than one incomplete step — the model has just written its own decomposition.
- **T-mutation (fallback, `mutation_fallback`):** second mutating round with no structured plan registered — deliberately one mutating round late, so a lone prep edit followed by `update_plan` still yields the plan-anchored per-step form.

Effect: inject **one** neutral user message (same mechanism as scoped-rule/contract injections): assess whether any registered step is a bounded independent workstream; if yes, `spawn_agent role='worker'` with self-contained task + exact owned files; if no, keep everything and continue — KEEP is fully valid; this will not be asked again. No forbidden directives ("prefer Worker", "delegate when possible", thresholds, keywords). No tool is stripped, no call is blocked, no reply is required.

Nag control: `ProgressLedger.delegation_decision_offered: bool` (serde-default, persisted via existing `ProgressUpdated`), so continue/resume windows do not re-ask. No re-offer within a run.

### Disposition capture (observable behavior, not self-report)

No new tool, no hidden reasoning. The harness derives and durably records the outcome as a `DelegationStage { action, detail }` event (ReviewStage pattern: persisted, LocalOnly, no public projection):

- `offered` — decision point fired (`detail` = trigger: plan | mutation_fallback)
- `delegated` — first Worker admitted after spawn admission (`detail` = scope)
- `kept` — first successful mutation *after* the offer with zero Workers spawned so far

`kept` is recorded once and is a first-class outcome, not a failure. A later Worker spawn after `kept` still records `delegated` (the disposition history is append-only facts, not a state machine).

### What this repair is NOT

No planner agent, no scheduler, no task DAG, no second runtime/EventLog/ToolHost, no keyword matrix, no file-count rules, no always-spawn, no weakening of Worker scope/admission/depth-1, no change to Reviewer/Explorer, no completion semantics (a disposition neither completes work nor blocks anything).

## Layer separation after repair

| Layer | State |
| --- | --- |
| Capability visibility | unchanged (already PASS) |
| Worker execution/safety/settlement | unchanged (READY) |
| Delegation judgment | one host-guaranteed decision point + durable disposition facts |
| Execution of a DELEGATE | existing spawn_agent → ChildProfile → ToolHost path, unchanged |
