# Multi-Agent MA5 — Mobile Closure

Status: **PASS for Mobile; Push DEFERRED by user decision** (MA0 §9a, D2).
Scope from `docs/MULTI_AGENT_MA0_REALITY_AND_GAP_AUDIT.md` §9 (REAL_MA5_SCOPE
item 1). Base: MA3 PASS at `b2e0d12`.

## 1. Contract

The phone reads a child only from the runtime's facts — the snapshot's
`children` and the typed live events (`sub_agent_updated` with
`outcome`/`stop`, `sub_agent_state_changed`, `sub_agent_progress`) — and has
one control over it, `cancel_child`. Nothing is derived from prose.

| Rule | Where |
|---|---|
| a settled child is final: no live event or snapshot reopens it | `SessionState._upsertSubAgent`, `_restoreChildren` |
| a snapshot merges: a child started after it keeps its row | `_restoreChildren` |
| an unknown child (no id, or a state change for a child never seen) marks the view stale; nothing is invented | `_upsertSubAgent`, `sub_agent_state_changed` |
| one entry per child: an accepted `spawn_agent` call is not shown beside its child row; a refused spawn still shows | `tool_call_started` / `tool_call_completed` |
| runtime notices in history (`kind: runtime_notice`) render as notices, not user input | `applySnapshot` |
| status line from `state`, `outcome`, `stop`; the `ok` bit only for a terminal recorded before outcomes were typed | `ChildAgent.statusLabel` |
| stop only on a running child, only for a pairing that may send commands, once | `ChildrenSheet`, `ChildAgent.canCancel` |

## 2. Changes

| Commit | Change |
|---|---|
| `99e27ec` | `SessionState.children`, typed labels, snapshot merge, de-dup, runtime notices, children strip + sheet with bounds/usage/stop, `Commands.cancelChild`, `children_test` journey |
| `06714ea` | **Runtime bug found by this acceptance**: the interactive chat turn (`run_in_session_with_content`, the turn TUI, Web and phone submit) had no steering source, so `CancelChild` never found a handle and a mid-turn steer was never injected. Both are pinned red→green by `crates/leveler-app/tests/mid_turn_controls.rs` (a child cancelled through `InProcessRuntimeClient`; a steer reaches the conversation). |
| `c6a3b66` | Review follow-ups: no invented child for an id-less update; snapshot redraws child rows; disabled `停止中…` after a press; a long notice wraps (a 92 px overflow seen on the simulator); simulator script fixes (work dir under `/tmp` for the socket path, projects registry at `state/web/projects.json`) |
| `48f8533` | A view left stale by a mid-answer reconnect asks for a snapshot when the turn ends (the resync banner stayed up forever before) |
| `3544129` | **Runtime race found by CI**: a child's cancel handle reached the host only after its start was flushed, so a stop sent the moment a child appeared could be told the child did not exist; the handle is now registered before the start is emitted (`a_child_is_cancellable_by_the_time_its_start_is_observed`). The app-level background cancel test, which raced the mock server's response order, was removed with the reason recorded. |

MA3's TUI `x` and Web 取消 go through the same `CancelChild` handler, so
`06714ea` also makes those controls work on a chat turn; MA3 had verified
them by unit tests only (MA3 §5 residual), which is how the bug survived.

## 3. Tests

`flutter analyze`: no issues. `flutter test`: 78 passed —
`test/children_test.dart` (typed state, merge, reconnect, de-dup, notices,
stale-view resync, command shape), `test/children_panel_test.dart` (strip
counts, stop only on a running child, read-only pairing, `停止中…`, recorded
facts), `test/chat_rendering_test.dart` (long notice at phone width).

## 4. Real acceptance

`scripts/simulator_pairing.sh` with `HOST_CONFIG` (real config) and
`JOURNEYS=children_test`, iPhone 16 Pro simulator, real relay, real
`leveler remote agent`, `leveler serve` built at `c6a3b66`, model
`deepseek/deepseek-v4-flash`. The app drives pairing with the terminal
confirming, then:

| Step | Evidence |
|---|---|
| a background explorer is spawned | host `sub_agent_started` 06:29:01; strip `子 Agent · 1 未结束` on screen |
| one entry | one `subAgent` timeline row, no `启动子 Agent` tool row |
| reconnect | a fresh `AppController` over the same keystore, new socket, reopens the session: the child comes back from the snapshot as `running`, `explorer`, read-only |
| stop from the phone | audit `cancel_child delivered` 06:29:05; host `sub_agent_finished` 06:29:05.175 `incomplete_no_result / cancelled`; phone shows `已取消` |
| parent continues | host `task_finished completed / answered` 06:29:07 |
| view consistent afterwards | resync completes; child still `cancelled`; no layout overflow in the run log |

The three runs before the fixes are part of the record: the first failed on the
script's stale registry path and socket length; the second reached the stop
and showed the child running on for 60 s to `completed` (the `06714ea` bug);
the third failed its final consistency check (the `48f8533` bug). The passing
run is the fourth, after both fixes, with no change to what the journey
asserts about the child.

## 5. Residuals

- Snapshot vs live ordering: a snapshot older than a live `interrupted`
  could show a child as running until the next event; the remote agent's
  ordering of snapshot replies against the event stream was not verified.
- Live `sub_agent_progress` tokens may be per activation; after a resume the
  snapshot total can be larger than the live value until the next snapshot.
- A stopped **background** child settles at its parent's next round
  boundary (MA1 settlement rule), so while the parent is inside a long model
  call every client still shows it running. In the accepted run that gap
  was under a second; it is bounded by one parent model call.
- Android is untested (no SDK on this host).
- Push: not built (D2).

## 6. Gates

```text
MOBILE_CHILD_STATE=PASS
MOBILE_RECONNECT=PASS
MOBILE_CONTROLS=PASS
MOBILE_PROTOCOL_ALIGNMENT=PASS
PUSH=DEFERRED_BY_USER_DECISION

MA5_PUSH_MOBILE=PASS (Mobile; Push deferred by user decision)
```

Exact main CI on `3544129` (MA5 head including both runtime fixes), run
`34814599065`, attempt 1: Linux, macOS, Windows `fmt · clippy · test`
success; web success; mobile analyze · test success; deny · audit success.
The run before it, `34813398623` on `c6a3b66`, failed on Linux in the
app-level background cancel test; that failure is the `3544129` row in §2.
