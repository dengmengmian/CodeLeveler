# CodeLeveler WebUI v2 Architecture

**Document Version:** v2.0  
**Status:** Proposed  
**Target:** WebUI + future Desktop Client  
**Companion:** [`CURRENT_WEBUI_ANALYSIS.md`](CURRENT_WEBUI_ANALYSIS.md)

This is a **constraint document**, not a visual mock. Implementation must
treat it as the acceptance standard. It does **not** authorize a second
EventLog, a Grafana dashboard, or a Web-only query path around SQLite.

---

# 1. Overview

## 1.1 Background

CodeLeveler WebUI already has Conversation, tool execution, Changes,
session management, and (on the protocol) Runtime Observability. The UI
still behaves like a **Web Chat Interface**.

The product is an **AI Software Engineering Workspace**. WebUI v2 moves
from:

```
Chat UI + Tool Output
```

to:

```
Desktop Application Layout
+ Agent Runtime Workspace
+ Execution Debugging Environment
```

Observability is **not a page**. It is the second view of every Agent
Session (internal name: Runtime Observatory; user-facing: **Activity**).

## 1.2 Non-goals

- Admin / OTel / CPU-memory-latency dashboards
- Full-text search of EventLog
- Session replay debugger (Phase 4)
- Changing Long Goal, Multi-Agent, Verification, or tool-execution
  semantics
- Browser SQLite, or TUI/Web each inventing a private event model

Canonical facts stay in EventLog + `model_requests` + sessions + turns.
The existing read model is `query_observability` →
`ClientCommand::QueryObservability` → `RuntimeEvent::ObservabilityLoaded`.
Web consumes that the same way TUI `/trace` and `leveler trace` do.

---

# 2. Product Positioning

```
AI Software Engineering Workspace
  = IDE Workspace
  + Agent Runtime
  + Debugger / Flight Recorder
  + Change Review
  + Multi-Agent Control Plane (later)
```

The user must see not only the answer, but **what the agent did, why,
what changed, and whether the result is trustworthy**.

---

# 3. Design Principles

## 3.1 Desktop Application Layout

Use an Application Shell, not a marketing web-page layout:

```
Application Header
Navigation Rail
Workspace
Inspector
Composer
```

## 3.2 Runtime First

Do not design around `Message`. Design around:

```
Session → Turn → Execution → Artifact
```

## 3.3 Observability Is Product Capability

Runtime Observatory is an **Agent Flight Recorder**, not monitoring.
It answers:

- What is the agent doing now?
- Why this tool?
- Which files were affected?
- Did verification finish?
- Can I trust completion?

---

# 4. Application Shell

```
┌───────────────────────────────────────────────┐
│ Application Header                            │
├───────┬──────────────────────┬────────────────┤
│ Rail  │ Workspace            │ Inspector      │
├───────┴──────────────────────┴────────────────┤
│ Composer                                      │
└───────────────────────────────────────────────┘
```

---

# 5. Application Header

Global identity and one high-weight run status only.

**Left:** project, branch.  
**Right:** session/runtime status, menu.

Forbidden: tabs, stacked task essays, runtime ledgers in the header.

---

# 6. Navigation Rail

Width **48px** for first-level icons. Business lists live in a **context
sidebar** of the current rail item, not in the icon rail.

```
◉ Sessions
▣ Workspace
⌕ Search
⑂ Changes
◎ Activity
◇ Agents
⚙ Settings
```

`Activity` = Runtime Observatory (user-facing name).

---

# 7. Sidebar

Belongs to the current Rail context.

**Sessions:** New Task, resume, history grouped by day.  
**Workspace:** files, symbols, repository, environment.

---

# 8. Workspace Area

Top of the workspace (not the application header):

```
Conversation | Changes | Execution
```

## 8.1 Conversation View

User request, agent response, execution summary, completion state.
No social chat bubbles. Content + accent line (already the Web
direction).

## 8.2 Changes View

Changed files, focused diff, review, verification. File list + one
focused file is the current contract; do not reopen a second router.

## 8.3 Execution View

Runtime debug view. **Data source: EventLog via `QueryObservability`**,
not a live ring buffer and not a new event taxonomy.

```
User request → Planning → Tool calls → Agent decisions
→ Verification → Completion
```

---

# 9. Inspector

Dynamic context panel. Modes stay **waiting > running > terminal > idle**.

| Mode | Shows |
| --- | --- |
| Running | Task, current step, model, tokens, tool counts |
| Completed | Result, verification, changes, trace counts |
| Waiting | Permission / clarification actions only |

Not a dashboard of equal-weight sections.

---

# 10. Runtime Observatory

| Layer | Name |
| --- | --- |
| Internal | Runtime Observatory |
| User-facing | Activity |

Data:

```
Session → Turns → Events → Tools → Artifacts
```

Projection already exists (`UiObservabilityLoaded`). Web must not
re-aggregate the EventLog in the browser.

---

# 11. Agent View

Prepare for Multi-Agent; do not invent a second agent runtime.

```
Main → Explorer / Reviewer / Worker
```

Status, current task, child identity, result. Live
`SubAgentUpdated` already exists on the wire; durable start/finish is
on EventLog. Graph UI is Phase 4 unless a cheap Inspector tree reuse
is enough.

---

# 12. Composer

Default: text + attach + send. Advanced settings in **Run
Configuration** (model, reasoning read-only, permission, product axes).
Do not put four chips back on the first row.

---

# 13. Frontend Architecture

```
Agent Runtime
      ↓
EventLog  (canonical, persist-before-forward)
      ↓
Backend Projection  (query_observability in leveler-app)
      ↓
ClientCommand / RuntimeEvent  (existing protocol)
      ↓
Frontend View Model
      ↓
React Components
```

UI must not open SQLite. UI must not invent `model.started` /
`tool.started` types if `RuntimeEvent` / `EngineEvent` already name
the fact. Map, do not duplicate.

---

# 14. Frontend Domain Model

Reuse protocol DTOs. Do not add `tool_stats` tables.

| Concept | Current canonical |
| --- | --- |
| Session | `SessionRecord` / `UiSessionSnapshot` / `UiSessionSummary` |
| Turn | `TurnRecord`; live turn via `RuntimeEvent` terminals |
| Execution window | `UiObservationRow` from `ObservabilityLoaded.window` |
| Tools (session) | `UiToolAggregate` (whole-session) |
| Requests | `model_requests` → `UiRequestObservation` |
| Artifact | `UiDiff` / verification / checkpoints already on snapshot |

Proposed names like `ExecutionEvent.type = model.started` are
**documentation aliases**. Wire names stay `assistant_message_started`,
`tool_call_started`, `verification_finished`, etc.

---

# 15. Component Architecture (target)

```
app-shell/     ApplicationRail, Header, WorkspaceLayout
workspace/     ConversationView, ChangesView, ExecutionView
inspector/     TaskInspector, RuntimeInspector, AgentInspector
activity/      Timeline, EventCard
agents/        AgentGraph
composer/      Composer, RunConfiguration
```

Existing files (`App.tsx`, `Rail.tsx`, `Timeline.tsx`, `DiffView.tsx`,
`Inspector.tsx`, `Composer.tsx`, `AgentRunBlock.tsx`) are the migration
substrate, not a greenfield tree.

---

# 16. Migration Plan

| Phase | Goal |
| --- | --- |
| 0 | Current analysis (this pair of docs). No UI change. |
| 1 | Desktop Shell: 48px rail, context sidebar, workspace tabs, inspector, composer |
| 2 | Runtime Observatory: Execution view, Activity, Runtime Inspector, `QueryObservability` |
| 3 | Engineering workflow: Changes/Verification/Review (mostly already shipped) |
| 4 | Multi-Agent graph, session replay |

---

# 17. Acceptance Criteria

## Layout

- [ ] Desktop Application Shell
- [ ] 48px Application Rail + context sidebar
- [ ] Dynamic Inspector (waiting/running/terminal)
- [ ] Workspace tabs: Conversation / Changes / Execution

## Runtime

- [ ] Session runtime visible without Grafana
- [ ] Execution Timeline from durable observatory (not window-local tools)
- [ ] Tool lifecycle visualization (call_id pairing)
- [ ] Verification state (existing snapshot)

## Engineering

- [ ] Changes workspace (file list + focused diff)
- [ ] Diff review
- [ ] Artifact navigation (checkpoints / files)

## Future Ready (not Phase 1)

- [ ] Multi-Agent UI extension
- [ ] Session Replay
- [ ] Runtime Debugging beyond Activity

---

# Final Goal

Not a prettier ChatGPT. An environment where the user can **understand,
observe, control, and verify** what the agent did.

WebUI validates React + runtime events first. A later Tauri/desktop
shell should embed the same SPA and the same protocol, not a third
client model.
