# Durable Runtime Observatory Audit

**Date:** 2026-08-19  
**Rule:** current code only. Observability is a **read model** over existing
durable facts. No second EventLog. TUI never opens SQLite.

```
Canonical Durable Facts (EventLog + model_requests + sessions + turns)
        ↓
Durable Observability Query  (leveler-app, fail-closed decode)
        ↓
Protocol DTOs / ClientCommand (read-only)
        ↓
TUI /trace  and  CLI `leveler trace`
```

---

## A. Durable Fact Matrix

| Area | Current source | Durable? | Queryable now? | TUI visible? | Gap |
| --- | --- | --- | --- | --- | --- |
| Session | `SessionRecord` | yes | yes (`SessionRepository`) | list/header | no observation DTO |
| Turn | `TurnRecord` (ordinal, kind, status, timestamps) | yes | yes (`TurnRepository::list`) | TurnEnd marker only | no public turn query |
| Runtime / Engine event | `events` via `EventStore` (`sequence`, `turn_id`, `type`, payload) | yes (non-transient) | `load` / `load_after` / `load_last_by_type` — **no bounded window around seq** | live `RuntimeEvent` only | window query missing |
| Model request | `model_requests` (`ModelRequestRecord`: id, provider, model, tokens, finish_reason, error_kind, latency_ms, retry_count, created_at) | yes | repo `load_for_session` exists; **trait is insert-only** | last-round `TokenUsage` only (transient) | expose load on store trait |
| Tool | `EngineEvent::ToolCallStarted/Finished` (call_id, name, args, is_error, agent_id) | yes | `EventStore::load_by_types` + query-layer pair on `(call_id, agent_id)` | TUI Tools tab (session-wide) | duration = matched pair timestamps; unfinished ≠ success; no request-id on tool |
| Verification | `VerificationStarted/Check/Finished`, `AcceptanceEvidence` | yes | event scan | live `UiVerification` | no historical query |
| Reviewer | `ReviewStage`, `ReviewStarted/Finding/Failed/Finished` | yes | event scan | ReviewStage not on TUI | project from events |
| SubAgent | `SubAgentStarted/Finished` durable; Progress/Activity **transient** | partial | start/finish only | live if TUI was open | tokens after restart: DEFERRED |
| Recovery | `WorkspaceSnapshotCreated`; interrupted turns (`finished_at` null); `RepairStarted`; ownership is engine-internal | partial | turns + events | no | owner epoch **not** on protocol (keep internal) |
| Usage | `ModelRequestRecord` sums | yes | `load_for_session` | last round only | use request table, do not sum transient TokenUsage |
| Context | `Compacted`, `ContextExpanded` durable; `ContextSnapshot` durable but **prompt-like** | yes | events | compact notice | never dump snapshot messages |
| Terminal | `TurnFinished` / `TaskFinished` + `TurnRecord.status` | yes | yes | TurnEnd | map existing outcomes, do not invent |

Transient (forwarded, **not** in EventLog): `TokenUsage`, stream deltas, `SubAgentProgress/Activity`, `UserShellOutput`, `CommandProgress`, `RunFinished`.

---

## B. Existing Query Surfaces

| Surface | Reuse |
| --- | --- |
| `EventStore::load` / `load_after` | yes, plus a **bounded window** |
| `EventStore::load_last_by_type` | latest snapshot/ledger — not the trace list |
| `ModelRequestRepository::load_for_session` | yes; add to `ModelRequestStore` trait |
| `SessionRepository` list/get | session observation header |
| `TurnRepository::list` | turn table |
| `ClientCommand::RequestSessionList` | pattern for read-only query → `RuntimeEvent` |
| `UiSessionSnapshot.last_sequence` | resync anchor |
| CLI `sessions show` | already reads EventLog + model_requests **directly** (CLI/app boundary). TUI must not copy that. Fold CLI onto the same query module. |
| Resume / `EventLog::replay` | fail-closed decode — query uses the same `EngineEvent::from_payload` |
| `PublicEvent` | remote-safe subset. Local observatory may show path/command **summaries** already redacted at write. |

---

## C. Missing Durable Facts

**ALREADY DURABLE + QUERYABLE:** sessions, turns, model_requests rows, full EventLog load, last-by-type.

**DURABLE BUT NOT QUERYABLE (protocol/TUI):** event window around seq; tool/verify/agent/recovery projections; request history as a client DTO.

**LIVE ONLY:** TokenUsage stream, sub-agent live tokens/steps, command progress, reasoning/text deltas.

**DERIVABLE FROM DURABLE FACTS:** tool duration from start/finish timestamps; request summary from `model_requests`; interrupted turns from `finished_at`; tool class from tool name; start/finish pairs on `call_id` / sub-agent `id` / approval `id` / shell `execution_id`.

**MISSING AND NOT WORTH ADDING:** TTFT, full prompt/body, owner epoch on the wire, OTel, second log, Goal/Window databases.

**MISSING AND REQUIRED for V1:** bounded EventStore window; read-only `ClientCommand` + result event; app query module (single source for TUI + CLI).

---

## D. Architecture Decision

| Question | Decision |
| --- | --- |
| Which crate owns Query? | **`leveler-app`** — composition root already maps `EngineEvent` → `RuntimeEvent`, opens `Database`, and must not leak storage into TUI. Engine stays the write owner of EventLog. |
| Protocol? | Additive read-only `ClientCommand::QueryObservability` + `RuntimeEvent::ObservabilityLoaded`. Unknown variants stay ignorable (`parse_runtime_event`). |
| Schema change? | **No new tables.** Event `sequence` index already exists. |
| New storage index? | Optional `COUNT GROUP BY type` on existing `events` — not a new log. |
| Replay EventLog? | Window + typed counts. Never load an unbounded session into TUI. Decode fail-closed. |
| Request/tool tables? | Requests: `model_requests`. Tools: EventLog start/finish pairs (no separate tool table). |

Causal links V1 will emit only with identity:

- `call_id` → ToolStarted ↔ ToolFinished  
- sub-agent `id` → Started ↔ Finished  
- approval / clarification / user-shell ids  
- same `turn_id` (related, not “caused”)

No time-proximity guessing. Repair↔verify is **same-turn related** only.

---

## Honest limits (V1)

- Historical reconstructs what EventLog + `model_requests` stored. Sub-agent token totals after restart are unavailable (transient).  
- Tool duration is wall-clock between persisted start and finish rows, not a runtime-measured field. Unfinished starts are `unfinished`, not success, and carry no duration.  
- Tool Summary is **session-wide** (`load_by_types` of lifecycle rows). The event window is **not** the aggregate source.  
- Agents tab + Recovery review stages remain window-local; copy already says 窗口.  
- Local TUI/CLI only; `PublicEvent` stays the remote boundary.  
- Previous “live-only TUI ring buffer” is **not** the source of truth.

---

## Closeout addendum (session-wide tools)

The V1 window-local Tools Summary gap is closed in `query_observability`:

```
EventStore::load_by_types(tool_call_started, tool_call_finished)
        ↓
decode fail-closed
        ↓
aggregate_tools  (pair on call_id + agent_id)
        ↓
UiObservabilityLoaded.tools
```

No `tool_stats` table. TUI does not group_by. Requests were already
session-wide (`model_requests`) — unchanged.
