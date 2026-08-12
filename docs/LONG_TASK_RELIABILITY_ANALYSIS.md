# Long Task Reliability Gate — Phase 0 Architecture Analysis

Baseline: `main` @ `6983d51` (clean; verified `6983d51` is the tip, `7d92626` is an earlier ancestor).
Method: 3 parallel code-trace agents (goal lifecycle / turn ownership+reconnect / tool-event durability),
cross-checked against the empirical D4 + minimal-repro evidence. All citations are file:line at this HEAD.

> **Headline correction (read first).** The Phase 0 audit *changed the diagnosis of P1-B*. The D4 / repro
> "reconnect wedge" was **not** a daemon-mode coupling bug. Every D4/repro run used `--auto-approve`, which
> **forces in-process (Embedded) execution** (`run_cmds.rs:183-185`), so the turn ran *inside the TUI client
> process* and the `leveler serve` daemon was an **idle bystander** (confirmed by the v3 `daemon.log`: only
> "ready", zero session activity). Killing the client killed the process that was running the turn — that is
> why the tool never finished. The daemon's actual client-death resilience path was **never exercised**. This
> reframes the fix and is the single most important thing to confirm in Phase 1.

---

## 1. Current Goal lifecycle

- `/goal` (TUI `submit.rs:248,729-735`) → `ClientCommand::RunGoal{session_id,content}` (`command.rs:40-43`) →
  runtime `interactive.rs:1267-1274` → `spawn_goal_turn` → `spawn_direct_goal_turn` (`interactive.rs:917-927`).
  `SubmitMessage` with `collaboration=="goal"` converges on the same path (`interactive.rs:1253-1263`).
- Goal runs as **`run_direct`** (`engine.rs:1090`): exactly **one** initial `run_turn` (`engine.rs:1103-1114`),
  then `supervise()` (`engine.rs:1117-1126,1176-1241`).
- Goal profile: `TurnProfile::Goal{continuation,limits}` with `ContinuationPolicy::UntilTerminal`
  (`session.rs:415-417`), enabling the `update_goal(complete|blocked)` completion gate.
- **Goal identity = the session.** No `goal_id` exists anywhere (`rg goal_id` = 0 hits). Goal text is a column
  on the `sessions` row (`session_repo.rs:30`, `update_goal` at `:213`). No goal table, no window counter, no
  goal-level budget row.

## 2. Current Turn lifecycle & round-limit lifecycle

- Per-turn round ceiling: `const MAX_TURN_ROUNDS = 100` (`drive.rs:206`), overridable by
  `StepLimits::max_rounds: Option<u32>` (`executor.rs:379-382`) — but that override is **not** exposed to the
  interactive/goal path (`RunLimitsConfig` only has duration/tokens/cost, `config.rs:39-46`), so goal always
  uses 100.
- Enforcement (`drive.rs:393-411`): `if round >= round_ceiling { … return StopReason::TurnLimitReached }`.
- Chain to terminal: `TurnLimitReached` → `supervise` calls `after_turn` → `Continuation::Stop`
  (`continuation.rs:82`) → `conclude_direct` → `direct_non_success_outcome(TurnLimitReached) = Failed`
  (`engine.rs:1833-1835`) → `finish_task` writes `TaskFinished{outcome:Failed, stop:TurnLimitReached}`
  (`engine.rs:392-424`). Matches the observed payload exactly.

## 3. Current ownership & reconnect lifecycle

- **Daemon = one `InProcessRuntimeClient`** (`interactive.rs:147`), spawned **detached, own process group**
  (`run_cmds.rs:284-289`). Clients are separate `SocketRuntimeClient` processes over the per-repo Unix socket.
- **In-process vs daemon selection** (`socket_intent`, `run_cmds.rs:173-189`): `--in-process` → Embedded;
  explicit socket → RequireExplicit; **`auto_approve || config_overridden` → Embedded**; else → ProbeDefault
  (attach to a running daemon, else spawn one via `ensure_default_runtime`, `run_cmds.rs:262-297`).
- **Active turn ownership** (`active_turns.rs:17-20`): `ActiveTurns` holds only a `CancellationToken` per
  session — **not** the worker/JoinHandle. The worker is a **detached `spawn_blocking`** (`interactive.rs:1064-1086`,
  `950-973`) with no lifetime link to any client connection; it is `!Send` (borrows a `&mut FnMut` observer
  across awaits) so it runs on a blocking thread.
- **Client disconnect does nothing** to ownership: `handle_connection` on client EOF just returns Ok(())
  (`lib.rs:444,454`), dropping the broadcast Receiver + decrementing a UI-waiter count. No cancel/finish/reap.
- **Reconnect** = new client process → new `SocketRuntimeClient` → fresh `Subscribe{session_id}` (future
  events only, **no replay**, `interactive.rs:259-285`) + one-shot `Snapshot` (`interactive.rs:2057-2150`).
  Snapshot mixes durable DB (messages, status) with **in-memory `LiveViews`** (`active_tools`, `:2126,2142`).
- **Ownership fencing** (correct): durable `RuntimeId` (`runtime_identity.rs:55`); canonical writes are
  owner-token scoped (`EventLog::new_owned`, `finish_turn_owned` `turn.rs:431-442`); `reap_after_restart`
  refuses to reap another runtime's tasks (`interactive.rs:729-734`). Reconnect creates **no** second owner.

## 4. Current tool-stream lifecycle (durability)

- Shell tool runs in `dispatch`; a 3s heartbeat ticker (`COMMAND_HEARTBEAT_SECS=3`, `drive.rs:1409-1431,2511`)
  emits `CommandProgress` on the **same task** via a synchronous observer.
- Emission path: executor observer → `EventEmitter::emit` **non-blocking `try_send`** into a bounded mpsc
  (`recorders.rs:67-93`) → per-turn **pump** drains and calls `EventLog::append` (**durable INSERT**, awaited)
  → **then** `forward` broadcasts to LiveViews/clients (`log.rs:58-93`). **Persist strictly precedes broadcast**;
  broadcast is fire-and-forget (`event_bridge.rs:144`, `let _ = self.events.send(...)`).
- The pump is a per-turn consumer `futures::join!`ed with the executor on the **same task** (`turn.rs:364-398`).
- **No client-facing await between ToolCallStarted and ToolCallFinished.** Broadcast never backpressures the
  sender; the session broadcast has a **permanent in-process receiver** that folds every event into `LiveViews`
  regardless of clients (`interactive.rs:264-283`). The agent's `run_command` uses the **non-streaming** `.run()`
  (`run_command.rs:304`) — no per-chunk client channel at all (streaming `UnboundedSender` is user-`!command` only).
- Child process wait is bounded: `select! child.wait() | sleep(600s) | cancel` (`command.rs:963-972`).

## 5. Current reconnect lifecycle (the wedge path)

Empirically: kill client mid-streaming-tool → `command_progress` stops at the kill, `tool_call_finished` never
written, session stuck `running`, host at 0% CPU. Minimal repro (`sleep 40 && echo MARK`, session `5c43b164`)
+ D4 v2 (`go build`, 52 min) both reproduce; v3 (no kill) completes normally = control.

## 6. D4 wedge — ROOT CAUSE (reframed, confirmed)

**The turn ran in-process, not in the daemon.** `--auto-approve` → `SocketIntent::Embedded` (`run_cmds.rs:183`,
comment: "A running daemon cannot inherit these invocation-scoped settings"). All D4/repro drivers pass
`--auto-approve`, so the turn executed inside the TUI client via `spawn_content_turn`'s `spawn_blocking +
block_on` on the **client** process runtime (`interactive.rs:1063-1086`). The separate `serve` daemon was idle
(v3 `daemon.log`: only "Local runtime ready", plus `reaped zombie turns … reaped=1` at startup — it reaped a
*prior* wedge's dangling turn, proving it wasn't running the current one).

Mechanism (Agent-3 path, now the confirmed one):
1. Client SIGKILL destroys the process runtime hosting the turn.
2. `dispatch_fut` (the 600s command) and the 600s `tokio::sleep` timeout (`command.rs:963`) die **with the
   process** — the timeout can never fire, because it lives on the dead runtime.
3. `ToolCallFinished` is produced only when `dispatch` returns; it never does → never emitted → the pump (sole
   writer, `turn.rs:381`) never INSERTs it. `turn_finished`/`task_finished` never written → status stays `running`.
4. Durable at kill: `tool_call_started` + already-drained `command_progress`. Lost: `tool_call_finished`, tool
   result, `turn_finished`. → a **dangling tool call**, session frozen `running`.

**Recovery gap (independent, solid):** `reap_after_restart` is wired only to daemon (re)start
(`run_cmds.rs:714,851`), `Quit` (`interactive.rs:1886`), and cancel **when no live token exists**
(`interactive.rs:1849-1862`). A plain `--session` reconnect (Subscribe+Snapshot) reaps nothing. And the
existing dangling-detector `EventLog::dangling_tool_calls` (`log.rs:165-215`) is **not wired to reconnect** —
only to next-start reconciliation. So reattaching shows history but can never clear the zombie.

**Critical product gap surfaced:** auto-approve/config-override sessions are *forced* in-process
(`run_cmds.rs:183`), i.e. **unattended `/goal` — the sessions that most need to survive client death — currently
have zero daemon resilience.** The resilient daemon path (§3, detached worker) is unreachable for them.

## 7. D4 goal-ceiling — ROOT CAUSE (confirmed, high confidence)

Two deliberate sites suppress continuation on the ceiling:
- **Primary (fix locus): `continuation.rs:63-83` `DefaultSupervisorPolicy::after_turn`.** Only
  `StopReason::Stalled` (with progress budget) returns `DriveGoalAgain`; `TurnLimitReached` falls through to
  `Continuation::Stop` (`:82`). Pinned by test `continuation.rs:221-238`.
- **Companion: `engine.rs:1833-1835`** maps `TurnLimitReached → TaskOutcome::Failed`; comment (`:1828-1832`)
  says resuming would "re-enter the same loop the ceiling just broke" — but that concern is about **transcript
  resume** (`ExtendBudget`), *not* `DriveGoalAgain`, which opens a **fresh objective-restated window**
  (`drive_goal_again`, `engine.rs:1245-1321`).

**The multi-window machinery already exists** — `supervise` loop (bounded `MAX_SUPERVISED_TURNS=32`,
`engine.rs:364`), `DriveGoalAgain`/`drive_goal_again`, and a no-progress cap (`ProgressCaps::no_progress_rounds=2`,
`progress.rs:37`) — it is simply gated **off** for the ceiling stop. Turn count was 1 because `after_turn` said Stop.

## 8. Correct ownership boundaries (the invariants to hold)

A. **Runtime owns execution.** Client disconnect must not equal task death. (Today it does for auto-approve.)
B. **Session ownership stays fenced.** Single owner; stale runtime can't write canonical events (already true, §3).
C. **Persist before project.** Anything needed after reconnect must be durable, not TUI-memory (already true for
   events; the gap is that a dead in-process turn never *reaches* the finish write).
D. **At-most-once destructive execution.** Reconnect/reap must fail a dangling tool, never re-run it.
E. **Round budget ≠ Goal lifetime.** Per-window bound stays; the Goal spans multiple bounded windows.
F. **Continuation is explicit & bounded.** No `while(true)`; reuse `supervise`+`no_progress` caps + a goal budget.
G. **No second architecture.** Reuse session/engine/EventLog/EventBridge/supervise; add a goal identity + window
   budget + reconnect-reap wiring — do not fork a parallel long-task runtime.

## 9. Proposed minimal architecture

### P1-B (do first — reconnect/execution resilience)
1. **Make execution outlive the client for unattended sessions.** The real bug is that `auto_approve` forces
   Embedded. Options (Phase-1 to pick): (a) let a goal/auto-approve session attach to the daemon and pass the
   approval policy **per-session** (so "daemon can't inherit invocation settings" no longer forces in-process);
   or (b) keep Embedded but make the runtime **daemon-hosted** for goal turns. Target invariant A.
2. **Wire `dangling_tool_calls` reconciliation into `--session` reconnect** (`log.rs:165` already exists). On
   attach/resume: detect ToolCallStarted-without-Finished, **synthesize a failed ToolCallFinished** (at-most-once,
   invariant D), and let the turn recover or terminate cleanly instead of freezing `running`.
3. **Bound the two client-independent stall candidates** (defense-in-depth; real latent wedges even in daemon
   mode): `command_gate.lock_owned().await` (`run_command.rs:303`, no timeout/cancel arm) and the flush/side-effect
   barrier awaiting the DB pump (`recorders.rs:102-105` + `turn.rs:364-396`). Add cancel arms / timeouts.
4. **Cancel must force-reap even with a live token.** Today cancel-with-live-token cancels the token and skips
   reap (`interactive.rs:1850-1855`); if the worker is blocked upstream of a cancel checkpoint it's ineffective.

### P1-A (do second — goal multi-window)
5. **Add a `TurnLimitReached` branch in `after_turn`** returning `DriveGoalAgain`, gated by (i) material
   progress this window and (ii) a **goal-level work-window budget** (new). Keep `MAX_TURN_ROUNDS=100` per window.
6. **Introduce durable goal identity + window budget** (goal_id or reuse session + a `work_window_index` /
   cumulative budget columns) so window transitions survive reconnect (invariant C) and the no-progress /
   max-window caps are enforced across windows.
7. **Map terminal states properly:** `TurnLimitReached` mid-goal → `WindowExhausted` (continue) → only
   `Failed`/`Blocked` on unrecoverable / no-progress / budget-exhausted. Update the 3 tests that pin the old
   behavior (`continuation.rs:221-238`, `engine.rs:1768-1772`, `engine.rs:1726-1732`).

## 10. Explicitly rejected alternatives
- **Raise the ceiling (100→500/1000) or `while(true)`** — violates §22; masks the structural gap. Rejected.
- **Transcript-resume on ceiling (`ExtendBudget`)** — re-enters the same busy loop; the existing code correctly
  refuses it. Use fresh-window `DriveGoalAgain` instead.
- **Auto-replay the in-flight tool on reconnect** — violates at-most-once (invariant D). Reconnect **reaps/fails**
  the dangling tool; it never re-runs it.
- **A second long-task runtime / session / event pipeline** — violates §17/G. Reuse `supervise` + EventLog.
- **Rewriting the daemon/client transport for reconnect** — not needed; the transport already survives client
  death (§3). The gap is (a) auto-approve forcing Embedded and (b) no reconnect-reap.

## 11. Open questions Phase 1 MUST resolve before coding
1. **Confirm the in-process diagnosis empirically**: run daemon mode *without* `--auto-approve` (or with a
   session-scoped approval), kill the client mid-streaming-tool, and verify the daemon-hosted turn **completes**
   (tool_call_finished appears, goal continues). If it also wedges → there IS a daemon coupling Agent-2's static
   trace missed (revisit command_gate / flush-barrier as the live cause).
2. Decide the auto-approve→daemon delivery (per-session approval policy vs daemon-hosted goal runtime).
3. Confirm whether `dangling_tool_calls` reconciliation can run mid-session (live token present) safely, or needs
   an ownership handshake.

## 12. Test plan (Phase 1 gate = failing tests first)
- **Reconnect (P1-B):** integration tests A–G from the Gate — disconnect before/during/after tool, reconnect
  while active/after complete, multiple reconnects. First write the **failing** repro: in-process turn killed
  mid-tool → dangling tool never reconciled on `--session` reattach. Then daemon-hosted variant must survive.
- **Goal multi-window (P1-A):** G1–G15 — one-window completion (no spurious 2nd window), ceiling→Window-2,
  3+ windows same session/goal, ordinary chat does NOT auto-window, cancel between windows, no-progress finite
  stop, reconnect across a window boundary, no duplicate in-flight tool.
- **Invariants:** R1–R10 (client disconnect ≠ cancel; no duplicate destructive tool; ownership fenced; finish
  durable; single owner; reconnect continues; cancel works; quit intact).
- **Tripwires (§32):** round-limit can't terminal-fail a goal without a continuation-policy decision; non-goal
  sessions can't auto-open windows; reconnect can't create a second owner; in-flight tool can't replay.

## Confidence
- **P1-A**: HIGH. Root cause + fix locus pinpointed; machinery exists; low-risk minimal change.
- **P1-B**: root *mechanism* now HIGH (in-process turn death + confirmed via auto-approve→Embedded and the
  bystander daemon log). The *fix shape* has one decision (auto-approve→daemon delivery) that Phase 1's empirical
  check (Open Q1) should settle before implementation. The reconnect-reap gap and the two stall candidates are
  solid regardless.
