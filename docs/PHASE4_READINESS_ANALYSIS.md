# Phase 4 Readiness Analysis

**Date:** 2026-08-19  
**Rule:** code facts only. No UI, no protocol change, no mock agents.  
**Question:** Can WebUI ship Agent Graph / Replay from **real** Runtime, or
must Harness Foundation land first?

**Verdict:**

```
Do NOT ship Agent Graph as “child sessions”.
Do NOT ship debugger Replay.

A truthful star graph (Main + in-session spawn_agent children) is
possible from existing SubAgentStarted/Finished — after a query-layer
fix so agents are session-wide, not Event-Window local.

True nested SessionIds / parent_id / independent child EventLogs
do not exist. Inventing them in UI would be fake.
```

---

# Current Capability

| Layer | What exists | Enough for Graph? | Enough for Replay? |
| --- | --- | --- | --- |
| `spawn_agent` | Real tool; roles explorer/worker; depth 1 | In-session children, not child sessions | n/a |
| EventLog | `sub_agent_started` / `sub_agent_finished` durable | Identity + role + task/summary | Lifecycle yes; live steps no |
| Tool attribution | `ToolCallStarted.agent_id` | Tools can hang off a child id | Partial |
| QueryObservability | `UiAgentObservation[]` from **window** | **No** — same class of bug Tools had | Window ≠ session history |
| Web | Execution timeline, Inspector Agents list (live), Completion Truth | Reuse shell; no graph component | Execution tab ≈ flight recorder, not replay |
| Engine resume | `EventLog::replay` + context snapshot | n/a | Resume ≠ UI replay |

---

# Runtime Analysis

## Parent / child as implemented

Current shape (`crates/leveler-agent/src/executor/drive.rs`):

```
Parent Session  (one SessionId, one EventLog)
    spawn_agent  →  id = "agent-N"
                    SubAgentStarted { id, nickname, role, task }
                    (child tools use agent_id = that id)
                    SubAgentFinished { id, ok, summary }
```

This **is** a parent–child relationship, but:

- Children are **not** `SessionId`s. Grep of `leveler-agent/src/sub_agent.rs`
  finds no `parent_id` / `child_session`.
- Parent is implicit: whatever session the EventLog belongs to.
- `UiAgentObservation` has `id, nickname, role, status, summary` — no
  parent field (`observability.rs`).

Background-first is in the **current** tree (`drive.rs` ~192–197,
`injected_tools.rs` spawn contract: returns immediately, settlement later).
That is **not** a second session; it is an in-process child of the same run.
Restart: outstanding children are **lost** and the model is told so
(`lost_children_note`).

Reviewer is harness-launched (`ReviewStage`), not `spawn_agent role=reviewer`
from the model (`MULTI_AGENT_V2_CURRENT_STATE.md`).

## Does this support a Graph UI without lying?

| Product sentence | True? |
| --- | --- |
| “Main spawned Explorer / Worker in this session” | Yes, if start/finish rows exist |
| “Each child is its own session you can open” | **No** |
| “Graph survives daemon restart with live tokens/steps” | **No** (progress/activity transient) |
| “Parent continues while children run” | Advertised; children still share this session’s log |

A **star graph** labeled as delegated workers of **this** session is honest.
A tree of sessions is not.

---

# EventLog Analysis

Canonical enum: `crates/leveler-engine/src/event.rs`.

## Agent lifecycle

| Fact | Event | Durable? |
| --- | --- | --- |
| Child started | `SubAgentStarted { id, nickname, role, task }` | **yes** (`is_transient` does not include it) |
| Child finished | `SubAgentFinished { id, nickname, ok, summary }` | **yes** |
| Live tokens/steps | `SubAgentProgress` / `SubAgentActivity` | **no** (explicitly transient) |
| Delegation KEEP/DELEGATE | `DelegationStage { action, detail }` | **yes** |
| Reviewer policy | `ReviewStage { action, detail, … }` | **yes** |

There is **no** `agent.spawned` with `parent_id` / `child_session_id`.

## Relationship

| Needed | Present |
| --- | --- |
| `parent_id` | **Missing.** Infer: all children belong to this `session_id`. |
| `child_id` | `SubAgentStarted.id` (`agent-1`, …) |
| Tool → agent | `ToolCallStarted/Finished.agent_id: Option<String>` (legacy omit = parent) |

## Result

| Needed | Present |
| --- | --- |
| Child result | `SubAgentFinished.ok` + `summary` (truncated in observatory to 64 chars) |
| Structured findings / artifacts per child | **Not** on the agent events. Diff is **session** `UiDiff`, not per-agent. |

`EventLog::replay` (`log.rs:115`) reconstructs every **persisted** event
fail-closed. Transient rows never enter the log.

---

# QueryObservability Analysis

`query_observability` (`crates/leveler-app/src/observability.rs`):

| Slice | Source | Scope |
| --- | --- | --- |
| `session` | `SessionRecord` + `count_by_type` | whole session (`subagent_started` **count** only) |
| `window` | `load_window` | bounded (default last 80) |
| `tools` | `load_by_types(tool_call_*)` | **whole session** |
| `requests` | `model_requests` | whole session |
| `agents` | `collect_agents(&decoded)` | **window only** |
| `recovery.review_stages` | window | window |
| `relations` | `call_id` / sub-agent `id` / `turn_id` | window |

`session.subagent_started` is a session-wide **count**. The **list** of
agents is not. After a long run, Execution/TUI Agents tab can miss early
children — same class of bug Tools Summary had before session-wide
`load_by_types`.

No child-session query. No execution-graph DTO. Hierarchy would be derived
in the client: Main + `UiAgentObservation[]`.

**Do not add a new query type for Phase 4 analysis.** If Graph is approved,
the honest increment is: session-wide `load_by_types(sub_agent_started,
sub_agent_finished)` — same pattern as tools — **not** a new EventLog.

---

# Replay Feasibility

Two different products:

## A. Engine resume (exists)

`EventLog::replay` + `latest_context_snapshot` + session/turn rows.
This restores **the agent**, not a UI scrubber.

## B. UI debugger Replay (does not exist)

| Reconstruct | Durable? |
| --- | --- |
| User request / turns / tool start-finish | yes |
| Plan / verification / task outcome | yes |
| Diff artifacts | snapshot `UiDiff`, not a per-event film |
| Assistant / reasoning **deltas** | **no** (transient) |
| Sub-agent live tokens / current step | **no** |
| TTFT / tool request id | **never recorded** |

Web Execution timeline is already a **flight recorder of the observatory
window**, not Replay. Stepping “as if live” would fabricate deltas that
were never stored. That is forbidden.

---

# WebUI Reuse Map

| Target | Reuse | Gap |
| --- | --- | --- |
| Agent Graph | Shell; Inspector `AgentRow` (live); Execution agent-class rows; `UiAgentObservation` | No graph component; live list is **this turn**; durable list is **window** |
| Replay viewer | Execution timeline + Changes + Completion Truth + snapshots | No snapshot-at-seq UI; no playhead; deltas missing |
| Artifacts | `DiffView` / `completionTruth` | Session-scoped, not per child |

Do **not** reuse `SessionView.agents` as the graph source of truth (cleared
on next user message, same anti-pattern as live tools).

---

# Missing Capabilities

## Already available

- `spawn_agent` + durable start/finish in the **parent** EventLog
- `agent_id` on tool lifecycle
- `DelegationStage` / `ReviewStage`
- QueryObservability session header + window + session-wide **tools**
- Web Execution / Inspector Runtime / Completion Truth / Changes
- Engine `replay` for resume

## Need backend / query (not UI cosmetics)

| Gap | Kind |
| --- | --- |
| Agents list is window-local | Query projection (same fix as tools) |
| No `parent_id` / child `SessionId` | Runtime — only needed if product wants nested sessions |
| Child result is a string summary, not structured artifacts | Runtime — don’t fake in UI |
| Background children die on process restart | Runtime (documented) |
| Sub-agent tokens after restart | Not durable; already Remaining Debt |
| UI Replay of streams | Missing durable facts; **cannot** be completed honestly |

## UI only (after query-scope fix)

- Star graph: Main + children from session-wide start/finish
- Click child → filter Execution tools by `agent_id` (data already on rows
  if window contains them; may still need window navigation)

---

# Recommended Next Steps

**Do not choose “A. Direct Agent Graph UI”** if that means nested sessions
or a live org-chart of processes that don’t exist.

**Do not choose “B. Child Session Runtime”** as a WebUI Phase 4. That is a
Harness change (new identity, fencing, EventLog per child). Out of scope
for projection work; `MULTI_AGENT_V2_CURRENT_STATE.md` already records the
cost of treating children as first-class concurrent sessions.

**Recommended Phase 4 (if any), in order:**

1. **Query completeness (tiny, same observatory pattern):** session-wide
   agent start/finish via `load_by_types`, like tools. No new event types.
2. **Honest star graph (UI only):** Main (this session) + those rows.
   Label: 本会话委派，不是独立 Session. No mock children.
3. **Replay:** keep Execution as the flight recorder. Do **not** start a
   Replay player until product accepts that reasoning/text streams cannot
   come back.

If product insists on “open this child as its own session”, that is
**Harness Foundation first**, then UI. WebUI must not invent it.

---

# Decision for Beta

| Ask | Answer |
| --- | --- |
| Graph of real in-session workers? | Feasible after session-wide agent projection |
| Graph of child sessions? | **Runtime missing — do not mock** |
| Debugger Replay? | **Not feasible** without storing transients; Execution already shows durable trajectory |
| Next code? | Only if product picks (1)+(2). Otherwise stop at Phase 3 engineering loop |
