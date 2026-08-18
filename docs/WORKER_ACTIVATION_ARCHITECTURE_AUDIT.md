# Worker activation architecture audit

**Authoritative baseline:** `cdc5887f3492`  
**Candidate #1:** `8298d35` on `fix/worker-natural-activation`  
**Scope:** whether Main can *see*, *understand*, *decide*, *request*, and *admit* a Worker. Not Worker execution mechanics.

This document does not change runtime contracts. It records the decision path.

## Decision path (top-level Main)

```
Application.allow_delegation
  (project.agents.delegation ∧ global agents_delegation; both default true)
        ↓
TurnPolicy.allow_delegation (default true)
        ↓
drive.rs tool catalog:
  spawn_agent advertised iff allow_delegation ∧ depth < 1
        ↓
model-visible spawn_agent schema + description
        ↓
optional user injection: multi_agent_steer_hint
        ↓
model chooses spawn_agent | KEEP
        ↓
admission: non-empty task, ChildProfile::admit(role, files),
           same-batch overlap, max_total_agents=6
        ↓
Worker: requires_scope=true, write allowlist, serial tools
```

Eval (`leveler eval`) uses the same Application path. `EvalApprove` does not hide `spawn_agent`.

## 1. Can Main see `spawn_agent`?

| Gate | Baseline `cdc5887` | Candidate `8298d35` |
| --- | --- | --- |
| Config kill-switch | yes, default on | unchanged |
| Depth | hidden at depth ≥ 1 | unchanged |
| Keyword heuristic | **does not** hide the tool | unchanged |
| Eval catalog | advertised from round 1 | advertised from round 1 |

`task_suggests_delegation` never controlled tool visibility.

Production proof (MA-PROBE-01): tool-call catalogs on both RED and GREEN contain `spawn_agent` in the advertised set (context snapshots include the tool definition). **Not TOOL_VISIBILITY_FAILURE.**

## 2. Can Main understand Worker?

Schema (both SHAs): `role` enum `default | explorer | worker`; Worker `files` are exclusive write scope.

Description:

| SHA | Lead framing |
| --- | --- |
| `cdc5887` | “do this when the **user asks** for parallel/multi-agent work” |
| `8298d35` | prefer bounded independently-verifiable scoped writes; KEEP when coordination costs more |

Reviewer is **not** in the model-facing enum (`AgentRole::parse` rejects `"reviewer"`). Unchanged.

On `cdc5887` this is **CAPABILITY_UNDERSTANDING_FAILURE** (partial) + **DECISION_POLICY_FAILURE** (keyword-gated hint never injected).

On `8298d35` Worker scoped write + targeted verification is model-visible. Remaining KEEP on hyperfine is **not** an understanding failure.

## 3. Decision policy

`task_suggests_delegation` is a **keyword heuristic** (parallel / 分别改 / two facets + implement).

| SHA | Hint injection |
| --- | --- |
| `cdc5887` | `allow ∧ depth=0 ∧ task_suggests_delegation(goal)` — **hidden capability-adjacent gate for guidance** |
| `8298d35` | `should_inject_delegation_hint(allow, depth)` = `allow ∧ depth=0` — keywords **do not** gate |

After `8298d35`, `task_suggests_delegation` has **no remaining critical-path use** (export + unit tests only). Soft leftover, not a gate.

Guidance is injected **once**, as a user message, **before** the first model round (same place as Task contract). First-edit on the GREEN hyperfine run was round 48; hint was present from round 1. **Not a timing defect.**

Children (`depth > 0`) do not get the hint and cannot spawn.

## 4. Constructing a valid Worker request

Required: `task` (non-empty). Worker additionally requires `files: […]`.

Honest denial if:

- Worker with empty `files`
- Explorer/Reviewer with `files`
- same-batch overlapping Worker scopes
- `agents_spawned ≥ 6`
- unknown named `agent`

No silent downgrade. **MA-PROBE-01 recorded zero spawn attempts**, so **not ADMISSION_FAILURE**.

## 5. Admission vs execution

Unchanged on `8298d35` and out of this gate’s repair surface:

- `ChildProfile::Worker`: `requires_scope`, serial tools, not read-only
- write allowlist + shell snapshot rollback
- overlap refuse
- depth = 1, concurrency = 4, total = 6

## 6. Plan phase

`require_explicit_plan` is a **soft nudge** after two explore rounds (`PLAN_SOFT_NUDGE_AFTER_ROUNDS = 2`). It does not strip `spawn_agent` and does not force `ToolChoice`. Delegation can be chosen before any plan.

## 7. Compaction

Context snapshots persist the system/tool catalog and the injected user hint. GREEN hyperfine: `keep_vs_delegate_visible` on durable events. Compaction did not drop the policy.

## 8. Failure classification of existing evidence

| Run | Class |
| --- | --- |
| Mini-Batch A–D, `cdc5887` | B + C (wording + keyword-gated hint) |
| MA-PROBE-01 RED `cdc5887` | C (hint not injected); B residual |
| MA-PROBE-01 GREEN `8298d35` | **E MODEL_DECISION** on a small coupled exporter — candidate **VALID_KEEP**, not a remaining Harness gate |

## 9. What `8298d35` is and is not

It only:

- un-gates top-level keep-vs-delegate guidance
- clarifies Worker scoped write + targeted verification
- adds tests for those two facts

It does **not** force spawn, change Worker/Reviewer/Explorer contracts, weaken safety, add a scheduler, or add a keyword matrix.

It is **candidate #1**, not an authoritative baseline. MERGE / MODIFY / DROP waits on Probe #2 (Worker-worthy) + negative control.

## 10. What this gate must still prove

Hyperfine KEEP after the policy is visible does **not** fail the candidate. Closing `MA-WA1` requires a **different**, genuinely separable real task to DELEGATE with scoped write + integration, and a small task to KEEP.
