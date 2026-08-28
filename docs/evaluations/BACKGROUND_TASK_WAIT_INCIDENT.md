# Background Task / wait_task Responsiveness Incident

**Date:** 2026-08-28  
**Session:** `71a092a8-6858-4ee8-a63e-419ed7fc2883`  
**Runtime:** `f4959049-25ec-43e4-b9f4-e29eed99117a`  
**HEAD:** `366f7ad936d9038688b41e364e6042f277b914ab` (`origin/main`)

Forensics first. The live process was not killed. No TaskRegistry rewrite.

---

## 1. Repository state

| | |
| --- | --- |
| HEAD | `366f7ad` |
| origin/main | `366f7ad` (identical) |
| Working tree | untracked `evals/baselines/c23a-treatment.log` only |

No overlapping background-task changes landed on origin during this audit.

---

## 2. Live incident

`LIVE_INCIDENT_AVAILABLE=YES`

Captured 2026-08-28T03:40:44Z without restarting the daemon or signalling cargo/rustc.

| Field | Value |
| --- | --- |
| session_id | `71a092a8-6858-4ee8-a63e-419ed7fc2883` |
| turn_id | `3f812fb8-8844-492b-aaec-e3515e22481f` (ordinal 3, `status=running` since 03:04:35Z) |
| goal_id | none (collaboration=`chat`) |
| runtime_id | `f4959049-25ec-43e4-b9f4-e29eed99117a` |
| current tool | `wait_task` call_id `call_00_ET_UlXHP5tZzlbitczOKwX38470` (seq 534, started 03:36:23Z, no `tool_call_finished`) |
| wait_task target | `bg-2` |
| timeout this call | `timeout_seconds=900` |
| TUI | `leveler` pid 8277 on ttys002; child `leveler serve` pid 8278 |
| background command | `cargo test --workspace --offline` |
| command PID / PGID | 10038 / 10038, parent 8278 |
| workspace | `/Users/mengmian/Develop/app/dengmengmian/codeleveler` |
| sandbox | `~/.leveler/run/sandboxes/codeleveler-sandbox-1E0RvC` |
| started_at | `run_command(background=true)` 03:08:41Z |

### Primary facts

| Gate | Value | Evidence |
| --- | --- | --- |
| `PROCESS_ALIVE` | **YES** | pid 10038 still listed at +32m; rustc children compiling into `target/debug/deps`; newest rlib `libsha2-*.rlib` at 11:41 local |
| `TASK_STATUS` | **RUNNING** | `get_task bg-2` at 03:36:21Z: `status: running`, `exit_code: None`, `duration_ms: 1659855` |
| `WAIT_TASK_PENDING` | **YES** | seq 534 `tool_call_started` with no matching `tool_call_finished` |
| `LAST_OUTPUT_AGE` | seconds | rlibs still appearing; `ACTIVE_PROGRESS` |

Decision table: **Case A** — internally consistent. The 28m+ TUI line is a truthful projection of a still-running compile plus a wait that owns the AgentLoop.

### Wait timeline (`bg-2`)

| UTC | Tool | Result |
| --- | --- | --- |
| 03:08:41 | `run_command` cargo test --workspace --offline, background | `task_id: bg-2` |
| 03:12:02 | `wait_task` timeout=120 | 03:14:02 `is_error=true` preview `wait timed out` |
| 03:14:04 | `get_task` | running, duration_ms=322839, compiling |
| 03:16:14 | `wait_task` timeout=600 | 03:26:14 `wait timed out` |
| 03:26:17 | `get_task` | running, duration_ms=1055524 |
| 03:26:19 | `wait_task` timeout=600 | 03:36:19 `wait timed out` |
| 03:36:21 | `get_task` | running, duration_ms=1659855 |
| 03:36:23 | `wait_task` timeout=900 | still pending at 03:40:44 |

Prior `bg-1` (`cargo test --workspace` without `--offline`) was killed at 03:08:39 after hanging on `Updating crates.io index` (sandbox denies network). That kill path worked.

The user message in the screenshot (“这是卡住了吗？没有下一步了？”) arrived while seq 534 was blocking. No assistant reply is possible until that `wait_task` returns.

---

## 3. Current lifecycle (code, not schema)

Owner: `leveler-execution::background::BackgroundTaskRegistry`  
Tools: `leveler-tools::tools::task_control` (`get_task` / `wait_task` / `kill_task`)  
Spawn: `run_command` / `shell_command` with `background=true`

```
run_command(background=true)
    memory: allocate bg-N, insert TaskInner Running
    durable: none (registry is process-memory)
    spawn Command, stdout/stderr piped, unix process_group(0), kill_on_drop
    log pumps (2) + wait reaper (child.wait)
        ↓
wait_task(task_id, timeout_seconds?)
    await: Notify::notified(), sleep(timeout), cancellation  (select, biased)
    timeout default 600s, min 1, NO max
    timeout → Err("wait timed out")  — no snapshot
    cancel  → Err("wait cancelled")  — does not kill the process
        ↓
process exit
    process_done=true, record exit_code
    terminal ONLY when process_done && log_pumps_remaining==0
        ↓
finalize_if_drained: Running→Exited or Killing→Killed (first-wins)
    done.notify_waiters()
        ↓
wait future completes → get() snapshot → ToolOutput
```

| Boundary | durable? | await? | cancel-aware? | timeout? | owner |
| --- | --- | --- | --- | --- | --- |
| spawn / Running | memory | no | n/a | n/a | registry |
| log pumps | memory | read until EOF | no | no | spawned tasks |
| process wait | memory | `Child::wait` | kill path | no | reaper task |
| terminal status | memory | after both pumps + wait | n/a | n/a | `finalize_if_drained` |
| `wait_task` | memory | Notify + sleep | **yes** | **yes, default 600s, unbounded max** | tool / AgentLoop |
| `get_task` | memory | lock only | n/a | n/a | tool |
| `kill_task` | memory | SIGTERM then SIGKILL on pgid | n/a | n/a | tool |
| daemon drop | memory | KillOnDrop SIGKILL tree | n/a | n/a | last registry handle |

Terminal first-wins: `finalize_if_drained` leaves an already-terminal status unchanged.

Missed-wakeup: `wait()` clones Notify, `enable()`s `notified()`, then re-checks terminal under the lock. This is the correct Notify ordering. Timeouts in this session returned on schedule (120s / 600s / 600s). `WAITER_WAKEUP_CORRECT=YES` for the incident.

---

## 4. wait_task contract today

```
WAIT_TASK_CURRENT_SEMANTICS=
  Block the tool call until the task is terminal, the wait is cancelled,
  or timeout_seconds elapses (default 600, min 1, no max).
  Timeout returns the string "wait timed out" as a tool error.
  Timeout does not kill the background process and does not include
  status / elapsed / log.

WAIT_TASK_HAS_BOUND=YES
WAIT_TASK_BOUND=timeout_seconds default 600s; caller may pass 900+; no cap
WAIT_TASK_BLOCKS_AGENT_LOOP=YES
  Duration: minutes (default), hours (if the model passes a large timeout),
  not literally infinite unless timeout is omitted at the registry API
  (the tool always passes Some(timeout)).
WAIT_TASK_CANCELLABLE=YES
  registry.wait select includes cancellation.cancelled() first (biased).
```

`get_task` can return running status + log while `wait_task` is blocked — but the model cannot call `get_task` until the current `wait_task` returns. That is why the user saw a frozen “等待任务” for the whole 900s slice.

---

## 5. Process / pipe termination

Design is:

```
process exit known
    ↓
drain stdout EOF and stderr EOF
    ↓
terminal status + notify waiters
```

Not: terminal-on-exit then best-effort drain.

`PIPE_DRAIN_CAN_BLOCK_TERMINAL=YES` in principle: a descendant that inherits the pipes can delay EOF after the spawned pid has been `wait()`ed. That is **not** this incident — pid 10038 is alive and rustc is producing artifacts.

`kill_task` does not notify waiters itself. Waiters wake when the reaper + pumps reach `finalize_if_drained`. Existing tests: `kill_running_sleep`, `kill_after_reaper_takes_child_terminates_process`.

Background tasks are **not durable**. Daemon death: last `BackgroundTaskRegistry` handle Drop signals the process group. Clones (per-turn) must not reap — tested.

Sandbox: `ps` inside the command sandbox cannot see host cargo. The model has no independent process oracle; `get_task` is the only truthful view. Do not weaken the sandbox.

---

## 6. Root cause

```
INCIDENT_CLASS=MULTIPLE_FACTORS
  primary:   LEGITIMATE_LONG_RUNNING_TASK
  secondary: WAIT_TASK_UNBOUNDED_RESPONSIVENESS_GAP
```

The compile is healthy. Timeouts fired. Registry state matches the OS.

What harmed the product:

1. One `wait_task` owns the AgentLoop for the entire interval. Default 600s; this call requested 900s with no cap.
2. Timeout is an **error string** with no snapshot. The model then `get_task` + `wait_task` again with a longer timeout (120 → 600 → 600 → 900).
3. The user cannot get an answer to “卡住了吗” until the wait returns.
4. The TUI is honest: it shows the current tool. Changing the TUI label would hide the contract.

Classification: **BETA_REQUIRED** (responsiveness), not a terminal-state-loss blocker.

Not a TaskRegistry rewrite. Not a TUI roster change.

---

## 7. Minimal fix (implemented)

Keep the registry. Change the wait **interval** contract only:

| | old | new |
| --- | --- | --- |
| Default interval | 600s | **30s** |
| Max interval | none (model passed 900) | **120s** clamp |
| Interval expiry | tool error `"wait timed out"`, no snapshot | **ok** running snapshot (status, elapsed, log) |
| Background process | not killed | still not killed |
| Mutation baseline | not taken on error (accidentally safe) | still not taken until terminal |
| Tool description | “Block until … exits (or timeout)” | bounded wait; may return while running |

Seams:

- `BackgroundTaskRegistry::wait` — sleep arm returns `Ok(snapshot)` instead of `Err("wait timed out")`.
- `WaitTaskTool` — clamp interval; Running/Killing snapshots are non-error and skip `take_mutation_baseline`.

Tests (isolated `CARGO_TARGET_DIR`, so they did not share `./target` with the live compile):

- `wait_interval_returns_running_snapshot_without_killing`
- `wait_cancel_returns_without_killing`
- `kill_wakes_a_pending_wait`
- `wait_interval_is_capped`
- `wait_interval_returns_running_as_ok_without_killing`
- existing `a_failing_background_command_is_reported_as_an_error_with_its_exit_code`

The live `cargo test --workspace --offline` was not killed. A second full-workspace compile in `./target` would have raced it.

---

## 8. What the 28m wait represented

Turn 3 started 03:04:35Z. `bg-2` started 03:08:41Z. At screenshot time the **turn clock** was ~28–30 minutes and the **current tool** was `wait_task` (7m into a 15m cap on the last call). Both numbers are consistent with a still-running workspace test compile, not a lost terminal state.
