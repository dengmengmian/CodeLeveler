# F6 · Implementation Audit (insertion points)

Audited at `c3bf11b` in a clean worktree. Phase 0's root cause is not repeated here — this document only fixes the minimal insertion points.

## Insertion points

| # | Concern | Exact location | Note |
| --- | --- | --- | --- |
| 1 | Model-visible boundary | `leveler-agent/src/executor/host.rs` — `ToolHost::dispatch_raw` (returns `(content, is_error, metadata)` from `registry.execute`) | The single choke point. `ToolHost::dispatch` delegates to it, and the parallel batch calls it directly, so every tool result — read_file, shell, browser, web, MCP, git — passes here exactly once. |
| 2 | Session scope for the agent | `admitted.ctx.session_scope: Option<Arc<str>>` (`leveler-tools/src/tool.rs:39`) | **Already present.** The registry can be keyed by session without threading any new parameter through `Executor`/`ToolHost` — which is why this gate needs no plumbing change. |
| 3 | Model-visible result assembly | `drive.rs` builds `ContentPart::ToolResult { content }` from the value returned by (1) | Sanitizing at (1) covers the model AND the provider request, because the same string is what enters `messages`. |
| 4 | Provider request | `messages` → `ModelRequest` in `executor/stream.rs` | No separate protection needed if (1) holds; asserted by test regardless. |
| 5 | Durable JSON boundary | `leveler-storage::redact_json_payload` (F2) — used by events, `session_messages`, turn payloads | Already the single durable JSON write boundary, and already structure-aware. Durable echo scrubbing belongs **here**, reusing F2's Value-walk so JSON structure stays intact. |
| 6 | Storage session id | `event_repo::append(session_id, …)`, `message_repo` and `turn_repo` equivalents all receive `&SessionId` | Available at every durable write site, so the registry lookup can be session-scoped. |
| 7 | Context snapshot / resume | `EngineEvent::ContextSnapshot` is built from in-memory `messages` and persisted through (5) | Sanitized content flows in; replay therefore returns sanitized content. |
| 8 | Diagnostics | `ModelRequestRecord` stores telemetry only (provider, model, token counts, finish reason, latency, retries) | Confirmed at `c3bf11b`: **no prompt bodies**, so there is no second durable path to protect. |

## Consequence for the design

Because (1) is a single choke point and (2) already carries the session, the architecture reduces to:

```
registry.execute(...)  →  raw ToolOutput
        │
        ├─ detect concrete secret values (value-position aware)
        ├─ register them under ctx.session_scope        ← Layer B input
        └─ return SANITIZED content                     ← Layer A
                │
                ├─► model messages ─► provider request        (protected by A)
                └─► events / session_messages / snapshots     (protected by A, and by B for anything else)
```

Layer B exists for text that never passed through (1) — most realistically a secret the **user** pastes into the goal, which the model may then repeat. It scrubs only exact registered values at the F2 durable boundary.

## Explicit non-changes

No new crate. No change to `Executor::new` or the factory builder. No tool-name lists. No second state machine. `ModelRequestRecord`, EventLog schema, and the F2 write contract are untouched.
