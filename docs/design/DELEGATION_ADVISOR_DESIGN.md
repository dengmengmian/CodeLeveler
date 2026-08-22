# Delegation Advisor — Design

**Status:** design only. Nothing here is implemented, and nothing here should be
implemented before the Beta gate closes.
**Motivating evidence:** `docs/evaluations/MA-WA1-FINAL.md`.

## Why this exists

Eight hypotheses were eliminated. The model reaches the decision surface every
time (offer coverage 54/54 among engaged runs), understands the task well
enough to complete it (`deepseek-v4-pro`: 10/12 verifier), and still delegates
about a quarter of the time. Raising the model's coding capability changed the
delegation rate by exactly zero (1/12 → 1/12, p = 1.00).

The reading that survives: the model is asked to make a judgement —
*"is some of this separable, and is it worth handing off?"* — from a general
availability notice, while the runtime already holds the structured facts that
judgement needs and never shows them.

The Advisor's job is to close that gap **without taking the decision away**.

## What this is not

- **Not forced delegation.** No `ToolChoice::Required`, no auto-dispatch, no
  planner that spawns on the model's behalf.
- **Not a threshold rule.** No `if files > N`, no keyword matching, no task-shape
  heuristics. Those were the first thing eliminated and they encode exactly the
  assumption the ladder disproved.
- **Not prompt steering.** The goal is not to talk the model into spawning.
- **Not a second orchestrator.** It reuses the existing work-item, plan and
  ownership machinery; it introduces no scheduler and no second event log.
- **Not a spawn-rate optimiser.** See §Success criteria: an Advisor that raises
  spawn rate while raising *unnecessary* delegation has failed.

## Design goal

> Move the runtime from **announcing that delegation exists** to **proposing a
> specific delegation the model can accept or decline** — and measure whether
> the resulting decisions are *better*, not whether there are more of them.

## Architecture

```
        Agent loop
            │
            │  work items · plan · dependency graph · edit history
            ▼
   ┌────────────────────┐
   │ Task Understanding │   pure analysis over state the runtime already has
   └────────┬───────────┘
            │  TaskAnalysis  { parallelizable, work_items[], delegation_score }
            ▼
   ┌────────────────────┐
   │ Delegation Proposal│   at most one live proposal; model accepts or declines
   └────────┬───────────┘
            │  accepted → an ordinary spawn_agent call, unchanged
            ▼
   ┌────────────────────┐
   │ SubAgentProvider   │   existing path: read-only start → claim_write_scope
   └────────┬───────────┘
            ▼
        Child agent
```

Everything below the proposal boundary is today's shipped path and is not
touched: `spawn_agent`'s schema, `claim_write_scope`, the ownership registry,
settlement, provenance.

## 1. Task Understanding Layer

Pure function over durable runtime state. No model call, no new persistence.

**Inputs** — all already tracked:

| input | source today |
| --- | --- |
| registered plan and its open/completed steps | `plan_updated` / `PlanState` |
| files modified so far, and by whom | `ProgressLedger.cumulative_modified_paths` |
| live ownership claims | ownership registry |
| files read/searched and their directories | tool-call history |
| outstanding children | `ProgressLedger.outstanding_children` |
| verification outcomes | `EvidenceLedger` |

**Output** (illustrative shape, not a wire format):

```json
{
  "parallelizable": true,
  "work_items": [
    { "id": "wi-1", "label": "encoder_jsonl.go",  "paths": ["pkg/yqlib/encoder_jsonl.go"],  "blocked_by": [] },
    { "id": "wi-2", "label": "encoder_markdown.go", "paths": ["pkg/yqlib/encoder_markdown.go"], "blocked_by": [] },
    { "id": "wi-3", "label": "format registration", "paths": ["pkg/yqlib/format.go"], "blocked_by": ["wi-1", "wi-2"] }
  ],
  "independent_count": 2,
  "delegation_score": 0.82,
  "rationale": ["2 open plan steps touch disjoint path sets", "no live claim on either"]
}
```

**Independence is derived from paths and plan structure**, not from language.
Two work items are independent when their path sets are disjoint, neither is
`blocked_by` the other, and neither is under a live ownership claim. That
definition is mechanical and auditable — which matters, because a scoring
function nobody can check is how heuristics smuggle themselves back in.

`delegation_score` is a **reported** quantity, never a gate. It exists so the
eval can ask "was the Advisor right?" — not so the runtime can act on it.

### Open design questions

- Do plan steps map to path sets reliably enough, or does the analyzer need the
  model to declare paths per step? The second is a schema change and would need
  its own justification.
- How is a work item retired — on the first edit inside its paths, on a
  completed plan step, or on settlement?
- What happens when the plan is absent? Roughly half of engaged runs never
  register one, so an analyzer that requires a plan is inert exactly where the
  historical spiral lived.

## 2. Delegation Proposal

**Shape.** A factual, specific, declinable statement. Illustrative:

> Three open steps touch disjoint files: `encoder_jsonl.go`,
> `encoder_markdown.go`, `encoder_html.go`. None is claimed. Each could go to a
> background worker now; `format.go` depends on all three and should stay here.

Contrast with today's notice, which says delegation is available and leaves the
decomposition to the model.

**Rules, all inherited from what the current mechanism already got right:**

- **At most one live proposal.** A proposal that has not been acted on is not
  repeated.
- **Declining is first-class and silent.** Not spawning is already the decline;
  it needs no announcement. KEEP must remain as cheap as it is today.
- **Superseded by facts, not by time.** A new proposal only when the independent
  work-item set materially changes — the fingerprint/latch discipline the
  existing reconsideration uses, which measured 0 duplicate offers across 54
  engaged runs.
- **No disposition demanded.** The prompt-ceremony regression that an earlier
  round removed must not return.
- **Never contradicts ownership.** A proposal must not name a path under a live
  claim; the registry is the authority.

## 3. Integration with the current runtime

| layer | change |
| --- | --- |
| Task Understanding | **new**, pure, no persistence |
| Proposal injection | replaces the *content* of the existing offer; reuses its trigger, latch and dedup machinery |
| `spawn_agent` | **unchanged** — an accepted proposal is an ordinary call |
| `claim_write_scope`, ownership, settlement, provenance | **unchanged** |
| EventLog | one new durable record: proposal issued, with its work items, so eval can score it |

The proposal is injected where the offer is injected today. That placement is
deliberate: the offer's timing was tested and is not the problem, so the Advisor
inherits it rather than re-litigating it.

## 4. Eval strategy

**Spawn rate is not the metric.** The metric is whether the decision was right.

For each proposal, the ground truth is derivable after the fact from the same
event log:

| outcome | definition |
| --- | --- |
| **appropriate delegation** | proposed, accepted, child produced useful work inside its claimed scope, no rework |
| **unnecessary delegation** | proposed and accepted, but the work was trivial or coupled — child output discarded, redone, or scope-conflicted |
| **missed delegation** | independent work existed and was not proposed, or was proposed and declined, and the run then ran out of budget on work a child could have absorbed |
| **correct KEEP** | independent work did not exist, or existed and was cheaper to do inline — and the run completed |

```
appropriate_delegation_rate = appropriate / (appropriate + unnecessary + missed)
```

Reported alongside, never traded against: verifier pass, completion, wall-clock
latency, token cost, safety counters.

**Acceptance shape.** An Advisor ships only if `appropriate` rises while
`unnecessary` stays at zero and verifier does not fall. A design that raises
delegation and lowers task success is a regression regardless of spawn rate.

**Power, from measured variance.** Adoption on a fixed task varies 0.08–0.30
across batches. Any Advisor A/B must be powered against that: at these rates,
twelve runs per arm resolves nothing short of a tripling. Compute the detectable
effect **before** the runs — the mistake every gate in this programme made until
Gate V2.

**Baseline is already collected.** The frozen `long-b` task has ~100 recorded
runs and the difficulty ladder gives four rungs. A future Advisor arm compares
against them, so the control does not need re-running from scratch — though a
same-day control arm is still required, because cross-day comparison confounds
provider drift.

## 5. Risks

| risk | why it matters | mitigation |
| --- | --- | --- |
| The analyzer becomes a heuristic engine | path-and-plan independence quietly grows special cases until it is `if files > N` | independence is defined mechanically and its inputs are auditable; any addition needs its own evidence |
| Proposals become a nag | the ceremony regression an earlier round removed | one live proposal, fingerprint dedup, silent decline — the same discipline that produced 0 duplicate offers in 54 runs |
| Proposals contradict ownership | a proposal naming a claimed path is a bug that looks like advice | registry is authoritative; proposals are filtered against live claims |
| Advisor raises spawn but lowers quality | the failure this design exists to avoid | acceptance requires `unnecessary = 0` and no verifier regression |
| It does not work either | plausible — eight hypotheses have already failed | it is falsifiable on `appropriate_delegation_rate`; a null result argues for Option A in the final report, and that is a legitimate outcome |

## 6. What would falsify this design

If an Advisor presents specific, correct, path-level proposals and the model
still declines at the same rate, then the limit is not informational, and
model-elected delegation should be re-examined as an architecture rather than
improved as a feature. That outcome is worth the experiment precisely because it
is decisive either way.
