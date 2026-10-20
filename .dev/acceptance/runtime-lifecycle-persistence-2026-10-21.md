# Runtime lifecycle, plan persistence and legacy SQLite — acceptance 2026-10-21

Scope: the three reliability gaps named for this round. Internal report; raw
evidence lives in the isolated labs, not in Git.

## 1. Root causes

| Area | Cause | Owner |
| --- | --- | --- |
| Desktop reconnect | Electron Main never respawned a lost `desktop-bridge`; the Node bridge stayed permanently closed. A killed daemon was already revived by `leveler-local-transport`, but the page had no connection state, and a reconnect snapshot was replayed against a stale history buffer. | `apps/leveler-desktop/src/bridge.cjs`, `main.cjs`, `renderer.mjs` |
| Plan persistence | `snapshot.plan` dropped any plan older than the newest `task_finished` (`interactive.rs`). The durable `plan_updated` row was never lost; the projection classified it as hidden. The Desktop then replayed history and dropped `restored.plan`, and the replay re-marked the plan active. | `crates/leveler-app/src/interactive.rs`, `apps/leveler-desktop/src/state.mjs` |
| Legacy SQLite | Global discovery opens foreign stores read-only and ran the latest schema (`turns.owner_boot_id`) without a schema gate. A pre-0025 store failed at parse time and the whole source disappeared from the list. | `crates/leveler-storage/src/session_facts.rs` |

Full audit: `.dev/investigations/runtime-lifecycle-persistence.md`.

## 2. What changed

### Desktop reconnect

- `bridge.cjs`: owns child lifecycle. On exit it rejects in-flight requests with
  `outcome_unknown` (written) or `not_sent` (never written), emits
  `connection: disconnected/reconnecting/connected/failed`, respawns with bounded
  backoff (`500ms … 30s`, 8 attempts), and probes `ping` so `connected` is
  proven by a real answer. `close()` stops respawn permanently.
- `desktop_bridge.rs`: adds the liveness-only `ping` method.
- `main.cjs`: remembers the last confirmed `{source_id, session_id}`, and on a
  resumed connection restores it with `open` (never `create`), then forwards a
  `restored` frame. Exposes `connection-status` and `reconnect`.
- `renderer.mjs`: real connection banner, send disabled while disconnected,
  automatic restore, explicit retry after exhausted backoff. A stale
  snapshot/`session_opened` is ignored when its `last_sequence` is older than an
  event already applied; a reconnect clears the stale replay buffer and reloads
  durable history.
- No automatic replay of any command; a killed daemon is revived by the
  existing Runtime Host reviver, not by Desktop.

### Plan persistence

- `UiPlanState` (`active | completed | interrupted`) added to `UiPlan`
  (additive, serde default active; protocol minor 1.14 → 1.15).
- `interactive.rs`: one `projected_plan` used by `snapshot` and the compaction
  `SessionOpened` push. The newest durable `plan_updated` is returned; a later
  `task_finished` classifies it `completed` (Completed outcome) or `interrupted`
  (failed/cancelled/other), never active. A newer plan row wins by sequence.
- Desktop: the runtime snapshot owns the plan classification during history
  replay; live `plan_updated` is active; completed/interrupted plans render as
  history with a state label, and the composer dock only shows active work.
  History replay no longer drops `restored.plan`; the dock's `in_progress`
  comparison was corrected to the wire value `running`.
- TUI adopts a snapshot plan only while busy AND active.

### Legacy SQLite

- `session_facts` probes the opened store's actual columns/tables and projects
  missing columns with the values their migrations assign: `archived_at NULL`,
  `owner_boot_id NULL`, `finished_at NULL`, `events.schema_version 1`,
  `collaboration 'goal'`, `work_profile 'balanced'`, no `session_messages`.
- Read-only discovery never migrates; a store migrates only when a runtime
  write-opens it. Corrupt/foreign stores remain explicit per-source errors.
- Test-only `create_legacy_database` builds genuine pre-N stores for unit tests.

## 3. Verification

| Check | Result |
| --- | --- |
| `cargo test -p leveler-storage --lib` | 169 passed |
| `cargo test -p leveler-app --test plan_snapshot_restart` | 6 passed (completed/interrupted/active) |
| `cargo test -p leveler-local-transport --lib client_reconnects_and_resyncs` | passed; now also proves live events after resubscribe |
| `cargo test -p leveler-cli --bin leveler desktop` | 11 passed |
| `cargo test -p leveler-tui --test reducer plan` | 21 passed |
| `cargo test -p leveler-tui --test turn_activity_boundary` | 15 passed |
| `cargo test -p leveler-client-protocol --lib` | 123 passed |
| `npm test` (desktop) | 147 passed |
| `node scripts/recovery.mjs` (real Electron/Runtime) | PASS: daemon loss same session, bridge loss automatic, no page errors |
| `node scripts/views.mjs` (real Electron/Runtime) | PASS: plan after restart `state=completed`, 3 steps, card visible |
| Dogfood `desktop_resilience_gate.py` | 8/8 required cases PASS |

Dogfood evidence: `/Users/mengmian/Develop/app/dengmengmian/dogfood/.dev/acceptance/desktop-resilience-2026-10-21.md`.
The dogfood run recorded `qualification: insufficient_uncommitted_candidate`
because another session was writing the same checkout; it must be re-run on the
committed candidate before it can be used as a release gate result.

## 4. Data-flow after the fix

```
Plan update -> durable events.plan_updated -> newest by sequence
            -> terminal newer? -> completed | interrupted
            -> else active
            -> snapshot.plan (state) -> Desktop plan card/view
```

```
Desktop bridge dies -> Main respawns (bounded backoff) -> ping proves the pipe
 -> Main reopens the last confirmed (source_id, session_id) -> snapshot
 -> renderer clears stale history, applies the snapshot, reloads history
 (never re-creates a session, never replays a written command)
```

```
read-only discovery -> probe tables/columns -> legacy-compatible SELECT
 -> list shows the source, file unchanged
open -> daemon write-open -> sqlx migrations 25..40 -> same session/snapshot
```

## 5. Known limits / risks

- **Candidate not frozen.** Parallel WIP in the same worktree; the dogfood run
  is evidence about that working tree, not a release-qualified candidate.
- Daemon revival progress is reported by the Runtime Host after it succeeds
  (warning + fresh snapshot), not as a live per-retry progress bar. Bridge loss
  does show live reconnect progress.
- Legacy compatibility covers the documented additive migrations on the list
  path. A pre-0003 store projects the migration defaults (`kind=user`,
  `status=running`), which can look like an ownership-unknown running turn; it
  is not auto-interrupted.
- macOS only in this round; Windows/Linux not exercised.
- `.dev/` is gitignored in CodeLeveler; these reports are force-added only if
  the delivery explicitly includes them.

## 6. Commit and frozen-candidate evidence

- CodeLeveler: `b09c7fbd45c55ff662798dec84443d1f0e582f18` (includes the
  Terminal/pending-input work that shared the working tree).
- Dogfood: `e23b78a` (`test(eval): gate desktop reconnect, plan restore and
  legacy stores`).
- Frozen candidate binary: `28ea9c37aa822e836d63580f2a07379fefc954d201cc4a0329cd23ad2032606a`
  (debug build, version `1.0.20 (b09c7fbd45c5)`).
- Re-run after the commit: `desktop_resilience` 8/8 required cases PASS,
  qualification `candidate_frozen`. The earlier dirty-tree run is superseded.
