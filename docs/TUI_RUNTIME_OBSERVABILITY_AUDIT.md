# TUI Runtime Observability Audit

**Date:** 2026-08-19  
**Scope:** first `/trace` screen. Projection only. No second fact system.

Canonical facts stay in Engine EventLog / RuntimeEvent. The TUI may only
*observe* what `leveler-client-protocol` already carries.

```
Canonical Runtime Facts
        |
        v
Observability Projection  (TUI presentation, bounded, not persisted)
        |
        v
TUI /trace Screen
```

---

## A. Existing Data Sources

Filled from current code (`leveler-client-protocol` `RuntimeEvent` /
`UiSessionSnapshot`, TUI `AppState` / `TranscriptState`). Not guessed.

| Fact | Source | Durable | TUI available | Notes |
| --- | --- | --- | --- | --- |
| Session id / repo / branch | Snapshot + `SessionUpdated` | yes | yes | Header already |
| Session status | Snapshot `status` | yes | yes | mapped to `RuntimeStatus` |
| Model + provider | Snapshot `model` | yes | yes | `model_label` |
| Reasoning effort | Snapshot `reasoning.effective` | yes | yes | read-only |
| Work profile / collaboration | Snapshot axes | yes | yes | |
| Permission | Snapshot `mode` | yes | yes | |
| Goal mode | TUI flag from `/goal` | no (client) | yes | not a GoalId machine |
| Turn busy / idle / error | `RuntimeStatus` + turn terminal events | yes (turns) | yes | no protocol Turn number |
| Turn elapsed | TUI `elapsed_secs` / `turn_started_at` | no | yes | client clock |
| Last sequence | Snapshot `last_sequence` | yes | snapshot only | not kept on AppState today |
| User / assistant messages | RuntimeEvent + snapshot messages | yes | yes | transcript |
| Tool start | `ToolCallStarted` | yes (engine) | yes | name, args JSON, parallel |
| Tool end | `ToolCallCompleted` | yes | yes | `ok`, truncated `preview`, `duration_ms` (client-measured) |
| Tool taxonomy | TUI `tool_taxonomy::ToolKind` | n/a | yes | presentation only |
| Plan | `PlanUpdated` / snapshot | yes | yes | |
| Verification | `VerificationUpdated` / snapshot | yes | yes | check names/status/evidence |
| Diff / files changed | `DiffUpdated` / snapshot | yes | yes | |
| Token usage (last round) | `TokenUsage` | transient | yes | input / output / cached; **replaces**, does not sum |
| Context estimate | `ContextUpdated` | no | yes | placeholder until TokenUsage |
| Compaction | `ContextCompacted { from, to }` | yes | status only | count not stored |
| Context expanded | `ContextExpanded` | yes | ignored in chrome | |
| Checkpoints | `CheckpointCreated` / snapshot | yes | yes | |
| SubAgent lifecycle | `SubAgentUpdated` | yes | yes | id, nickname, role, done, ok, detail |
| SubAgent tokens | `SubAgentProgress` | transient | yes | |
| SubAgent step | `SubAgentActivity` | transient | yes | |
| Background tasks | `BackgroundTaskStarted/Exited` | yes | transcript | duration on exit |
| User shell | `UserShell*` | partial | yes | not an agent tool |
| Approvals / clarifications | RuntimeEvent | live waiters | yes | |
| Command progress | `CommandProgress` | no | activity line | |
| Agent activity label | `AgentActivity` | no | activity line | |
| Turn terminal | `TurnCompleted/Answered/…` | yes | transcript TurnEnd | |
| Turn progress / no-progress | `TurnProgress` | additive | activity line only | phase, streak; **not stored** |
| Stream deltas | `AssistantTextDelta` / `ReasoningDelta` | no | live buffer | **not for trace** |
| Model **request** id | — | — | **no** | no `ModelRequestStarted` |
| Model **latency** | — | — | **no** | do not estimate |
| Per-request token **history** | TokenUsage is last-round only | no | **partial** | can accumulate live in TUI |
| TTFT / retries / stop reason | Engine `TurnFinished.stop` | engine | **not on RuntimeEvent** | |
| Recovery owner epoch / fencing | engine internal | yes (engine) | **no** | do not leak |
| Resume / reconcile / replay | engine EventLog | yes | **not on protocol** | |
| Runtime id | — | — | **no** | |
| Task id | engine; snapshot 1:1 session | yes | session id only | |
| Full system prompt / request body | provider | — | **must not show** | |
| EventLog rows | `leveler-storage` / engine | yes | **TUI must not query** | |

---

## B. Existing TUI Surface

| Piece | Role for `/trace` |
| --- | --- |
| `AppState` | Domain facts the reducer already keeps. Add **presentation-only** `TraceView`. Never persist it. |
| `Screen` | New `Screen::Trace`. Architecture: “新增全屏 Screen → `screen.rs` + `render/`，不动 conversation”. |
| `transcript` | Tool groups, sub-agents, turn-end. Source for Tools / Agents tabs. |
| `activity_stream` + `tool_taxonomy` | Reuse `ToolKind` / `lookup` for READ/SEARCH/EDIT/SHELL. Do **not** add a second name switch. |
| `workbench` | Conversation only. Trace is a full-screen view like Tools/Help. |
| `overlay` | Not used for Trace. Inspect is an in-screen pane. |
| `reducer/runtime_apply.rs` | Feed each `RuntimeEvent` into the projection **before** existing apply. |
| `reducer/submit.rs` | `/trace` is a TUI navigation command (`BusyPolicy::Always`). No `ClientCommand`. |
| `reducer/screen_nav.rs` | Esc back to Conversation; Tab/1–6; ↑↓; Enter inspect; `p` pause follow. |
| `conversation::geometry` | **Do not reuse.** Trace has its own scroll/follow/selected. |

`/trace` is the same class of command as `/tools` / `/help`, not `/web` (no server) and not a model prompt.

---

## C. Gaps

### ALREADY AVAILABLE

Turn busy/terminal, tools + duration, verification, plan, diff file count, last-round tokens, context %, sub-agents, checkpoints, goal-mode flag, axes, model, permission.

### AVAILABLE BUT NOT PROJECTED

Live sequence of meaningful events (TUI currently applies then drops the event). Last-round `TokenUsage` history (can ring-buffer in the TUI). `ContextCompacted` count. `TurnProgress` phase/streak. `last_sequence` from snapshot.

V1 projects these in TUI memory only.

### REQUIRES READ-ONLY PROTOCOL ADDITION (not in V1)

| Missing | Why not invent |
| --- | --- |
| Model request id / latency / TTFT / stop | no request-level RuntimeEvent |
| Historical EventLog query | TUI must not open SQLite; no read-only query command yet |
| Owner epoch / fencing / resume/reconcile | engine-internal; not a product leak without a designed snapshot |
| Turn ordinal | not on protocol; counting TUI terminals is not a runtime Turn id |
| Token **sum** across rounds | TokenUsage replaces; summing last-round snapshots would double-count overlapping prompts |

### NOT WORTH ADDING IN V1

OTel export, raw prompt, full tool args/results, second telemetry event type, GoalId/window DB, request TTFT, repeated-read heuristic if target cannot be parsed as a single path.

Repeated reads: **V1 only if** `read_file` args yield a JSON `path` string. Count exact `(name, path)` in the current transcript. Otherwise DEFERRED.

---

## D. Implementation Plan

1. **Reuse facts** — `RuntimeEvent` stream the TUI already receives; transcript tools/sub-agents; snapshot fields.
2. **Derive projection** — bounded `TraceView` in `AppState` (`observability/`). Classify via existing `ToolKind`.
3. **No query command** — V1 is live (+ current transcript). Historical EventLog = DEFERRED.
4. **No new protocol fields.**

Honest limit: opening `/trace` after a long daemon-backed session only shows events **this TUI process observed**, plus whatever the current transcript still holds. Do not claim full session EventLog replay.

---

## Screen placement

```
Screen::Trace
  tab: Overview | Trace | Requests | Tools | Agents | Recovery
```

P0–P1 all ship if data exists; empty sections say the fact is absent, never pad with guesses.
