# Beta Final Gate Reconciliation

## 1. Status

```text
BETA_FINAL_GATE=PASS
BETA_READY=YES
OPEN_BETA_BLOCKERS_CURRENT=0
NON_BLOCKING_RESIDUAL_COUNT=13
FOUNDATION_FROZEN=YES
FOUNDATION_REOPENED=NO
```

The Beta gates recorded before the Foundation freeze were checked against the
current tree. Each was inherited, rechecked, or marked superseded or obsolete.
A five-run real-model dogfood batch on independent fixtures then confirmed the
product end to end. No Beta blocker was found, and no product code changed.

This record is the authority for the Beta gate. The historical Beta documents
were removed from `docs/` in `d8fdc9b` and remain readable from `d8fdc9b^`.

## 2. Current Identity

```text
BETA_TESTED_HEAD=b0d22171f98690d1d4a42eb4f5a5aa3ed538823b
BETA_TESTED_PRODUCT_TREE=identical to FOUNDATION_TESTED_HEAD 30839e3 (b0d2217 adds documentation only)
FOUNDATION_FREEZE_RECORD_HEAD=b0d22171f98690d1d4a42eb4f5a5aa3ed538823b
COMMITS_AFTER_FREEZE=0
BETA_FINAL_CI_RUN_ID=34752739798
BETA_FINAL_CI_ATTEMPT=1
BETA_FINAL_CI_RESULT=success
BINARY=leveler 0.2.0-beta.1 (b0d22171f986), debug build of the tested head
MODEL=deepseek/deepseek-v4-flash
```

## 3. Historical Gate Inventory

| # | Gate | Source @ last content commit | Historical status | Classification |
| --- | --- | --- | --- | --- |
| 1 | Beta release readiness, four blockers | `BETA_RELEASE_READINESS.md` @ `e2e8938` (09-08) | BLOCKER-1..4 open | SUPERSEDED |
| 2 | Blocker resolution and remaining risks | `BETA_BLOCKER_RESOLUTION.md` @ `941cf8a` (09-09) | blockers closed; risks 0a (macOS flaky test) and 0b (three Windows tests) open | SUPERSEDED |
| 3 | Post-closure Beta baseline | `BETA_BASELINE_POST_CLOSURE.md` @ `7f82d72` (09-09) | baseline `8761e24f`, Dogfood V1 PASS | SUPERSEDED |
| 4 | Unified Dogfood Acceptance V1 | `evaluations/UNIFIED_DOGFOOD_ACCEPTANCE_V1_RESULT.md` @ `7c6cde2` (09-09) | PASS, 7 cases | SUPERSEDED |
| 5 | Beta Product Closure | `evaluations/BETA_PRODUCT_CLOSURE.md` @ `62d2234` (09-10) | `BETA_READY = YES` at `d5313600`; three P2 defects open | SUPERSEDED |
| 6 | Install and release payload | same, plus CI steps | PASS | VALID |
| 7 | Real Usage Beta round 1 plan | `evaluations/REAL_USAGE_BETA_001.md` @ `e2e8938` | plan | OBSOLETE |
| 8 | `v0.2.0-beta.2` release notes | `releases/v0.2.0-beta.2.md` @ `62d2234` | recommended, not cut | OBSOLETE |
| 9 | Mobile Beta closure, runtime alignment, freeze | `MOBILE_BETA_CLOSURE.md` etc. @ `3164f30` (08-21), tag `mobile-beta-mvp` | closed and frozen | VALID |
| 10 | Verification semantics / F7 grounded authority | `docs/VERIFICATION_SEMANTICS_CLOSURE.md` @ `6724268` (09-10) | closed | VALID |
| 11 | Real PTY TUI acceptance | `docs/evaluations/PTY_REAL_TUI_FINAL_ACCEPTANCE.md` @ `4e7ea29` (09-10) | PASS | NEEDS_RECHECK |
| 12 | TUI execution presentation | `docs/evaluations/TUI_EXECUTION_PRESENTATION_CLOSURE.md` @ `1d40f17` (09-12) | `TUI_EXECUTION_PRESENTATION_CLOSURE=PASS` | VALID |
| 13 | Memory product closure | `docs/MEMORY_FINAL_PRODUCT_CLOSURE.md` @ `f3e63b1` (09-12) | `MEMORY_FINAL_PRODUCT_CLOSURE=PASS` | VALID |
| 14 | Runtime version consistency | `f5a63b2` / `8539d5a` (08-29) | stale daemon replaced | NEEDS_RECHECK |
| 15 | Browser and web_search truth | `docs/FOUNDATION_W2_FINAL_CLOSURE.md` | PASS | VALID |
| 16 | Daemon crash consistency | `docs/FOUNDATION_DAEMON_CRASH_CONSISTENCY_CLOSURE.md` | PASS | VALID |
| 17 | Windows canary evidence | `docs/FOUNDATION_WINDOWS_CANARY_ACCUMULATED_EVIDENCE_REVIEW.md` | PASS | VALID |
| 18 | Engine ownership | `docs/FOUNDATION_W3_A_ENGINE_OWNERSHIP_AUDIT.md` | PASS | VALID |
| 19 | Foundation acceptance | `docs/FOUNDATION_ACCEPTANCE_AND_FREEZE.md` @ `b0d2217` | `FOUNDATION_FROZEN=YES` | VALID |
| 20 | Protocol and generated contracts | schema export, `check:protocol` | PASS | VALID |

```text
TOTAL_GATES=20
VALID=11
NEEDS_RECHECK=2   (both rechecked; PASS)
SUPERSEDED=5
OBSOLETE=2
BLOCKED=0
```

## 4. Supersession Map

| Historical evidence | Superseded by |
| --- | --- |
| Beta release readiness blockers 1–4 | `BETA_BLOCKER_RESOLUTION.md`, then CI run `34752739798` (Windows compiles, lints, runs canaries and the workspace suite) |
| Risk 0a, `duration_budget_stops_the_run_between_rounds` flaky on macOS | Passed on Linux, macOS, and Windows in `34752739798` attempt 1; stays a watched margin |
| Risk 0b, three Windows tests failing | Windows workspace test step green in `34752739798` attempt 1 |
| Post-closure baseline `8761e24f` and Dogfood V1 | This record: current workspace gate and section 7 dogfood batch |
| Beta Product Closure resume smoke and lifecycle evidence | Foundation daemon crash-consistency closure and Foundation acceptance, plus D4 |
| Beta Product Closure TUI gate | TUI execution presentation closure (09-12) and the section 6 recheck |
| Engine lifecycle evidence in any Beta document | W3-A audit and Foundation acceptance |
| Browser truth notes before W2 | W2 final closure |
| Real Usage Beta round 1 plan, `v0.2.0-beta.2` notes | Obsolete as gates. Cutting a release remains the owner's decision. |

No two current documents give different statuses for the same gate.

## 5. Current Gate Matrix

| Gate | Historical evidence | Current classification | Current evidence | Result |
| --- | --- | --- | --- | --- |
| Completion truth | Beta Product Closure honesty section | VALID | truth targets below; D4/D5; W4 self-dogfood | PASS |
| Grounded authority (F7) | Verification semantics closure | VALID | cited tests all present and green | PASS |
| Runtime version consistency | `f5a63b2` | NEEDS_RECHECK → rechecked | 8 unit tests, health E2E, live stale-daemon replacement | PASS |
| Daemon crash consistency | Foundation closure | VALID | Foundation gate; D4 kill -9 resume | PASS |
| Memory | Memory final product closure | VALID | no memory code change since `f3e63b1`; 58 tests | PASS |
| Browser | W2 | VALID | 3 × 12/12 real-browser acceptance; contract targets | PASS |
| web_search | W2 | VALID | `leveler-tools web_search` 8 | PASS |
| Windows process tree | Windows canary review | VALID | both canaries Alive → Gone in `34752739798` | PASS |
| Protocol / generated contracts | baseline | VALID | schema export 3/3; `check:protocol` | PASS |
| Mobile | Mobile Beta closure | VALID | analyze clean; 54 tests; CI Mobile job | PASS |
| Coding harness | Foundation acceptance | VALID | Foundation regression targets; D1–D5 | PASS |
| Foundation | Foundation acceptance | VALID | `FOUNDATION_FROZEN=YES`; no commit since | PASS |
| UI / TUI core flow | PTY acceptance, TUI closure | NEEDS_RECHECK → rechecked | TUI and Web suites; live PTY launch | PASS |

## 6. Targeted Revalidation

Completion truth and grounded authority:

| Target | Passed |
| --- | ---: |
| `leveler-verifier --lib report::` | 23 |
| `leveler-app --lib event_bridge` (incl. `a_run_that_was_not_verified_never_projects_as_passed`) | 36 |
| `leveler-app --lib session::` (incl. `completed_unverified_work_is_not_reported_as_incomplete`) | 21 |
| `leveler-app --test direct_verification` | 7 |
| `leveler-tui --test reducer unverified` | 3 |
| `leveler-eval --lib false_completion` | 2 |

Verification semantics closure tests all exist:
`a_head_move_explains_only_the_paths_it_left_clean` (execution),
`a_head_move_is_not_an_authored_modification`,
`a_fast_forward_pull_authors_nothing`,
`a_write_alongside_a_head_move_is_still_reported`,
`a_write_committed_by_the_same_command_is_still_reported` (tools), and
`a_branch_switch_does_not_inherit_the_projects_test_gate`, which moved from the
Engine to `leveler-agent/tests/direct_test.rs` with the W3 repair.

`VerificationFinished` consumers, checked one by one:

| Consumer | What it reads | Can `passed=true` with `unavailable` show as verified? |
| --- | --- | --- |
| Event bridge → `UiVerification.passed` | `verification_outcome`: `Passed`→`Some(true)`, `Failed`→`Some(false)`, `NotRun`/`Unavailable`→`None` | No |
| TUI verification screen | `UiVerification.passed`; `None` prints no verdict | No |
| Web `completionTruth.ts` | `verified` only when `passed === true` and the turn outcome is completed/answered | No |
| Mobile `session_state.dart` | `verification_updated.passed`; `null` gives no "验证通过" | No |
| CLI text and JSONL render | `verification` first; `passed` labelled as the gate | No |
| Observability `row_verification` | `verification` first; legacy `passed:true` gives no verdict | No |
| `PublicEvent` projection | carries both fields; no rendering consumer in the tree | No |

```text
VERIFICATION_EVENT_CONSUMERS_AUTHORITY_SAFE=YES
```

Runtime version consistency: `runtime_consistency_tests` and
`replacement_verification_tests` (8 passed),
`daemon_e2e::health_reports_identity_and_admission` (1),
`runtime_identity` (4). Live check on the Rust fixture: a daemon started from
the installed `leveler 0.2.0-beta.1 (667ef326373d)`, then the tested-head TUI
opened in a PTY. The old daemon retired, a daemon from the tested-head binary
took the socket, and the TUI reached its idle composer with no error text.

Memory: `leveler-memory` 45 passed; workspace `memory` filter 58 passed. No
memory crate, memory tool, or recall-assembly line changed since `f3e63b1`.

Protocol: `leveler-client-protocol --features schema --test schema_export`
3/3 (`ui_session_snapshot`, `client_command`, `runtime_event` current). Web
`check:protocol` passed. Mobile replays the shared
`testdata/signed_envelope.golden.json`. Since `mobile-beta-mvp`,
`leveler-remote-protocol` changed only in `src/policy.rs` (+40), its tests, and
one `Cargo.toml` line.

## 7. Dogfood Batch

Real model, real tools, real git repositories in the session scratchpad, no
mock. Two independent fixtures: `fx-node` (ES modules, `node --test`) and
`fx-rust` (a three-file crate). Neither depends on `~/.leveler/run`.

| Field | D1 Simple edit | D2 Multi-file | D3 Long goal | D4 Resume | D5 Verification honesty |
| --- | --- | --- | --- | --- | --- |
| RUN_ID | `01e6d084` | `c73ed32d` | `400a80ea` | `0c9e3d19` | `944a504e` |
| TASK | fix `slugify` whitespace bug | add `price_cents`, update callers, add `total_value_cents` | add `wrap`, word-boundary `truncate`, index, tests, README | add `Inventory` module; kill -9 at 3 s; `run --resume` | add `countWords` beside a known-red pending test |
| REPOSITORY | fx-node | fx-rust | fx-node | fx-rust | fx-node |
| DURATION | 12 s | 14 s | 46 s | 17 s total | 20 s |
| MODEL_REQUESTS | 4 | 4 | 7 | 5 | 6 |
| TOOLS_USED | read_file×3, apply_patch, run_command, update_goal | read_file×3, apply_patch×2, run_command, update_goal | read_file×5, update_plan×4, apply_patch×2, write_file, run_command, update_goal | read_file×3 (killed), write_file, apply_patch, run_command, update_goal | read_file×5, write_file×2, apply_patch, run_command, update_goal |
| FILES_CHANGED | 1 | 2 | 6 | 2 | 3 |
| VERIFICATION_REQUESTED | npm test | cargo fmt/check/test | npm test | cargo fmt/check/test | npm test |
| VERIFICATION_RESULT | test=passed → passed | all passed → passed | test=passed → passed | fmt=failed (non-gating), check/test passed → passed | test=failed → failed |
| OUTCOME | completed | completed | completed | completed | completed |
| STOP_REASON | Completed, exit 0 | Completed, exit 0 | Completed, exit 0 | Completed, exit 0 | Completed with failed gate, exit 1 |
| RESUME_USED | no | no | no | yes | no |
| LONG_GOAL_USED | goal opened and settled | goal opened and settled | goal opened, plan used 4×, settled | same goal record settled after resume | goal opened and settled |
| REVIEW_USED | no (independent review off) | no | no | no | no |
| DELEGATION_USED | no | no | no | no | no |
| FALSE_VERIFIED | no | no | no | no | no |
| PRODUCT_ERROR | none | none | none | none | none |

Independent checks outside CodeLeveler: fx-node `npm test` 3/3 after D1 and
10/10 after D3 (`wrap("the quick brown fox", 10)` → `["the quick","brown fox"]`;
`truncate("hello big world", 12, {wordBoundary:true})` → `hello big...`);
fx-rust `cargo test` 3/3 after D2 and 7/7 after D4.

D4 durable facts: turn 1 `interrupted` with stop reason "unclean process
exit", turn 2 `completed`. One goal record `settled`. The initiating message
exists once; the second `user` row is the harness's goal-unresolved nudge. The
three read calls from the killed turn stayed in the transcript. No dangling
tool call.

D5 durable facts: `task_finished` has `outcome=completed`,
`verification=failed`; `verification_finished` has `passed:false`. The model
left `test/pending.test.js` untouched. The known-red test was not attributed to
the baseline, so the gate stood as failed.

W4 self-repo dogfood `122ad8ca` on the same product tree (`30839e3`) is the
`CompletedUnverified` sample: `outcome=completed`, `verification=unavailable`,
CLI exit 1.

```text
RUNS_TOTAL=5
RUNS_SUCCESSFUL=5 (task delivered; D5 honestly reported a failed gate)
FALSE_VERIFIED_TOTAL=0
INCORRECT_AND_VERIFIED=0
LIFECYCLE_CORRUPTION=0
DUPLICATE_RECOVERY=0
LOST_ACCEPTED_INPUT=0
CRASH_WITHOUT_DURABLE_EXPLANATION=0
COMPLETION_TRUTH_PRODUCT_GATE=PASS
DOGFOOD_ACCEPTANCE=PASS
```

## 8. Completion Truth

```text
COMPLETION_TRUTH=PASS
GROUNDED_AUTHORITY=PASS
FALSE_VERIFIED_PATH_FOUND=NO
UNAVAILABLE_IS_NOT_VERIFIED=YES
COMPLETED_UNVERIFIED_HONEST=YES
ENGINE_SEMANTIC_AUTHORITY=NO
```

The live batch covered the verdicts `passed` (D1–D4), `failed` (D5), and
`unavailable` (W4 self-repo). In each case the session row, `task_finished`,
and CLI exit code agreed. A model's `update_goal complete` never produced a
verdict; the verification plan did. The Engine persists the Harness-supplied
verdict (Foundation acceptance, section 4).

## 9. Cross-platform CI

Run `34752739798`, push to `main`, head `b0d22171…`, attempt 1, completed,
success:

| Job / step | Result |
| --- | --- |
| Linux Rust (fmt, clippy, workspace tests, installer checksum, release payload, remote control, Chrome acceptance 12/12) | success |
| macOS Rust (incl. Chrome acceptance 12/12) | success |
| Windows Rust (incl. security canaries 70 passed, Edge acceptance 12/12) | success |
| Web | success |
| Mobile | success |
| deny · audit | success |

Windows canaries: `windows_job_cancellation_kills_grandchildren` and
`windows_job_timeout_kills_grandchildren`, pid file 3.133 s, Alive witness
3.589 s, then Gone. An unobservable result panics.

Local gate on the tested head: `git diff --check`, `cargo fmt --check`, and
`cargo clippy -D warnings` pass. `cargo test --workspace --all-features
--locked --no-fail-fast` gives 3812 passed, 0 failed, 20 ignored (opt-in
live/manual/visual only). Web `npm ci`, `check:protocol`, `typecheck`,
`npm test` (19 files, 180 tests), and `build` pass. Mobile `flutter analyze`
reports no issues; `flutter test` passes 54.

## 10. Non-blocking Residuals

| # | Residual | Class | Beta blocker | Evidence |
| --- | --- | --- | --- | --- |
| 1 | Self-repo tests fail inside the verify sandbox (`~/.leveler/run/locks`) | PRODUCT_USABILITY_DEBT | NO | Foundation record section 6; verdict stays `unavailable` |
| 2 | `VerificationFinished.passed` is the gate, `true` for unavailable | PRODUCT_SEMANTICS_DEBT | NO | section 6 consumer table |
| 3 | Goal checkpoint create port not ownership-fenced | FOUNDATION_NON_BLOCKING | NO | no path to a false completion |
| 4 | Parallel parent hard crash before first child stays Running | NON_BLOCKING | NO | never shown Completed |
| 5 | Historical Windows canary setup intermittency | NON_BLOCKING | NO | fail-closed; current sample green |
| 6 | `persist_runtime_config` unfenced config write | NON_BLOCKING | NO | configuration columns only |
| 7 | `EngineEvent` carries Coding vocabulary | NON_BLOCKING | NO | compile-time coupling |
| 8 | `configs/models/*.yaml` still ship `default_effort: max` | DEFERRED_PRODUCT_WORK | NO | Beta Product Closure P2, still present |
| 9 | Resumed session gives no hint that files are already modified | DEFERRED_PRODUCT_WORK | NO | Beta Product Closure P2; no fix commit since |
| 10 | A wrong API key does not name its source variable | DEFERRED_PRODUCT_WORK | NO | Beta Product Closure P2; no fix commit since |
| 11 | Baseline attribution does not recognise a pre-existing `node --test` failure | DEFERRED_PRODUCT_WORK | NO | D5 reported `failed` (conservative, not green) |
| 12 | A failing non-gating `cargo fmt` still ends "verification passed" | PRODUCT_SEMANTICS_DEBT | NO | D4; `cargo fmt` is `gating=false` by plan; check and test passed |
| 13 | Stale documentation references | DOC_DEBT | NO | `CHANGELOG.md` links deleted `docs/BETA_BASELINE_POST_CLOSURE.md`; `VERIFICATION_SEMANTICS_CLOSURE.md` quotes `conclude_direct` in `leveler-engine/src/engine.rs`, now in Coding |

## 11. Explicit Non-goals

Not part of this gate, and not started:

- Multi-Agent durable child sessions, `SubAgentProvider`, background-agent UX,
  capability negotiation.
- Push notifications and Multi-Agent Mobile UI.
- Cutting `v0.2.0-beta.2`, including the workspace version bump and the
  Windows artifact policy. That is the owner's release decision.
- Any Foundation, Engine, Memory, Browser, or verification-system redesign.

## 12. Final Gate

```text
COMPLETION_TRUTH=PASS
GROUNDED_AUTHORITY=PASS
RUNTIME_VERSION_CONSISTENCY=PASS
DAEMON_CRASH_CONSISTENCY=PASS
MEMORY=PASS
BROWSER=PASS
WEB_SEARCH=PASS
WINDOWS_PROCESS_TREE=PASS
PROTOCOL_CONTRACT=PASS
MOBILE_BETA=PASS
CODING_HARNESS=PASS
FOUNDATION=PASS
UI_PRODUCT_SURFACE=PASS
DOGFOOD_ACCEPTANCE=PASS
FALSE_VERIFIED_TOTAL=0
EXACT_MAIN_CI=PASS
FIRST_ATTEMPT_STABLE=YES
OPEN_BETA_BLOCKERS_CURRENT=0
BETA_FINAL_GATE=PASS
BETA_READY=YES
```

## 13. Next Phase

```text
READY_FOR_POST_BETA_PRODUCT_WORK=YES
MULTI_AGENT_STARTED=NO
```

The next phase is chosen separately. Residuals 1, 2, 11, and 12 all concern
how verification results are produced or labelled. They are candidates for a
Verify Sandbox Runtime Isolation task if that is taken before Multi-Agent
Product Closure.
