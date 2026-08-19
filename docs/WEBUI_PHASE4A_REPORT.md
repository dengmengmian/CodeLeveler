# WebUI Phase 4A — Agent Delegation Projection

**Date:** 2026-08-19  
**Baseline:** `1010f0fd44c68b92aa05cbdd5f1b1bd5d9292dee`  
**Scope:** Session-wide Agent projection + honest in-session Delegation view.
No Child Session, no Replay, no new EventLog, no new Query type, no protocol
change.

---

## 1. Code Facts

Current Runtime Agent model (unchanged):

```
Parent Session  (one SessionId, one EventLog)
    spawn_agent  →  id = "agent-N"
                    SubAgentStarted { id, nickname, role, task }   durable
                    child tools carry agent_id = that id
                    SubAgentFinished { id, nickname, ok, summary } durable
```

This is **in-session delegation**, depth = 1.

It is **not**:

| Capability | Present? |
| --- | --- |
| Child `SessionId` | no |
| `parent_id` | no (parent is implicit: this EventLog) |
| Per-child EventLog | no |
| Child Resume | no |
| Durable `SubAgentProgress` / `SubAgentActivity` | no (transient) |
| Nested spawn beyond depth 1 | no |

`UiAgentObservation` fields that exist: `id`, `nickname`, `role`, `status`,
`summary`. There is no separate `task` field. While running, `collect_agents`
stores the start-task in `summary`; on finish, the finish-summary overwrites
it. The UI does not invent the lost task.

Tool rows already project `fields.Agent` (`agent-N` or `main`). That is the
only attribution used in Execution.

---

## 2. Query Fix

```
before:
  agents = collect_agents(&decoded)     // decoded = Event Window

after:
  agent_rows = load_by_types(sub_agent_started, sub_agent_finished)
  agents = collect_agents(&decode_records(&agent_rows))
```

Same pattern as session-wide tools. Still `QueryObservability`. Still
`UiAgentObservation[]`. No new store.

| Slice | Scope after 4A |
| --- | --- |
| `window` | bounded Event Window (unchanged) |
| `tools` | session-wide lifecycle |
| `agents` | session-wide lifecycle |
| `requests` | whole-session `model_requests` |

Long sessions keep early Explorer / Worker rows even when their start/finish
sit outside the last-80 window.

TUI `/trace` Agents empty copy and `leveler trace` heading were updated to
match this fact (`此会话` / `Agents (session-wide)`). Not a TUI feature
expansion.

---

## 3. UI Semantics

```
Agent = delegated worker in this Session
```

Not:

```
independent child Session
```

Inspector / Activity render a **star**:

```
Main
├ Explorer   ✓ Finished
└ Worker     ● Running
```

Labels: `AGENTS` / `Delegated`. Rows are not session switchers. There is no
Agents rail page, no child history, no org tree, no fake depth > 1.

---

## 4. Live vs Durable

| Source | What it is | UI |
| --- | --- | --- |
| `SessionView.agents` | Current-turn live HUD (`sub_agent_updated` / progress / activity). Cleared on the next user message. | Inspector `Running` only (`status === 'run'`), including `recentStep` |
| `QueryObservability.agents` | Durable session-wide start/finish projection | Inspector `Delegated` star; Activity count and list |

Activity `Agents` count = `projectObservability().agents.length`
(`summary.delegatedAgents`). Never `SessionView.agents.length`.

---

## 5. CompletionTruth Fix

Bug: `changesApplied` could be true from `completionReport.files_changed > 0`
while `artifacts` was `0 files +0 −0` (`diff == null`).

```
diff present, files.length > 0
        → exact totals   "N files  +x −y"

diff present, files.length === 0
        → none           "未改仓库"
        (does not fall back to the report)

diff == null, files_changed > 0
        → report count   "N files changed"
        (no invented +/−)

otherwise
        → none           "未改仓库"
```

`added` / `removed` are `null` on the report path. Facts, Inspector Artifacts,
and Changes footer share this projection.

---

## 6. Observability Ordering

**Not verified serial-safe.** Guard added.

One WebSocket tab delivers commands sequentially, and each
`QueryObservability` awaits the SQLite query before Ack. That is not enough:

`ObservabilityLoaded` is a **session broadcast**. TUI `/trace`, a second Web
tab, or any other client on the same session can complete a query while
another query is in flight. An older `last_sequence` can arrive later.

Store reducer:

```
same session_id (existing)
AND incoming.last_sequence >= current.last_sequence
```

Missing sequence counts as `-1`. Equal is a refresh. Session switch still
clears `observation` first. No request manager. No protocol change.

---

## 7. Validation

```
cargo test -p leveler-app --lib     145 passed
npm test                            92 passed
npm run typecheck                   PASS (protocol.gen.ts in sync)
npm run build                       PASS
```

Covered:

| Case | Where |
| --- | --- |
| Agent start+finish both outside the window | `agent_list_is_session_wide_not_window` |
| Start outside, finish inside, role preserved | same |
| Running agent outside the window | `running_agent_outside_window_still_listed` |
| running / completed / failed ViewModel | `observabilityView.test.ts` |
| Finished agent does not invent a task | same |
| Activity count = observatory list, not live | same + `store.test.ts` live-vs-durable |
| `SessionView.agents` cleared on next turn; observatory list kept | `store.test.ts` |
| report-only `files_changed` is not `+0 −0` | `completionTruth.test.ts` |
| loaded empty diff does not fall back to the report | same |
| stale `last_sequence` dropped | `store.test.ts` |

Headed browser click-through was not run in this environment.

---

## 8. Remaining Runtime Gaps

Must stay visible. UI did not invent them:

```
No child SessionId
No parent_id
No per-child EventLog
No durable SubAgentActivity
No durable SubAgentProgress
No debugger Replay
```

Also still true, and not papered over:

- Finish overwrites the start-task on `UiAgentObservation.summary`. Completed
  rows show the finish summary only.
- Restart still drops in-flight children (`lost_children_note`).
- Reviewer is harness-launched, not `spawn_agent role=reviewer`.
- Execution remains a flight recorder of the observatory window. No play,
  pause, scrubber, or streamed-delta replay.

Phase 4A stops here. Child Session belongs on a later Structured SubAgent
Capability path, not a WebUI mock.
