# Gate 4 · Reviewer mechanism — closeout

**Result: N7 → VERIFIED_FIXED (proof level B, regression-tested).**

Gate 4's policy half already shipped at `4714486`: `ReviewTrigger` classified a change, and a task
that policy said needed review closed as `CompletedUnverified` when none was on record. The half
that was missing is the one that makes the designation actionable — **the harness had no way to
launch a reviewer at all**. A child could only be created by a model calling `spawn_agent` inside
a `drive.rs` round, and R008 and R009 both simply declined to delegate. The runtime was scoring a
requirement it could not satisfy.

## What changed

| Layer | Change |
| --- | --- |
| `leveler-agent/src/sub_agent.rs` | `AgentRole::Reviewer`. `AgentRole::parse` deliberately does **not** accept `"reviewer"` — the role is unreachable from a model tool-call, so "a review happened" cannot be self-awarded. |
| `leveler-agent/src/executor.rs` | Reviewer resolves like Explorer: read-only registry (`read_only_subset`), explorer loop policy, plus its own prompt — judge the change, do not extend it, name file + defect, and say so plainly when nothing is blocking. |
| `leveler-agent/src/executor/handlers.rs` | `Executor::run_reviewer_child` — the second entrance to the **same** primitive (`run_one_sub_agent_on`). Returns `DelegatedChildResult { ok, text, modified_files }`. |
| `leveler-engine/src/turn.rs` | `TurnRunner::run_review` — builds the host executor from the existing `ExecutorFactory`, applies the same `OwnershipFence`, persists the `SubAgentStarted`/`SubAgentFinished` pair through the same `EventLog`. |
| `leveler-engine/src/engine.rs` | `conclude_direct`'s N7 branch now **launches** the review instead of only recording its absence, and downgrades only when the review could not be obtained. `review_brief` builds the brief (task + changed files, capped at 40 paths). |

No new crate, no second scheduler, no second `ToolHost`, no child state machine, no change to the
`spawn_agent` schema or its dispatch. The primitive itself is untouched.

## Tests (all in `crates/leveler-engine/tests/direct_test.rs`)

| Test | Asserts |
| --- | --- |
| `security_shaped_change_gets_a_harness_launched_review` | A change to `src/auth.rs` produces exactly one `SubAgentStarted{role:"reviewer"}`, a matching `SubAgentFinished{ok:true}`, and the task stays `Verified`. The model never calls `spawn_agent`. |
| `ordinary_change_launches_no_reviewer` | A narrow edit to `src/lib.rs` launches no child at all — review is not a tax on every task. |
| `harness_reviewer_cannot_modify_the_code_it_reviews` | The reviewer's own `apply_patch` attempt leaves the file byte-identical. |

## Reverse validation (all three ran RED with the fix disabled)

| Disabled | Observed failure |
| --- | --- |
| The `run_review` call in `conclude_direct` | `reviewers.len()` = 0 — mechanism absent. |
| Same, with the mechanism assertions removed | outcome = `CompletedUnverified`, not `Verified` — the launch is what earns the verdict, not a weakened audit. |
| `AgentRole::Reviewer` → full registry | The reviewer actually rewrote `src/auth.rs` to `pub fn login() { todo!() }` — the read-only guard is load-bearing, not decorative. |

Regression: `leveler-engine` + `leveler-agent`, 455 tests, 0 failed. `cargo clippy --all-targets`
clean, `cargo fmt --check` clean.

## Scope, stated honestly

- **Direct tasks only.** The launch sits in `conclude_direct`, at the one point a task would
  otherwise close as `Verified`. Goal/orchestrate paths reach their verdict elsewhere and are
  **not** covered by this gate.
- **The review is evidence that a review happened, not a gate on its content.** A reviewer that
  completes keeps the task `Verified` even if its report names defects; the findings are persisted
  in `SubAgentFinished.summary` and surfaced to the UI, but nothing parses them. Acting on
  findings needs the structured child result — that is Gate 5's `ChildResult`, and until it lands
  this remains a mechanism, not a verdict.
- **Proof level B.** Unit + regression only. No daemon smoke and no real-usage run has yet
  observed a harness-launched reviewer against a live model.
