# WebUI Phase 2 — QueryObservability Integration

**Date:** 2026-08-19  
**Scope:** Project existing QueryObservability into Execution workspace and
Inspector Runtime. No new EventLog path, stats table, protocol, or dashboard.

---

## 1. Implemented

| Surface | Behavior |
| --- | --- |
| Execution workspace | Timeline from `UiObservabilityLoaded.window`, grouped by `turn_id` |
| Inspector Runtime | Model, duration, request count, session-wide tool_started, verification_runs |
| Activity sidebar | Same observatory totals + `UiToolAggregate` (whole session) |
| Projection | `lib/observabilityView.ts` maps DTOs → `ObservabilityView` |

Removed Inspector「更多」that aggregated `SessionView.tools` (current-turn live list).

---

## 2. Data flow

```
EventLog + model_requests
        ↓
leveler-app::query_observability
        ↓
ClientCommand::QueryObservability   (existing; after=80)
        ↓
RuntimeEvent::ObservabilityLoaded
        ↓
Web WS deliver / applyEvent
        ↓
AppState.observation
        ↓
projectObservability()
        ↓
ExecutionView / Inspector Runtime / Activity sidebar
```

TUI `/trace` and `leveler trace` already use this query. Web now does too.

Refresh: same live event set as TUI `should_refresh_trace` (tool/turn/verify/…).
`observability_loaded` does not re-query.

---

## 3. Files changed

- `web/src/lib/observabilityView.ts` + `.test.ts` — projection
- `web/src/lib/controller.ts` + `.test.ts` — `queryObservability`, apply loaded, refresh
- `web/src/state/store.tsx` + `.test.ts` — `observation` off live tools
- `web/src/components/ExecutionView.tsx` — timeline
- `web/src/components/Inspector.tsx` — Runtime section
- `web/src/components/Rail.tsx` — Activity from observatory
- `web/src/app.css` — debugger-style timeline
- `docs/WEBUI_PHASE2_REPORT.md`

**Not changed:** EventLog, `query_observability` backend, ClientCommand /
RuntimeEvent schema, Agent Graph, Replay.

---

## 4. Validation

```
npm test         73 passed
npm run typecheck  PASS
npm run build      PASS
```

Headed browser click-through not run here.

---

## 5. Remaining issues

- **No session turn count on `UiSessionObservation`.** Protocol was not
  extended. Inspector shows **Requests** (`request_count`) and **Tools**
  (`tool_started`), not a guessed turn total. Unique `turn_id`s in the
  window are only used to **group the timeline**, not as a session total.
- Timeline is the bounded Event Window (latest 80), same cap as TUI `/trace`.
  Tool **counts** are whole-session.
- Live `SessionView.tools` still feeds Conversation `AgentRunBlock` (current
  turn HUD). That is not used for Runtime totals.
- Activity / Execution still need a headed walkthrough on a real display.
