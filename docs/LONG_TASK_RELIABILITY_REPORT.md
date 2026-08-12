# Long Task Reliability Gate — Final Report

Branch `fix/long-task-reliability-gate` off `main @ 6983d51`. No target-repo push.
Deliverable commits: `84df004` (Phase 0 analysis) · `448f184` (R1) · `a374f50` (R2) · docs.

## Gate decision: PASSED (with an environment caveat on the D4 replay's final gate)

Both P1s are fixed, regression is green, and the D4 exact replay demonstrates the
fixes end-to-end: a goal that died at one window now runs the full stack across
four bounded windows, and a client kill mid-goal no longer wedges it. The replay's
final status is `incomplete` **only** because the sandbox blocks the Go 1.26.2
toolchain download that `go test` needs (env, per §28) — not the round ceiling,
not a wedge, and not a false completion.

## A. Baseline
CodeLeveler `6983d51` (main tip; `7d92626` is an ancestor). Env: Go 1.26.2
(GOSUMDB/GOTOOLCHAIN friction), Node v24.13.0, pnpm 11.0.1, macOS arm64.

## B. D4 evidence (first run, pre-fix)
D4 first run (`6983d51`, unattended `/goal`, memos favorites): `turns=1`,
`stop=turn_limit_reached`, backend-only, no frontend/i18n/tests. The reconnect
probe wedged. Root of both: see C/F.

## C. Reconnect root cause (reframed by Phase 0 controls)
NOT a daemon reconnect bug. `--auto-approve` forced `SocketIntent::Embedded`
(`run_cmds.rs:183`), so the goal's runtime lived in the TUI process; killing the
client killed the runtime, the 600 s tool timeout died with it, `tool_call_finished`
was never written → `running` + dangling tool. Controls A/B/C proved it: a
daemon-hosted turn survives a client kill (A), an embedded turn dies with it (B),
and a daemon reconnect continues cleanly (C).

## D. Reconnect fix (R1, `448f184`)
`--auto-approve` is now a per-session `ApprovalPolicy` on the trusted-local
`CreateSessionRequest`, not a reason to embed. `approver` auto-approves when the
session policy says so OR the daemon is globally `--auto-approve` (fallback).
`socket_intent` no longer embeds on `--auto-approve` (`--in-process` /
`config_overridden` still do). The web/remote boundary force-resets to
`Interactive` (§6.G). Old: kill client → kill execution. New: goal runs in the
daemon and survives client disconnect.

## E. Reconnect invariants
R1 client-disconnect ≠ cancel ✅ (R3: goal advanced headless 239→244 tools with
no client). R2 no duplicate in-flight tool ✅ (control C + PTY: started 1→1).
R3–R8 ownership fencing intact (unchanged; full test green). R9 cancel / R10 quit
unchanged. Per the controls, "reconnect during a streaming tool wedges the daemon"
was withdrawn — it was the embedded-runtime death, not a daemon coupling.

## F. Goal root cause (R2)
`/goal` = one `run_direct` turn; `after_turn` (`continuation.rs`) only re-drove on
`Stalled`, so `TurnLimitReached` fell to `Stop` and the outcome map turned it into
`Failed`. The multi-window machinery (`supervise`, `drive_goal_again`,
`MAX_SUPERVISED_TURNS=32`) existed but was gated off for the ceiling.

## G. Goal multi-window architecture (R2, `a374f50`)
`after_turn`: `TurnLimitReached` → `DriveGoalAgain` (next bounded window) unless
spinning across windows or a pinned round budget (evals). Window = one bounded
agent interval; the per-turn 100-round ceiling ends a window, not the goal.
Terminal states: Completed / Blocked / Cancelled; a ceiling stop is BudgetLimited
(incomplete + resumable), not Failed.

## H. Work-window policy
Per-window: unchanged 100-round ceiling (NOT raised — §22). Goal-level: in-memory
`windows_without_progress` (from cumulative modified-file growth) bounded by
`MAX_NO_PROGRESS_WINDOWS=2`, under the absolute `MAX_SUPERVISED_TURNS=32`. No DB,
no `goal_id` — goal identity stays the session (daemon-crash recovery out of scope).

## I. Persistence / recovery
Events persist-before-broadcast (unchanged). Client disconnect is handled by the
live daemon runtime (R1), so no dangling-tool reconciliation was needed on
reconnect — and per §4 we deliberately did NOT synthesize `ToolCallFinished` on a
plain reconnect (an in-flight tool is not an orphan). Restarted-daemon recovery
keeps the existing reap-on-restart path.

## J. Replay safety
No auto-replay of destructive tools on reconnect. The reconnect path snapshots +
resubscribes; it never re-issues a tool (R3: started stayed constant across the
kill/reattach; control C confirmed no duplicate).

## K. Tests
- `cargo fmt --all -- --check` ✅
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` ✅ 0/0
- `cargo test --workspace --all-features --locked --no-fail-fast` ✅ **123 suites,
  2676 passed, 0 failed** (no env-flaky this run).
- New unit tests: `socket_intent` auto-approve→ProbeDefault; `should_auto_approve`
  (§6.B per-session isolation + global fallback); continuation ceiling→next-window,
  cross-window-spin→stop, pinned→stop; outcome map ceiling→BudgetLimited.

## L. PTY (new binary)
9/9: startup renders, **R1 `--auto-approve` attaches a daemon (not Embedded)**,
tool auto-approved streaming, resize ×4 no panic, daemon survives client kill,
reattach shows session (not splash), tool finished once no duplicate, status
completed, no panic.

## M. D4 exact replay (R3, clean baseline `bd636a43`)

| | D4 first run (`6983d51`) | D4 replay (R1+R2) |
|---|---|---|
| Work windows | **1** | **4** |
| Stop reason | turn_limit_reached | **completed** |
| Implementation | backend only | **backend + frontend components + i18n + tests + e2e** |
| Frontend | generated `pb.ts` only | `pages/Favorites.tsx`, MemoActionMenu, MemoHeader, AppSidebar nav, router, query hooks (13 files) |
| Tool calls | 366 | 528 |
| Model requests | 100 (=ceiling cap) | 362 |
| Output tokens | 42k | 139k |
| Reconnect | wedged | **advanced headless (239→244), no wedge, resumed** |
| go build | EXIT 0 | **EXIT 0** |
| Blocked by | round ceiling (product) | Go toolchain download (env) |
| False completion | no | no |
| Final status | incomplete | incomplete (env-gated `go test`) |

Windows: (1) backend store+migrations → (2) frontend+i18n → (3) e2e/real-app
(`.e2e-data/memos_prod.db`) → (4) a `repair` window to fix the Go toolchain. The
agent's honest final message: fixed what it could, backend/frontend/e2e favorite
flow verified, but `go test` still blocked on the toolchain download. Independent
`go build ./...` → EXIT 0; `Favorites.tsx` and the full frontend footprint exist;
no new dependencies.

## N. Findings
- **P1-A goal continuation — FIXED & validated** (R3: 4 windows, stop=completed).
- **P1-B execution hosting — FIXED & validated** (R3 reconnect: headless advance,
  no wedge). Original "reconnect wedge" reframed to the auto-approve-embed cause.
- **Env limitation (not a defect):** the sandbox blocks the Go 1.26.2 toolchain
  download (`GOSUMDB`), so the memos `go test` gate can't complete here. Same class
  as the missing Browser capability (§28). Record, do not gate on it.
- No CodeLeveler bug surfaced by the replay.

## O. Regression
chat/plan/goal/session/runtime/remote/TUI/storage/protocol all green in the
123-suite run; PTY covers the interactive + reconnect surface.

## P. Gate decision
**LONG TASK RELIABILITY GATE PASSED.** The two P1s are fixed on the existing
runtime/session/engine (no second architecture, no DB schema, no ceiling raise),
regression + clippy + fmt are green, and the D4 exact replay demonstrates full-stack
multi-window completion with a working reconnect — env-gated only at the final
`go test`. Per §38, STOP after this report: no further D4-replay fixes, no
Browser/Home Layout/Capability work.
