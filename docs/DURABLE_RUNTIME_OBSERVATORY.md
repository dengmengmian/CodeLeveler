# Durable Runtime Observatory

**Purpose:** inspect what a CodeLeveler runtime *actually did*, while it runs
and after the client or daemon has gone away. Not a second log, not telemetry
upload, not a debug dump of `RuntimeEvent`.

## Architecture

```
Canonical durable facts
  EventLog + model_requests + sessions + turns
        ↓
leveler-app::observability::query_observability
  (fail-closed decode of EngineEvent)
        ↓
ClientCommand::QueryObservability  →  RuntimeEvent::ObservabilityLoaded
        ↓
TUI /trace     CLI `leveler trace`
```

Current and historical sessions use the **same** `UiObservabilityLoaded`
payload. The TUI does not open SQLite. The CLI calls the same query function
the runtime command uses.

## Data sources

| Observation | Canonical source | Derived? | Durable? | Scope |
| --- | --- | --- | --- | --- |
| Session | `SessionRecord` | no | yes | session |
| Turn | `TurnRecord` | interrupted = no `finished_at` | yes | session |
| Event window | `EventStore::load_window` | presentation class | yes | **window** (bounded local navigation) |
| Requests | `model_requests` | sums/avg; display list capped at 200 | yes | **session** |
| Tools | `EventStore::load_by_types(tool_call_started, tool_call_finished)` then pair on `(call_id, agent_id)` | duration from timestamps of a matched pair only | yes | **session** (independent of the window) |
| Verification (counts) | `count_by_type(verification_started)` | no | yes | session |
| Agents tab | `sub_agent_started/finished` inside the window | no | yes | **window** (copy says 窗口) |
| Recovery counts | turns + `count_by_type` | interrupted = no `finished_at` | yes | session |
| Recovery review stages | `review_stage` inside the window | no | yes | **window** |

Transient (not in EventLog): token-usage stream, reasoning/text deltas,
sub-agent live tokens. After restart those are gone; request rows remain.

## Security

- Query is **read-only**. Remote clients are **denied** (`runtime observatory is local-only`).
- Default rows never include raw tool bodies, prompts, or `ContextSnapshot` messages.
- Payload strings were already redacted at EventLog write.
- No telemetry export.

## Navigation (TUI)

`/trace` — current session. `/trace <session-id>` or Sessions list `t` —
historical session, same screen.

`1–6` tabs · `Tab` · `↑↓` · `Enter` inspect (re-query window around seq) ·
`f` filter · `r` refresh · `Esc` back.

## CLI

```
leveler trace [session-id] [--seq N] [--before 20] [--after 80] [--json]
```

## Window vs session aggregates

```
Event Window  = bounded local trace navigation (latest N, or center_seq ± before/after)
Tool Summary  = whole-session aggregation over tool lifecycle rows
Requests      = whole-session `model_requests` (not the window)
```

The TUI never receives the full EventLog. Tool Summary is computed in
`query_observability` after `load_by_types`. Unfinished starts (`started`
without `finished`) count as `unfinished`, not success, and do not invent
duration.

## Performance

Window default 80 events from the tip; max 100 either side of a center seq.
Request list truncated to 200. Tool Summary scans only
`tool_call_started` / `tool_call_finished` (indexed `session_id, type,
sequence`). TUI closed: zero extra work (no live ring buffer). Corrupt
EventLog rows fail the query rather than skip.

## Known limits

- Sub-agent token totals after restart: not durable.
- Model TTFT / request id on tools: not recorded.
- Owner epoch / fencing: not on the protocol (intentional).
- Agents tab and Recovery *review stages* are still window-local; their
  copy says so. Overview counts (verify / repair / snapshots) are session-wide.
- Causal links only for `call_id` (+ `agent_id`), sub-agent `id`, and same `turn_id`.
- No OTel, export, or full-text search.

## Closeout status

| Gate | Status |
| --- | --- |
| Implementation | PASS |
| Automated durability (storage reopen / query reconstruction) | PASS |
| Session-wide tool aggregation | PASS |
| Real daemon restart / reconnect / PTY-driven TUI + CLI historical query | PASS |

Real-runtime notes (2026-08-19, isolated `LEVELER_HOME`, `target/debug/leveler`):

- `leveler serve --auto-approve` became ready; RuntimeId survived SIGINT restart.
- A PTY-driven `leveler tui --auto-approve` created a session against that daemon; a live DeepSeek turn persisted `read_file` start/finish + `model_requests`.
- After daemon restart, `leveler trace <id>` reconstructed the same session, window, requests, and **session-wide** tool summary.
- Client disconnect: TUI `/quit` while the session was `running`; the daemon finished it (`list_files` + two `read_file`); `leveler trace` showed the events that happened after the client left.
- Read-only smokes did not start a verification run (`review not_required` / `CompletedUnverified`). That is the product outcome for no mutation, not a missing observatory query.
