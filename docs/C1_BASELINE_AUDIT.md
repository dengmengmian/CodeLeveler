# C1_BASELINE_AUDIT

> Status: baseline audit for C1 (Real Task Completion)
> Baseline: `main @ 1468b46` · Model: deepseek/deepseek-v4-flash · Date: 2026-08-07
> No production code was changed for this audit.

## 1. Current Task Execution Flow

```text
User Task (TUI SubmitMessage / RunGoal / eval)
-> TaskEngine.run/chat/resume
   -> acquire_ownership (P2C token) -> mark_running (fenced)
   -> TurnRunner.run_turn: reap -> start_owned -> TurnStarted
      -> Executor (seeded Plan/Ledger/Progress from event log)
      -> drive loop per round:
         model stream (same-request retry, backoff+jitter)
         -> ToolHost admit (barrier -> hooks -> permission -> approval
            -> barrier -> ownership fence) -> dispatch
         -> results in call order -> fenced transcript append
         -> loop guard / plan gate / budgets / observe-thrash checks
      -> update_goal(complete|blocked) or stall/budget stop
   -> supervise (goal continuation <= 32 turns, budget extension <= 2)
   -> conclude_direct:
      non-success stop -> terminal (Incomplete-with-work still verifies)
      no mutation or no gates -> CompletedUnverified (K19)
      -> verify (gate-time plan rediscovery, blast-radius scoping)
      -> baseline reconcile (base_commit attribution)
      -> failed && repairable && attempts < 1 -> ONE repair turn -> re-verify
      -> finalize_task_outcome -> TaskOutcome
   -> finish_task_owned (event + outcome + status + state, one transaction)
```

## 2. Completion Decision

Only the Engine writes terminal facts. `Verified` requires gates that exist,
ran green, plus a real mutation (K19). The model cannot self-verify:

1. `update_goal(complete)` is gated by `check()` (open todos / missing
   delivery evidence -> typed refusal, `GoalIntercepted` on the log; no
   attempt-count bypass, only explicit `override_incomplete_todos`).
2. Even a resolved goal only reaches `Verified` through the Verifier's own
   command runs.
3. `only_verified_is_automation_success` pins the outcome semantics.

## 3. Repair Loop (actual behavior)

`verification_is_repairable` (scope violations and non-retryable environment
failures are not repaired) -> at most ONE repair turn
(`DIRECT_REPAIR_ATTEMPTS = 1`) -> repair goal = original goal + each failed
gate's name and evidence -> re-verify. The repair turn starts with
`prior = Vec::new()` — it sees the failure evidence but NOT the original
turn's context.

## 4. Failure Taxonomy (current handling / gap)

| Class | Handling | Gap |
| --- | --- | --- |
| misunderstanding | TaskContract + Understand phase | no restate/confirm loop |
| localization | grep/find_symbol/LSP, locate_hint | no repo map (C2) |
| context | auto-compaction w/ snapshot watermark | long tasks untested |
| tool failure | model-visible error text; patch errors show actual file content + line numbers | no escalation on repeated same error |
| edit failure | apply_patch atomic; write allowlist; file budget | — |
| build/test failure | evidence + classification + 1 repair turn | repair lacks context; single round |
| verification | blast-radius scoping; baseline attribution (tests) | compile failures have no baseline offset |
| no-progress loop | call fingerprint guard -> observe-thrash second chance -> hard stop cap | measured 0% |
| budget exhaustion | typed; <= 2 targeted extensions | — |
| model failure | same-request retry; malformed tool-JSON repair | — |
| environment | non-retryable class skips repair; ToolMissing explicit | — |
| false completion | goal gate + K19 + Verifier; measured 0/15 | weak-verification repos untested |
| sub-agent failure | ok:false + partial progress rollup to parent | not enforced at the completion gate |

## 5. Existing Eval Coverage

smoke x3, core x21 (single-file go/rust/ts with hidden acceptance tests),
hard x5, regression x6 (incl. recovery-compile-fail), scenarios (manual).
Metrics: completion, false completion, steps, tool calls, loop rate,
validation rate, TTFF, recovery rate.

Gaps: multi-file changes, repository exploration (fixed tiny file sets),
long tasks / context exhaustion, multi-round repair chains, repos without
verification, real-repo noise (pre-existing red, dirty worktree).

## 6. Real Model Baseline (measured)

12 cases (hard x5 + recovery-compile-fail + core sample x6), plus the 3-case
gate sample: **15/15 passed, 100% completion accuracy, 0 false completion,
0 user intervention, loop rate 0%, validation rate 100%, recovery rate 100%,
avg 5.8 steps / 7.5 tool calls.** Heaviest case: ts-concurrency-limit
(9 steps, ~139k input tokens).

## 7. Top Failure Modes (by real-task impact)

1. **Eval saturation** — the current suite no longer separates anything;
   improvement work has no signal source.
2. **Repair context break** — empty `prior`, single round.
3. **Compile failures cannot be baseline-attributed** (missing observation
   event; test failures can).
4. **Localization without structure** on large repos (C2 scope, noted).
5. **Sub-agent failure not enforced at the completion gate.**
6. **Weak-verification repos**: CompletedUnverified is honest but offers no
   guidance loop.

## 8. C1 Scope Proposal

DO (small, measurable):
1. Eval expansion FIRST with the existing framework: multi-file cases,
   exploration-required cases, repair-chain cases, no-verification case —
   restore metric resolution before changing behavior.
2. Repair with context: inject the original turn's compacted context
   (existing RawTranscript/assemble machinery); allow a second repair round
   only when round one improved a gate.
3. Baseline attribution for build failures.
4. Unresolved child-agent failures surface at the `update_goal` gate.
5. Token-cost audit for heavy cases (repeated context injection).

DO NOT: repo index/map (C2), toolset expansion (C3), long-task framework
(C4), multi-agent topology, TUI/Web UX, any Runtime/Storage/Ownership work
(gate closed).
