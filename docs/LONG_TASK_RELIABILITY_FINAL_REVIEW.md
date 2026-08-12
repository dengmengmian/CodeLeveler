# Long Task Reliability Final Code Review

Read-only review of the gate change set. No product code modified. Not a merge.
Method: 3 independent adversarial reviewers over the diff, each finding verified
against the actual code by the synthesizer.

## A. Baseline
- MAIN_BASELINE (merge-base, where the gate branched): `6983d51`
- GATE_HEAD: `127c601` (`fix/long-task-reliability-gate`)
- Note: `main` has since advanced to `04bbd8e` via two UNRELATED prompt-greeting
  commits (`744d3e2`, `04bbd8e`) not part of this gate. The gate's own change set
  is `6983d51..127c601`.

## B. Diff summary
17 files, +620/-25. Product code: `leveler-client-protocol` (ApprovalPolicy enum),
`leveler-local-transport` (CreateSessionRequest field), `leveler-app`
(SessionRuntimeConfig + approver + should_auto_approve + create_session),
`leveler-cli` (socket_intent + cmd_tui), `leveler-web` (create_session reset),
`leveler-engine` (continuation after_turn + supervise counter + outcome remap).
Plus tests and docs. No migrations, no new runtime, no new channel, no goal_id.

## C. R1 — Approval-policy review
R1.1 `--auto-approve`→daemon (ProbeDefault): **HOLDS** (`run_cmds.rs:174-190`, tested).
R1.2 genuinely per-session (`session_runtime` map keyed by SessionId): **HOLDS**.
R1.3 A(Interactive)/B(AutoApprove) isolated: **HOLDS** — with the documented global
fallback: `serve --auto-approve` sets `self.auto_approve` and `should_auto_approve(_,
true)` is always true, so a global daemon flag auto-approves everything (pre-existing).
R1.4 **remote cannot elevate: VIOLATED → BLOCKER** (see Findings B-1).
R1.5 policy consistent across `--session` reconnect: **HOLDS** (reconnect only
snapshots; can't set a policy).
R1.6 restore doesn't let defaults clobber a live config; approval_policy is **NOT
persisted**; restored default = **Interactive (safe)** (`interactive.rs:318-326,
373-375, 394-417`). **HOLDS**.
R1.7 `--in-process --auto-approve` still embeds: **HOLDS** (tested).
R1.8 `config_overridden` still embeds: **HOLDS** (tested).

## D. Runtime-ownership review (§11)
No regression — the diff does not touch ActiveTurns, the disconnect handler, reap,
or runtime identity. Client disconnect does not cancel/finish/reap the turn or
duplicate a worker (`accepted_work_continues_after_the_ui_disconnects`,
local-transport `lib.rs:2225`; reap guarded by runtime identity, `interactive.rs:753`).
Execution ownership = daemon runtime, unchanged. **HOLDS**.

## E. Disconnect / reconnect review (§12)
Reconnect uses `snapshot` only; it does NOT synthesize a failed ToolCallFinished for a
Started-without-Finished tool, and does NOT reap a live daemon-owned in-flight tool as
an orphan (reap is runtime-identity-guarded, restart/exit-scoped). The gate adds
nothing here. The forbidden "synthesize failure on reconnect" pattern is absent.
**HOLDS**.

## F. Replay-safety review (§13)
At-most-once preserved: `CreateSession` is explicitly non-replayable
(`safe_to_retry_after_transport_failure` false for Send/CreateSession/Subscribe,
local-transport `lib.rs:206-216`); `Deliver` dedups by CommandEnvelope command_id.
Adding `approval_policy` changes no idempotency key. No duplicate RunGoal / SubmitMessage
/ tool from reconnect. **HOLDS**.

## G. R2 — Multi-window review
R2.1 goal/UntilTerminal only: **HOLDS** — chat runs `conclude_direct` directly and never
enters `supervise()` (`engine.rs:769-787`); no PLAN execution kind exists; eval pinned
budget early-returns Stop before the new branch (`continuation.rs:71-73`, tested).
R2.2 100-round ceiling not raised: **HOLDS** (`drive.rs:206` unchanged, not in diff).
R2.3 no infinite loop / R2.4 32-window hard bound: **HOLDS** (`for _ in
0..MAX_SUPERVISED_TURNS`, `MAX_NO_PROGRESS_WINDOWS=2` stops far earlier, saturating_add).
R2.5 cancellation at within-window / transition / next-window-start: **HOLDS**.

## H. No-progress review (§15)
Signal = growth of the cumulative deduped `modified_files` set (`engine.rs:1192,
1248-1253`). ESCAPE (read/grep/test-forever): **bounded** to ~2 extra windows, not 32.
FALSE-KILL: **soft-VIOLATED, MINOR** (see Findings m-1) — a goal iterating on an
already-modified file set (or deleting files) while repeatedly hitting the ceiling is
counted as no-progress and stopped after ~2 windows (~300-round grace). Completing
goals are unaffected (they stop on Completed, not TurnLimitReached).

## I. Outcome / completion review (§16, §17)
Mid-window ceiling is NEVER written as a session terminal failure — `finish_task` is
the single lifecycle writer, called once after the whole `supervise` loop
(`engine.rs:392, 453/466/479`). Final `TurnLimitReached`→`BudgetLimited`→
`terminal_status_for`→`SessionStatus::Incomplete` (resumable, not "failed"). The last
window's `Completed` still flows through the identical verify + bounded-repair path
(`conclude_direct`, `:1404-1420`); `supervise()` cannot short-circuit verification.
**HOLDS**. NOTE: mid-goal windows still emit a turn-level "reached the 100-round
ceiling" message (cosmetic; not a lifecycle failure).

## J. Context review (§18)
`drive_goal_again` re-loads full raw then compacts via `assemble(... chat_default
initial_budget)`, so the model context is bounded each window (`engine.rs:1285-1292`).
Continuation prompt is a fixed template (no growth). NOTE: one User restatement
accumulates per window (bounded by compaction + window caps); verification facts are
not threaded between windows (verification is terminal by design). **HOLDS**.

## K. Security review (§21)
Serde default = Interactive (safe for old/omitting clients). Not persisted; restore =
Interactive (safe). **BUT**: AutoApprove is honored from more than the trusted-local
Unix transport — see B-1. The stated invariant ("Only honoured from the trusted local
transport, never elevated by a remote client") is **not** enforced at the daemon
boundary.

## L. Protocol compatibility
Additive: one enum + one `#[serde(default)]` field. Old client omitting it → Interactive.
Wire-compatible. **HOLDS**.

## M. Architecture conformance (§19)
All 10 "must-not" claims **HOLD**: no second/goal runtime, no special execution engine,
no parallel event channel, no TUI-owned continuation/ownership, no UI strings in engine,
no duplicate protocol state (one additive field), **no goal/window DB table** (zero
migrations; `windows_without_progress` is in-memory), no goal_id. Changes confined to
the declared seams (after_turn / supervise / approver / socket_intent / web boundary).

## N. Test review (§22)
COVERED: socket_intent auto-approve→ProbeDefault; should_auto_approve per-session +
global fallback; in-process wins; config-override embeds; pinned-budget no-continuation;
ceiling→next-window (policy); cross-window-spin→stop (policy); final-exhaustion→
BudgetLimited; the four reconnect invariants (by PRE-EXISTING daemon tests using
Interactive).
GAPS:
- **remote-cannot-elevate: NO TEST** (and the behavior is actually VIOLATED — B-1).
- **multi-window counter WIRING untested end-to-end**: no test drives `supervise()`
  across ≥2 real windows; the progress→reset / stagnation→increment code
  (`engine.rs:1244-1253`) — the only runaway bound — is never exercised (M-2).
- **resume-preserves-policy: NO TEST** (restore hardcodes Interactive, fail-closed; a
  future persist+restore regression could silently auto-approve — M-3).
- R1 headline "auto-approve goal survives client disconnect" has no automated e2e test;
  proven only by unit(socket_intent)+PTY/manual controls (NOTE).

## O. Full regression
`cargo fmt --all -- --check`: **PASS**. On the SAME commit (127c601), the Phase-5 run was
`cargo clippy --workspace --all-targets --all-features -- -D warnings` **0/0** and
`cargo test --workspace --all-features --locked --no-fail-fast` **123 suites / 2676
passed / 0 failed**. A confirming re-run was launched for this review (in progress at
report time; nothing in the tree changed since Phase 5). No test regressions expected
or observed.

## P. Findings

### BLOCKER
- **B-1 — Remote privilege elevation.** `approval_policy` is honored from remote
  session-creation entry points that do NOT force-reset it: the remote-agent bridge
  (`crates/leveler-remote-agent/src/bridge.rs:226-229` deserializes `CreateSessionRequest`
  straight from a paired device's signed body and calls `create_session`) and the raw
  wire handler (`crates/leveler-local-transport/src/lib.rs:397-401`). The `§6.G` reset
  exists only in the web handler (`server.rs:259-260`). A Control-scope paired remote
  device can create an `AutoApprove` session over the untrusted relay/tunnel, skipping
  the approval overlay for shell/file mutations — a remote privilege escalation that
  contradicts the invariant printed on the type. FIX DIRECTION (do NOT apply in this
  review): force `approval_policy = Interactive` at the daemon's trust boundary for any
  non-trusted-local-Unix origin (or have `create_session` accept AutoApprove only from
  the local transport), so the guarantee holds at one enforced choke point instead of
  one of three client handlers.

### MAJOR
- **M-1 — Raw TCP/Unix wire `CreateSession` also doesn't reset** (`lib.rs:397-401`).
  Same root cause as B-1, lower exposure (bearer-token + loopback). Closed by the same
  boundary fix.
- **M-2 — Multi-window counter wiring untested.** `engine.rs:1244-1253` is the only
  bound on a runaway/stuck goal, yet no test drives `supervise()` across ≥2 real windows
  (policy tests feed the counter a literal). Add an engine integration test with a stub
  runner returning `TurnLimitReached` repeatedly; assert stop at `MAX_NO_PROGRESS_WINDOWS`
  and never exceeding `MAX_SUPERVISED_TURNS`.
- **M-3 — Security/lifecycle invariants unpinned:** `remote cannot elevate` and
  `resume preserves policy` have no tests. (The former is now known-broken — B-1 — so its
  fix must ship with a test.)

### MINOR
- **m-1 — No-progress uses a coarse "new distinct file path" proxy** (§15 FALSE-KILL).
  A genuinely-progressing goal that re-edits an already-touched file set while hitting the
  ceiling is stopped after ~2 extra windows. Author should accept consciously or move to a
  content/edit-activity signal. ~300-round grace; completing goals unaffected.

### NOTE
- Eval pinned-budget guard keys on `ContinuationPolicy::bounded`, not
  `step_limits.max_rounds`; an eval pinning rounds only via `max_rounds` + `UntilTerminal`
  would open windows (latent — no such eval today).
- Global `serve --auto-approve` defeats the web reset (`should_auto_approve(Interactive,
  true)==true`) — pre-existing; document as a known limitation of "remote cannot elevate".
- Silent `AutoApprove→Interactive` downgrade across a daemon restart (safe direction;
  consistent with "daemon-crash continuation out of scope").
- Mid-goal windows emit a "reached the 100-round ceiling" turn message before continuing
  (cosmetic; no lifecycle failure written).
- Docs accurate, no overclaim; zh-CN in parity; empirical D4-replay numbers correctly
  framed as a manual run.

## Q. Merge recommendation

**CHANGES REQUIRED.**

The gate's core is sound — architecture-conformant (no second runtime / DB table /
goal_id / ceiling raise), R2 multi-window is correct and hard-bounded, ownership /
reconnect / replay-safety / completion-gate all hold, docs don't overclaim, and the D4
replay validated the intended behavior end-to-end. But **B-1 is a real remote privilege
escalation**: the "remote cannot elevate to AutoApprove" invariant is enforced in one of
three session-creation entry points. That must be fixed (reset at the daemon trust
boundary) and covered by a test before merge; M-2/M-3 test gaps should ship with it.
Once B-1 is closed at the boundary with a test, and M-2/M-3 tests are added, this is
mergeable — no architectural rework is required.

Per the review mandate: findings are reported only. No code changed, no merge, no push.
