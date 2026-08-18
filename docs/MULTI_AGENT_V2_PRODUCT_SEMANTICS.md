# Multi-Agent V2 — product semantics

**Top-level principle: Main is both Coding Agent and Coordinator.** Main understands the
Goal, establishes the workstreams, identifies independent bounded work, delegates when
delegation is materially useful, continues useful independent work while children run,
avoids duplicating child-owned work, integrates results, judges findings, performs final
verification, and owns Completion Truth.

## Default orchestration policy (model-visible, one canonical home)

- Small or tightly coupled work → **KEEP**.
- An independent bounded workstream with meaningful execution value → **DELEGATION-PREFERRED**
  ("delegate focused independent work so it does not consume this conversation's context").
- Multiple independent workstreams → start the delegations **together in one assistant
  message**; they run concurrently.
- Children run **in the background by default** (runtime-resolved when the parameter is
  omitted — the model relies on the advertised default, it does not have to reproduce it).
- Foreground (`run_in_background: false`) **only when the next action depends on that
  child's result**.
- While children run, **continue useful independent work**: another disjoint area,
  integration boundaries, test preparation, consuming other children. Never edit files a
  running child owns — the runtime refuses it (structural, not advisory).
- The runtime **tells Main when each child settles** (unconditional notice carrying the
  child's truthful result, partial results included). No polling.
- KEEP remains first-class: trivial edits, one coupled chain, unsettled interfaces, no
  safe ownership boundary, child would need nearly all of Main's context, integration
  cost exceeds benefit. Selective orchestration, not maximal agent count.

This is product policy, not eval coercion. Forbidden forever: "always spawn", per-repo or
per-task rules, keyword matrices, file-count/round-count triggers.

## Safety semantics (CodeLeveler-owned, unchanged or strengthened)

- Worker requires an explicit exclusive scope (files or directories); writes outside are
  refused pre-execution.
- Overlapping Worker scopes are refused — same batch AND against any still-running child.
- The PARENT's own mutations inside an active child's scope are refused with an honest
  message (structural duplicate-work/conflict prevention; stronger than the reference,
  which leaves sibling coordination to the model).
- Explorer/Reviewer stay structurally read-only; Reviewer stays harness-launched;
  depth stays 1; concurrency 4 / total 6 caps stay.
- ChildResult four-way truth and partial preservation stay; the settlement notice embeds
  them verbatim. Findings lifecycle and blocking-finding gates stay authoritative.

## Lifecycle semantics (Beta scope)

- BETA: background one-shot children · durable model-visible child identity · unconditional
  round-boundary settlement · outstanding-children completion gate (no Verified/complete
  while a child's result is unconsumed) · end-of-run drain (a run never orphans a running
  child silently) · resume truth (children do not survive a process restart: a durable
  ledger note tells Main which children were lost, scope released, re-delegate if needed).
- POST-BETA (deliberately deferred): continuable children, send_message, context fork,
  interrupt tool, list_agents, external child providers, workflow scripting.

## Observability

Existing events carry V2: SubAgentStarted at spawn, SubAgentFinished at settlement,
DelegationStage facts (offered/kept/delegated) unchanged, ProgressLedger tracks
outstanding children durably. No new event log, no scheduler, no second runtime — the
drive loop owns child futures exactly as it owns tool futures today.
