# Multi-Agent V2 — CodeLeveler current state audit

**Baseline:** `a7ee9b2d919c` (includes the delegation decision layer, reviewer diff-injection, rollback-test determinism). Facts below are verified against this tree and against the production probes run on its candidates.

## Runtime primitives (what exists and works)

| Surface | State | Evidence |
| --- | --- | --- |
| `spawn_agent` tool | Advertised at depth 0 whenever `allow_delegation` (kill-switch honored); roles default/explorer/worker; named personas | drive.rs catalog; probes: present in every catalog snapshot |
| Admission | Honest denials: empty task, empty Worker scope, read-only role with scope, same-batch scope overlap, cap (6 total / 4 concurrent), unknown persona, depth | `ChildProfile::admit`, drive.rs spawn batch |
| Worker safety | Exclusive write scope (file or directory, trailing-slash-normalized), write-allowlist enforcement pre-execution, serial writer tools, shell rollback | authorization.rs, probes |
| Explorer / Reviewer | Structurally read-only registries; Reviewer harness-launch only (`AgentRole::parse` rejects "reviewer"), 20-round cap | sub_agent.rs, handlers.rs |
| Child results | `ChildResult` four-way truth (completed/no-findings/partial/no-result), partial preservation, findings adoption + lifecycle, blocking findings gate Verified | R007b N1 machinery |
| Delegation decision layer | One-shot neutral KEEP/DELEGATE point (plan-anchored per-step form; deferred mutation fallback); durable `DelegationStage offered/kept/delegated`, one fact per goal epoch across windows | merged 7ed3f2d, production-proven on 10+ runs |
| Reviewer brief | Now embeds capped unified diff + untracked list + bounded conclusion contract | merged a7ee9b2, NOT yet production-revalidated |
| EventLog | All child/delegation facts durable (SubAgentStarted/Finished, ReviewStage, DelegationStage), replay-safe, LocalOnly classes | engine/event.rs |

## The load-bearing gap (verified in drive.rs, not inferred)

**`spawn_agent` is synchronous at the round granularity.** All spawn calls of one assistant turn run as a batch; the parent's model loop `await`s the whole batch and folds every child result into that same tool round (`drive.rs` spawn section: `futs.next()` drained before `tool_message` is built). Consequences:

1. **Parent cannot continue useful work while a child runs.** The "delegation frees Main to do X" value proposition is structurally absent — a rational model correctly perceives delegation as *blocking* its own trajectory for the child's wall time and adding integration cost. Every rational-KEEP rationale we recorded (miller, B′, pro diagnostic: "no latency win", "coordination cost") is a *correct* reading of today's runtime.
2. **No cross-round child identity for the model.** Ids (`agent-N`) exist in events but the model can never reference a running child, check on it, or receive a later settlement — there is nothing to reference after the round returns.
3. **Settlement is synchronous only.** No mechanism delivers a child result into a later round.
4. **Overlap admission is batch-local.** `admitted_worker_scopes` protects only same-batch siblings — sufficient today (children can't outlive the round), insufficient the moment children run in the background.
5. **Parent/child write conflicts are impossible today only because the parent is frozen.** Background execution requires a parent-side fence against active Worker scopes (structural "do not duplicate child-owned work").

## Orchestration policy location (current)

- One-shot round-0 steer hint (`multi_agent_steer_hint`, un-keyword-gated) + spawn_agent description + one-shot decision point. No coordinator framing in the base system prompt; the model's role is "executor who may delegate", not "coordinator".
- `task_suggests_delegation`: **no critical-path role** (export + telemetry only since the keyword-gate removal). Must stay that way.

## Production behavior on this architecture (deepseek-v4-flash + v4-pro)

Across hyperfine, xsv --json ×4, miller ×2, xsv json+dedup ×2, controls ×5: decision point reached and answered explicitly; **0 natural delegations**; KEEP rationales consistently cite exactly the costs this architecture actually imposes (blocking, shared-context loss, integration overhead). The prior conclusion "model adoption failure" is superseded: the model was accurately pricing a synchronous delegation primitive.

## Limits & policies

- depth = 1 (children cannot spawn), concurrent = 4, total = 6 per run, child wall cap 20 min, reviewer 20 rounds. All appropriate for Beta.
- Recovery/ownership/Completion Truth: unchanged by any of this; production-proven in prior gates.
