# Foundation Acceptance & Freeze

## 1. Status

```text
FOUNDATION_ARCHITECTURE=PASS
FOUNDATION_ACCEPTANCE=PASS
FOUNDATION_FROZEN=YES
FREEZE_CONDITION=first-attempt main CI success for the commit adding this record
```

Every local acceptance gate below passed on the tested product head
(`f1ae044`). A record cannot name its own commit or that commit's CI run, so
the final run is reported against this record rather than written into it. If
that run fails on its first attempt, this freeze is void and must be withdrawn
by a follow-up commit.

This is a **re-freeze**: the earlier record at `30839e3` is superseded by the
snapshot here, and its content is retained below as history. The W2, daemon,
Windows canary, and W3-A records remain the authority for their own gates.

## 2. Accepted Identity

```text
FOUNDATION_TESTED_HEAD=f1ae0441dd8caa0795071f4be0702d5d53108f6e
FOUNDATION_TESTED_TREE=edf1ee662700d29cd093e072ec7d5184909e12dc
FOUNDATION_TESTED_BRANCH=main
PREVIOUS_FREEZE_HEAD=30839e3e32849dba838f5cef8856c56e2b060150
SUPERSEDED_BY=f1ae0441dd8caa0795071f4be0702d5d53108f6e
FREEZE_RECORD_COMMIT=the commit that adds this section (documentation only)
```

`FOUNDATION_TESTED_HEAD`/`FOUNDATION_TESTED_TREE` name the product tree every
gate below ran on. The commit that adds this record changes documentation only,
so it is deliberately not written into `FOUNDATION_TESTED_HEAD`.

## 3. Re-freeze Acceptance (f1ae044)

Baseline stability was proven: the workspace was clean and `HEAD`/tree were
byte-identical before and after the acceptance run.

```text
TEST_START_HEAD=f1ae0441dd8caa0795071f4be0702d5d53108f6e
TEST_END_HEAD=f1ae0441dd8caa0795071f4be0702d5d53108f6e
TEST_START_TREE=edf1ee662700d29cd093e072ec7d5184909e12dc
TEST_END_TREE=edf1ee662700d29cd093e072ec7d5184909e12dc
BASELINE_STABLE=YES
WORKTREE_CLEAN=YES
```

Static and web gates:

```text
cargo fmt --all -- --check=PASS
cargo clippy --workspace --all-targets --all-features -- -D warnings=PASS
cargo test --workspace=PASS (PASSED=4004, FAILED=0, IGNORED=20)
web typecheck (gen-protocol --check + tsc --noEmit)=PASS
web vitest=PASS (198 passed, 19 files)
```

Architecture proof (Engine owns lifecycle, not agent intelligence):

```text
ENGINE_AGENT_DEPENDENCY=NONE
ENGINE_TOOLS_DEPENDENCY=NONE
ENGINE_VERIFIER_DEPENDENCY=NONE
ENGINE_APP_DEPENDENCY=NONE
leveler-engine --test minimal_harness=PASS (6 passed, 0 failed)
leveler-engine --test ownership_direction=PASS (9 passed, 0 failed)
ENGINE_LIFECYCLE_OWNERSHIP=PASS
ENGINE_AGENT_INTELLIGENCE_SEPARATION=PASS
```

Ownership / crash acceptance:

```text
leveler-app --test execution_ownership=PASS (8)
leveler-engine --test boot_authority=PASS (13)
leveler-app --test reaper_authority=PASS (8)
leveler-agent --test crash_recovery_test=PASS (18)
leveler-storage --test fencing_contract=PASS (2)
leveler-storage --lib ownership_store=PASS (3)
leveler-cli --test daemon_e2e=PASS (7)
leveler-app --test goal_identity=PASS (6)
leveler-app --test goal_settlement=PASS (3)
leveler-app --test lifecycle_authority=PASS (7)
```

Browser contract regression (live cross-OS browser CI evidence is inherited
from the W2 record; this re-freeze re-ran the local contract suites):

```text
leveler-tools --test browser_surface=PASS (4)
leveler-tools --test capability_composition=PASS (7)
leveler-browser (acceptance + selection)=PASS (12)
leveler-tools --lib web_search=PASS (6)
```

Real dogfood (real runtime, real provider, no mocks):

```text
DOGFOOD_TASK=fix add() in calc.py to return a+b, then run python3 test_calc.py
DOGFOOD_MODEL=deepseek/deepseek-v4-flash
DOGFOOD_SESSION=75d3b163-369b-4122-95ef-e7904d4c9f50
DOGFOOD_BINARY=leveler 0.2.0-beta.2 (f1ae0441dd8c)
DOGFOOD_TOOLS=read_file, read_file, apply_patch, run_command, update_goal (5 started / 5 finished)
DOGFOOD_EDIT_OCCURRED=YES (calc.py: `return a - b` -> `return a + b`)
DOGFOOD_TEST=python3 test_calc.py exit 0 ("all tests passed")
DOGFOOD_STOP_REASON=CompletedUnverified
DOGFOOD_VERIFICATION=not_run (reason: no_automatic_verification)
```

Truth review (from the persisted session database and `leveler trace`):

```text
TASK_RESULT_CORRECT=YES
COMPLETION_TRUTH=PASS
TERMINAL_PERSISTED=YES
OWNERSHIP_RELEASED=YES (task owner_runtime_id/owner_boot_id NULL, owner_epoch=1)
SESSION_STATE_CORRECT=YES (status=completed, state=complete, outcome=completed, verification=not_run)
PERSISTENCE_READBACK=PASS
RESUME_TRUTH=PASS
FALSE_VERIFIED=0
INCORRECT_AND_VERIFIED=0
```

The runtime reported `CompletedUnverified` because its automatic verification
plan did not run; the model's own `run_command` checking the fixture is not the
runtime's verification. The terminal records exactly that, so no passing
verification is claimed that did not happen.

## 4. Authority Model

```text
Model owns reasoning.
Harness owns domain semantics.
Engine owns lifecycle.
```

A read-only re-audit of the re-frozen head confirmed the W3-A conclusions still
hold:

```text
ONE_SESSION_TASK_LIFECYCLE_OWNER=YES
ENGINE_SELECTS_CODING_WORKFLOW_STATE=NO
ENGINE_INTERPRETS_CODING_SEEDS=NO
ENGINE_INTERPRETS_CODING_CHECKPOINTS=NO
ENGINE_INFERS_DOMAIN_COMPLETION=NO
PRODUCT_OWNS_PARALLEL_PARENT_LIFECYCLE=NO
LONG_GOAL_FENCING=PASS
TASK_GOAL_TERMINAL_ATOMICITY=PASS
ENGINE_RECOVERY_WORDING_DOMAIN_NEUTRAL=YES
NEW_ARCHITECTURE_FRAMEWORK_INTRODUCED=NO
SECOND_LIFECYCLE_SYSTEM=NO
SECOND_DURABLE_TRUTH_SOURCE=NO
ENGINE_TO_CODING_DEPENDENCY=NONE
```

`EngineEvent` and `leveler-lifecycle` still carry Coding/product vocabulary
(`PlanUpdated`, `EvidenceLedgerUpdated`, `GoalCheckpointCreated`, `AgentState`).
The Engine only persists, replays, and data-classifies those variants. This is
compile-time coupling, not semantic authority, and it is not a freeze blocker.

---

# Superseded freeze record (30839e3) — retained as history

The sections below are the original freeze record for product head
`30839e3e32849dba838f5cef8856c56e2b060150`. They remain valid evidence for
their own gates; the authoritative snapshot for the current frozen baseline is
sections 1–4 above.

## Original record: Closed Gates

```text
W2=PASS
DAEMON_CRASH_CONSISTENCY=PASS
WINDOWS_CANARY_EVIDENCE_REVIEW=PASS
W3_ENGINE_OWNERSHIP=PASS
MINIMAL_NON_CODING_HARNESS=PASS
CODING_HARNESS_REGRESSION=PASS
FOUNDATION_DOGFOOD=PASS
```

Source records:

- [`FOUNDATION_W2_FINAL_CLOSURE.md`](FOUNDATION_W2_FINAL_CLOSURE.md)
- [`FOUNDATION_DAEMON_CRASH_CONSISTENCY_CLOSURE.md`](FOUNDATION_DAEMON_CRASH_CONSISTENCY_CLOSURE.md)
- [`FOUNDATION_WINDOWS_CANARY_ACCUMULATED_EVIDENCE_REVIEW.md`](FOUNDATION_WINDOWS_CANARY_ACCUMULATED_EVIDENCE_REVIEW.md)
- [`FOUNDATION_W3_A_ENGINE_OWNERSHIP_AUDIT.md`](FOUNDATION_W3_A_ENGINE_OWNERSHIP_AUDIT.md)

## 4. Authority Model

```text
Model owns reasoning.
Harness owns domain semantics.
Engine owns lifecycle.
```

A read-only re-audit of the tested head confirmed the W3-A repair holds:

```text
ONE_SESSION_TASK_LIFECYCLE_OWNER=YES
ENGINE_SELECTS_CODING_WORKFLOW_STATE=NO
ENGINE_INTERPRETS_CODING_SEEDS=NO
ENGINE_INTERPRETS_CODING_CHECKPOINTS=NO
ENGINE_INFERS_DOMAIN_COMPLETION=NO
PRODUCT_OWNS_PARALLEL_PARENT_LIFECYCLE=NO
LONG_GOAL_FENCING=PASS
TASK_GOAL_TERMINAL_ATOMICITY=PASS
ENGINE_RECOVERY_WORDING_DOMAIN_NEUTRAL=YES
NEW_ARCHITECTURE_FRAMEWORK_INTRODUCED=NO
SECOND_LIFECYCLE_SYSTEM=NO
SECOND_DURABLE_TRUTH_SOURCE=NO
ENGINE_TO_CODING_DEPENDENCY=NONE
```

Evidence:

- Session/task creation: every production creation path (normal and daemon in
  `leveler-app/src/session.rs`, fork in `interactive.rs`, parallel parent in
  `parallel.rs`, the Coding runtime in `leveler-agent/src/coding/run.rs`, eval
  in `leveler-cli/src/eval_cmd.rs`) calls `TaskEngine::create_task`, which
  writes the session and task rows in one `TaskCreationStore` transaction.
  Remaining direct `SessionRepository::create` calls are inside test modules.
- Running transition: `TaskEngine::mark_running` and `start_task` take the
  `AgentState` from the caller. The Engine source contains no
  `AgentState::Execute` outside event tests.
- Long goals: `TaskEngine::open_goal` requires an ownership token, and
  `TaskTerminal.goal` is committed by `finish_task_owned` with the task terminal
  and `TaskFinished` row.
- Parallel parent: `parallel.rs` uses `create_task`, `mark_running`, a fenced
  `EventLog::new_owned`, and `finish_task`; every controlled post-start error
  reaches that one terminal call.
- Seeds and checkpoints: `leveler-engine/src` has no `PlanState`,
  `EvidenceLedger`, `ProgressLedger`, or seed assembly. Checkpoint projection,
  resume splicing, and reaped-session checkpoint triggers live in
  `leveler-agent/src/coding/checkpoint.rs` and `leveler-app`; the Engine keeps
  `commit_goal_checkpoint`, the transcript watermark, and `ReapedSession`.
- Recovery wording: `EngineError::RecoveryConfirmationRequired` and
  `acknowledge_crash_window` say only that a side effect may have happened and
  was not replayed. The workspace instruction stays in the CLI.
- Abstractions: the W3 repair added narrow value types and ports
  (`TaskExecution`, `TaskTerminal`, `GoalTerminalUpdate`, `ReapedSession`,
  `TurnStart`, `TaskCreationStore`, `CompactionCheckpoint`) and no manager,
  registry, locator, bus, runtime, or store. Pre-existing registries
  (`ToolRegistry`, `ProviderRegistry`, `OwnershipRegistry`,
  `BackgroundTaskRegistry`) are unrelated to lifecycle.
- Dependency: `cargo tree -p leveler-engine -e normal` contains no
  `leveler-agent`, `leveler-tools`, `leveler-verifier`, `leveler-app`, or
  product crate; `tests/ownership_direction.rs` enforces the direction.

`EngineEvent` and `leveler-lifecycle` still carry Coding/product vocabulary
(`PlanUpdated`, `EvidenceLedgerUpdated`, `GoalCheckpointCreated`, `AgentState`).
The Engine only persists, replays, and data-classifies those variants. This is
compile-time coupling, not semantic authority, and it is not a freeze blocker.

## 5. Second Harness Proof

`crates/leveler-engine/tests/minimal_harness.rs` is the non-Coding harness. It
imports nothing from `leveler-agent`, `leveler-tools`, or `leveler-verifier`
(checked by the test itself) and carries a private `MinimalResult` payload the
Engine never reads.

```text
TEST_COUNT=5
RESULT=5 passed, 0 failed
```

| Capability | Proven by |
| --- | --- |
| Session and task creation | `create_task` in every test |
| Turn execution and ordered lifecycle events | `the_engine_runs_a_turn_for_a_harness_that_is_not_the_coding_agent` |
| Session and turn rows survive a reconnect | same test, database reopened |
| Payload opacity | same test: no persisted row contains the harness result |
| Coding seed events stay opaque | `a_non_coding_turn_does_not_parse_coding_seed_events` |
| Restart reaping to `interrupted`, exactly-once initiating input, resume, terminal commit | `an_interrupted_turn_is_visible_after_restart_and_the_next_turn_runs` |
| Lost-child settlement without child semantics | `the_engine_settles_a_ghost_child_for_a_harness_with_no_child_semantics` |
| No Coding imports | `the_second_harness_does_not_reach_for_the_coding_harness` |

The harness still names `AgentState` values from `leveler-lifecycle` when it
calls `mark_running` and `finish_task`, and emits generic `EngineEvent`
variants. It needs no plan, evidence, progress, workspace, checkpoint,
reviewer, delegation, diff, or repository type.

## 6. Coding Harness Acceptance

Mechanical targets on the tested head, all 0 failed:

| Area | Target (passed) |
| --- | --- |
| Normal turn, terminal truth, ownership | `leveler-agent --test direct_test` (43), `phase0_baseline_test` (3), `side_effect_barrier_test` (4) |
| Long goal | `leveler-app --test goal_identity` (6), `goal_settlement` (3) |
| Resume and crash recovery | `leveler-agent --test crash_recovery_test` (17), `leveler-app --test zombie_turns` (1), `session_axes_resume` (5) |
| Checkpoint | `leveler-agent --test goal_checkpoint_test` (6) |
| Compaction | `leveler-agent --test multi_turn_session_test` (11) |
| Verification | `leveler-app --test direct_verification` (7) |
| Review | `direct_test` review cases, `leveler-agent --lib coding::run::` (20) |
| Parallel and multi-agent | `leveler-app --test parallel_ownership` (3), `leveler-agent --test multi_agent_test` (71), `ma_restart_truth_test` (8) |
| Lifecycle tripwires | `leveler-app --test lifecycle_authority` (7), `leveler-engine --test ownership_direction` (9), `owned_event_log_boundary` (1), `leveler-storage --test fencing_contract` (2), `leveler-app --test daemon_lifecycle` (6) |

Real dogfood, run with the tested-head debug binary
(`leveler 0.2.0-beta.1 (30839e3e3284)`) in a detached throwaway worktree:

```text
DOGFOOD_TASK=correct the stale doc comment on TaskEngine::task_for_session, then run cargo test -p leveler-engine --test minimal_harness
DOGFOOD_REAL_MODEL=deepseek/deepseek-v4-flash (6 requests)
DOGFOOD_SESSION=122ad8ca-b300-4e2e-aebc-4ab037aef4c7
DOGFOOD_TOOLS_USED=grep, read_file, apply_patch, run_command, update_goal
DOGFOOD_EDIT_OCCURRED=YES (one doc comment in crates/leveler-engine/src/engine.rs)
DOGFOOD_VERIFICATION=model ran minimal_harness 5/5; harness checks fmt=passed, check=passed, test=failed, verdict=unavailable
DOGFOOD_COMPLETION=outcome=completed, verification=unavailable, StopReason::CompletedUnverified, CLI exit 1
DOGFOOD_RESULT=PASS
```

Durable facts in the session database: one `task_started`, one user turn
`completed` with the initiating message, five tool call start/finish pairs,
`verification_check`×3, one `task_finished` with `outcome=completed` and
`verification=unavailable`, and a session row with the same values.

The harness-run `cargo test --workspace` failed only under verification write
confinement. Reproduced on `leveler-tools --test edit_contract` with the
product's own `process_request_for_verify_check`: two tests fail with
`lock ~/.leveler/run/locks/…: Operation not permitted`, while the same target
passes 8/8 unconfined in the same worktree. The change was a comment. The
Coding Harness did not report a passing verification it did not have, and the
CLI returns failure for `CompletedUnverified` by design
(`leveler-cli/src/run_cmds.rs`). The same pattern was seen in the 2026-09-11
dogfood before the W3 repair.

## 7. Crash / Recovery Acceptance

```text
DETERMINISTIC_CRASH_BARRIER=PASS
EXISTING_SIGKILL_E2E=PASS
RECOVERY_EXACTLY_ONCE=PASS
```

`cargo test -p leveler-cli --test daemon_e2e --all-features -- sigkill` ran
three tests, all passed:

- `sigkill_after_durable_ack_before_transcript_append_recovers_once`
  (deterministic barrier, `test-crash-barrier` feature);
- `sigkill_during_a_task_recovers_on_restart_without_duplication`;
- `connected_client_recovers_after_daemon_sigkill`.

The minimal harness independently proves the reaper projects an accepted
initiating input exactly once. Base CI ran both SIGKILL tests on Linux and
macOS.

## 8. Cross-platform Safety

Base CI run `34750049988`, attempt 1, all six jobs succeeded: Linux, macOS,
and Windows Rust; Web; Mobile; deny/audit.

Windows process-tree canaries, job `103704840946`, step "Windows security
canaries":

| Canary | Pid file | Alive witness | Result |
| --- | ---: | ---: | --- |
| `windows_job_cancellation_kills_grandchildren` | 3.427 s | 3.952 s | ok |
| `windows_job_timeout_kills_grandchildren` | 3.440 s | 3.952 s | ok |

Both canaries established an Alive witness and then verified the grandchild
Gone; an unobservable result panics, so no fail-closed branch was taken. They
also passed in the workspace test step.

```text
ADDITIONAL_SUCCESS_SAMPLE=YES
HISTORICAL_ROOT_CAUSE=UNDETERMINED
DETERMINISM_PROVEN=NO
```

Browser acceptance in the same run: Windows Edge 12/12, Linux Chrome 12/12,
macOS Chrome 12/12. Local contract regression: `browser_surface` (4),
`capability_composition` (7), `leveler-browser --test selection` (4),
`leveler-tools web_search` (8). No live Tavily request was made.

## 9. Workspace Gate

```text
git diff --check=PASS
cargo fmt --all -- --check=PASS
cargo clippy --workspace --all-targets --all-features -- -D warnings=PASS
cargo test --workspace --all-features --locked --no-fail-fast=PASS
PASSED=3812
FAILED=0
IGNORED=20
```

The 20 ignored tests are opt-in by declaration: 11 manual TUI product
scenarios, 2 manual visual dumps, 1 splash preview, 1 frame-evidence dump,
1 live MCP server test, 1 live rust-analyzer timing test, 2 replay tests
needing an external recording, and 1 full-repository verifier probe. None covers a
Foundation invariant.

## 10. Residuals

| Residual | Classification | Basis |
| --- | --- | --- |
| Goal checkpoint create port is not ownership-fenced | NON_BLOCKING | Checkpoints are derived projections (W3-A), and resume ignores a checkpoint whose cursor is beyond the durable log (`coding/checkpoint.rs`). A stale write can change model-visible resume context but cannot change task, turn, goal, or completion rows, which stay fenced. |
| Parallel parent hard crash between Running and first child | NON_BLOCKING | Controlled errors always commit a terminal fact. A hard crash leaves the parent durable `Running`, never `Completed`; the restart reaper only reaps running turns, and the interactive snapshot shows a Running session with no running turn as Interrupted. No second owner exists. Reaping turnless tasks is a separate recovery-contract change. |
| Historical Windows canary setup intermittency | NON_BLOCKING | Passed again on the base run; root cause undetermined; failures stay fail-closed. |
| Interactive runtime config write bypasses the Engine | NON_BLOCKING | `interactive.rs` `persist_runtime_config` writes model, mode, sandbox, and execution kind (always `direct`) through `SessionRepository` without a token. It touches configuration columns, not status, workflow state, outcome, or terminal facts. |
| `EngineEvent` and `leveler-lifecycle` carry Coding vocabulary | NON_BLOCKING | Compile-time coupling only; see section 4. |
| This repository's tests cannot pass harness verification under write confinement | DEFERRED_PRODUCT_WORK | Tests write the real `~/.leveler/run/locks`, which the workspace-only verify sandbox denies (section 6). Self-dogfood therefore ends `CompletedUnverified`. A test-isolation or verify-scope change, not a lifecycle defect. |
| `VerificationFinished.passed` is `true` for an unavailable verdict | DEFERRED_PRODUCT_WORK | Seen again in the dogfood event log (`passed: true, verification: unavailable`). The canonical `task_finished` row and session column both say `unavailable`; the boolean is a pre-existing two-way gate field. |

```text
BLOCKING_RESIDUAL_COUNT=0
```

## 11. Freeze Rule

Foundation is frozen.

Future changes to Engine or lifecycle foundations require a concrete product,
correctness, safety, or performance need.

"No cleaner architecture" by itself is not sufficient justification.
