# Multi-agent architecture review

Self-review after product closure. No second runtime, scheduler, EventLog, or
Goal state machine was introduced.

## Reused

- `spawn_agent` / `run_one_sub_agent_on` / `run_reviewer_child`
- `AgentRole`, depth=1, concurrency=4, total=6
- `ChildResult` status truth (N1)
- `EvidenceLedger` + `EvidenceLedgerUpdated`
- `SubAgentStarted` / `SubAgentFinished`
- ToolHost permission, ownership fence, write allowlist
- `read_only_subset` (observe ∩ Safe)
- Existing completion / review-stage gates
- TUI sub-agent tree (projection of the same events)

## New (narrow)

- `FindingRecord` / `FindingState` / `FindingKind` on the existing ledger
- `report_finding` / `resolve_finding` injected tools
- `ChildProfile::{resolve,admit}` + `scopes_overlap`
- Host `record_parent_finding` when a Worker does not finish
- `carry_forward_findings` for unsettled debt across a fresh epoch
- TUI finding-count projection from the finish summary

## Checks

| Risk | Result |
| --- | --- |
| Duplicate lifecycle | No. Findings refine EvidenceLedger; they do not replace Goal/Turn/Task. |
| Duplicate ownership | No. Child writes still go through ToolHost + parent fence. |
| Second scheduler | No. Same-batch join of `spawn_agent` calls. |
| Role × tool-name explosion | No. Profile → registry class + allowlist. |
| Overly generic framework | No. Closed kinds, closed states, one admit function. |
| Distributed abstraction | No. Local harness only. |
| UI-only truth | No. TUI counts from the durable finish summary; ledger is SoT. |
| Child completion false-positive | Structured findings count as a result; incomplete Worker is blocking. |
| Unresolved finding closeout bypass | Blocking finding refuses `update_goal(complete)` and `Verified`. Ledger load failure is fail-closed. |

## Known limits (not blockers)

- `ChildResult.findings` remains the prose field; typed records travel on
  `SubAgentRunResult` / `DelegatedChildResult`. Parent join always adopts
  those, so the product path is wired.
- Same-batch Worker overlap is checked; a later round that re-scopes onto a
  still-running Worker is not. Depth is 1 and workers are serial, so the
  residual risk is a sequential overwrite the allowlist still attributes.
- Web has no multi-agent chrome. Protocol events already exist.
  `DEFER_POST_BETA`.
- Explorer has no browser-read tools (Network-risk / outside observe class).
  `DEFER_POST_BETA`.
- Incomplete harness review that later relaunches may adopt a second copy of
  the same prose finding (new parent id). Reject the duplicate.
