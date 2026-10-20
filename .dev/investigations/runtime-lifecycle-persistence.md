# Runtime Lifecycle & Persistence Audit — 2026-10-20

Scope: the three reliability gaps named for this round. Read-only audit first; no
runtime behavior was changed before this document.

## 0. Summary

| Area | Root cause | Owner |
| --- | --- | --- |
| Desktop reconnect | Electron Main never respawns a lost `desktop-bridge`; the Node bridge marks itself permanently closed. The daemon-level transport already revives a killed daemon, but the UI has no connection state and cannot distinguish a dead bridge from a dead daemon. | `apps/leveler-desktop/src/bridge.cjs`, `main.cjs`, `renderer.mjs` |
| Plan persistence | The plan is persisted as a durable `plan_updated` event, but `snapshot.plan` deliberately hides any plan older than the latest `task_finished`. After a normal completed turn and a restart, the projection therefore returns `None`. The Desktop also discards the plan computed from history replay. | `crates/leveler-app/src/interactive.rs` snapshot, `apps/leveler-desktop/src/renderer.mjs` |
| Legacy SQLite | Global task discovery opens foreign stores read-only and runs current-schema SQL without any schema gate. A store last written before migration 0025 lacks `turns.owner_boot_id`, so `session_facts` fails and the whole source vanishes from the list. | `crates/leveler-storage/src/session_facts.rs` |

## 1. Daemon lifecycle chain (as built)

```
Renderer (sandboxed)
  window.desktop.*                  apps/leveler-desktop/src/preload.cjs
        |
        v  ipcMain.handle
Electron Main                       apps/leveler-desktop/src/main.cjs:83,99-147
        |
        v  child_process.spawn(binary, ['desktop-bridge'])
Node Bridge (JSONL)                 apps/leveler-desktop/src/bridge.cjs:3-45
        |
        v  stdin/stdout lines       crates/leveler-cli/src/desktop_bridge.rs:338-373
Rust desktop-bridge process         crates/leveler-cli/src/desktop_bridge.rs:350
        |
        v  leveler-runtime-host library
ensure_default_runtime / adopt      crates/leveler-runtime-host/src/client.rs:980-1171
        |
        v  Unix socket <home>/run/sockets/<repo-hash>.sock
leveler serve daemon                 crates/leveler-runtime-host/src/server.rs:91-137
        |
        v
sessions.db (source_id = absolute path)
```

Key facts:

- The "Runtime Host" is a library inside `desktop-bridge`, not a process. The
  daemon is `leveler serve`, spawned detached by the runtime host
  (`client.rs:1030-1057`). `execution-host` is a third process only for
  background services.
- A dead daemon is already handled below Electron: the local transport
  subscription loop reconnects every 200 ms and `DaemonReviver` respawns it
  (`crates/leveler-local-transport/src/lib.rs:1716-1791`,
  `crates/leveler-runtime-host/src/client.rs:1132-1171`). Safe requests are
  revived and replayed once; unsafe requests return `outcome_unknown`
  (`lib.rs:1199-1223`).
- A dead `desktop-bridge` is not handled anywhere above it. `Bridge.fail()`
  sets `closed = true` permanently and every later request rejects with
  "reopen Desktop to reconnect" (`bridge.cjs:19-34`). There is no respawn, no
  backoff, no session restore, and no renderer connection state.
- `error_payload` adds `state`/`next_step` (`desktop_bridge.rs:394-401`), but
  the Node bridge copies only `message`/`kind` (`bridge.cjs:16`); the
  structured lifecycle facts never reach the page.
- The renderer has no connection state machine. `loading` is rendered as
  "连接 Runtime…" (`renderer.mjs:234-236`); a dead bridge leaves the header
  showing the last task status. Transport errors share `#error-banner` with
  ordinary action errors (`renderer.mjs:255-259,6645-6651`).
- A request rejected after the child is already closed is classified
  `outcome_unknown` by `main.cjs:143`, even though nothing was ever written.

### Desired transitions (this round)

```
connecting -> connected -> disconnected -> reconnecting -> connected
                                                 |
                                                 +-> failed (manual retry)
```

Rules to enforce:

- Respawn only the owned child; the daemon is detached and must never be killed
  by Desktop shutdown.
- Restore by `open(source_id, session_id)` using ids already confirmed by a
  snapshot. Never auto-create a session; never replay a `deliver` that was
  written before the disconnect.
- Pending requests that were already written when the child died are
  `outcome_unknown`; requests not yet written are definitely not sent.
- Bounded backoff (no unbounded 200 ms polling in Electron Main).
- Daemon spawn races remain owned by the runtime host socket lock; Desktop
  restore must not spawn a second daemon by itself.

## 2. Plan persistence chain

```
update_plan tool                       crates/leveler-agent/src/update_plan.rs:136-175
  -> AgentEvent::PlanUpdated           executor/drive.rs:3280-3290
  -> EngineEvent::PlanUpdated          coding/event.rs:137, engine/event.rs:405
  -> durable events row `plan_updated` engine/log.rs:75-105 (not transient)
  -> EventBridge -> ui_plan            leveler-app/src/event_bridge.rs:18-35,862
  -> client event `plan_updated`       client-protocol/src/event.rs:352
  -> Desktop state.mjs:93              next.plan = event.plan
```

Restore path:

```
snapshot(session)                     leveler-app/src/interactive.rs:5789
  live.plan (this process only)       live_view.rs:21,134,149-166
  else latest durable plan_updated    event_repo.rs:311-335
  else compare with task_finished     interactive.rs:5873-5907
      terminal newer -> None
```

Root cause: the compare at `interactive.rs:5884-5886` treats every plan older
than the latest task terminal as inactive and drops it. A normal turn always
ends with `task_finished`, so a completed plan is null after any restart. This
is currently codified by `crates/leveler-app/tests/plan_snapshot_restart.rs:154-181`.

Additional gaps:

- The wire `UiPlan` has no lifecycle state (`progress.rs:49-54`), so an old
  completed plan cannot be distinguished from an active one.
- Desktop history replay computes `restored.plan` and then drops it
  (`renderer.mjs:6686-6691`).
- The composer dock compares `step.status === 'in_progress'` while the wire
  value is `running` (`renderer.mjs:1489,1493`; `progress.rs:24-38`).
- Compaction `SessionOpened` pushes use only `live.plan` (`interactive.rs:6803-6827`).

### Decision

Keep one durable authority (the append-only `events` table). Extend the
projection rather than adding a plan database:

- `UiPlan` gains `state: active | completed | interrupted` (serde default
  `active` for old runtimes).
- `snapshot.plan` returns the newest durable plan even when a terminal is
  newer, with its derived state. A newer `plan_updated` always wins by
  sequence, so a historical plan never overwrites a new one.
- Completed = latest terminal outcome is a completed/answered kind.
  Interrupted = terminal is failed/cancelled/interrupted, or the turn was
  reaped. Active = no terminal after the plan row.
- No plan row means `None`; text is never parsed to invent steps.
- Desktop renders the state; an active plan keeps the live card, a completed
  or interrupted plan is shown as history, not as current work.

## 3. Legacy SQLite chain

```
global list                          leveler-app/src/global_task_index.rs:73-141
  -> Database::connect_read_only     leveler-storage/src/session_facts.rs:118-129
  -> SessionFacts SQL                session_facts.rs:57-83
       sessions.archived_at          migration 0015
       turns.owner_boot_id           migration 0025
       session_messages table        migration 0002
       events.schema_version         migration 0004
```

Root cause: the read-only projection always runs the latest query text. Any
store older than a referenced migration fails at parse time; the error is
swallowed per source (`global_task_index.rs:87-97`) and the entire store
disappears. This is deterministic for a 0015–0024 store: `archived_at` exists,
`owner_boot_id` does not.

New sessions work because their runtime path always write-opens the store
through `Database::connect`, which runs `sqlx::migrate!` (`database.rs:37-73`).
Read-only discovery explicitly never migrates (`session_facts.rs:1-2`).

Affected columns on the list path, in migration order: `events.schema_version`
(0004), `sessions.collaboration`/`work_profile` (0006), `sessions.archived_at`
(0015), `turns.finished_at` (0003), `turns.owner_boot_id` (0025), and the
`session_messages` table (0002).

### Decision

Schema capability detection in `session_facts`, not migration on the read path:

- Probe the actual columns/tables of the opened store once per scan.
- Build the projection from available columns; missing ownership/time columns
  project as `NULL` (the same meaning the schema documents: ownership unknown).
- Missing `session_messages` means no legacy first-user text; missing
  `turns`/`events` means no rows.
- Old stores stay unmodified until a runtime write-opens them; that migration
  remains the single migration authority and preserves all rows (0025 only
  adds nullable columns).
- Corrupt stores and stores with no `sessions` table still fail explicitly per
  source; they are never reported as empty success.

Migration safety to verify: 0025 is additive; sqlx applies each migration and
its bookkeeping row in one transaction; rerunning is idempotent. Tests must
cover pre-0025 fixture → read-only facts → resume through a migrating open →
row-for-row preservation.

## 4. Scope and verification

| Fix | Unit/component | Real runtime | Desktop | Dogfood |
| --- | --- | --- | --- | --- |
| Bridge respawn + session restore | bridge tests | bridge kill/restart | Playwright: kill bridge, assert reconnect | new Desktop case |
| Daemon auto-revive | existing transport tests | kill daemon, assert same session | Playwright: kill daemon, assert state | extend lifecycle gate |
| Plan state projection | snapshot tests | restart runtime, reopen session | Playwright: plan after restart | new Plan case |
| Legacy SQLite | session_facts tests with pre-0025 fixture | open/resume legacy store | list shows source | extend cross-version gate |

Existing tests that encode old behavior will be updated only where the product
contract is intentionally changed:

- `plan_snapshot_restart.rs` "terminal plan is not resurrected" becomes
  "terminal plan is returned as completed, never as active".
- `recovery.mjs` "reopen Desktop" becomes "automatic reconnect and restore".
- `bridge.test.mjs` child-exit expectations become respawn-or-fail behavior.

No test is changed before the corresponding failing case is written against
the new contract.
