# F7-C — Behavioral Witness Contract: Closure Report

```
Current phase:
    Beta Capability Closure

Current step:
    F7-C Behavioral Witness Contract + Engine Verification Evidence Integration

Completed:
    Engine Verification → EvidenceLedger integration re-audited (shipped cc840d7,
    unchanged); the semantic-substitution defect reproduced as a named test on
    the real completion path; the terminal enforcement point, which had no
    end-to-end coverage, now has one; the architecture gate answered and stopped.

Next:
    New Treatment Dogfood Acceptance | BLOCKED — architecture review first

Beta impact:
    OPEN_BETA_BLOCKER=1
    OPEN_BETA_REQUIRED=1
    PHASE_B=HOLD
```

Read `docs/F7C_BEHAVIORAL_WITNESS_ARCHITECTURE.md` first: it carries the
decision. This report carries what was built, what was measured, and what was
deliberately not done.

---

## 1. Root cause, and how the four pieces relate

| | what it is | state |
| --- | --- | --- |
| F7-A | a behavioural obligation cited the `apply_patch` that made the edit; a file changing is not the behaviour changing | CLOSED |
| F7-B GAP 1 | `refs=[]` — an uncited behavioural claim discharged on the judge's word | CLOSED by F7-C's lifecycle fix |
| F7-B GAP 2 | evidence that is real, fresh, successful, runtime-issued and correctly kinded can exercise a **neighbouring** behaviour | **OPEN** |
| engine-evidence gap | the runtime's own verification reached the EventLog and never the EvidenceLedger, so the one decision that mattered was taken blind | CLOSED (`cc840d7`) |

The engine-evidence gap was the *lifecycle* defect: F7-B's GAP 1 rule was
correct and sat at the wrong moment, so it demanded proof that did not exist
yet. Moving the evidence, not the rule, closed GAP 1.

GAP 2 is a different kind of thing. It is not a rule in the wrong place; it is a
question the runtime's evidence vocabulary cannot ask.

## 2. The most important question, answered

> Requirement = A. Evidence/Witness = B. B is real and successful but
> semantically different. The judge says B proves A. Why does CodeLeveler
> refuse Verified?

**It does not refuse.** That is the honest answer, and it is now a test rather
than a sentence:

`crates/leveler-engine/tests/direct_test.rs` —
`gap2_an_observation_of_something_else_still_discharges_a_behavioural_claim`

- Obligation: *"the summary must omit rows whose amount is zero"* (`behavior`).
- Judge: satisfied, `refs=[]`, `strength: observed` — the normal shape; 26 of
  the 32 behavioural obligations in the preserved cohort cite no id.
- Runtime evidence: the engine's own gate, a real command, runtime-issued,
  correctly kinded, exit 0, run after the work landed. It checks that a file
  exists. It exercises nothing the obligation is about.
- Result today: **`TaskOutcome::Verified`.**

The authority path, exactly:

```
conclude_direct                                    engine.rs:1948
  verify() → record_verification_evidence          engine.rs:2327
    ledger.record_verify("engine-verification:0:unrelated", fp, 0)
    ledger.runtime_evidence_complete = true
  last_persisted_ledger → completion_debt()        engine.rs:2110
    CompletionContract::open_obligations
      discharged(kind=Behavior, …)                 completion_contract.rs:457
        cited.is_empty()                → true
        runtime_evidence_complete       → true     (commit point)
        ledger.observed_the_changed_tree()
          any VerifyRecord{exit_code==0, after_mutation_seq>0}  → TRUE
      ⇒ discharged ⇒ no debt ⇒ Verified stands
```

`observed_the_changed_tree()` asks whether the run observed *anything*. It has
no way to ask whether it observed *this*. The test asserts today's wrong
behaviour on purpose, so that closing GAP 2 breaks it loudly and it becomes the
green one.

## 3. Witness derivation safety — stated without overclaiming

The task's §19 hard question was whether a pre-derived witness merely moves the
same A→B substitution from reconciliation to derivation. It does, for the
obligations that need it. The reasoning is in the architecture doc §5–§7; the
short form:

- Where the **user** supplied the discriminating material — a named command, a
  coverage demand, a named path — the contract **already** carries it as
  `CommandSuccess` / `TestCoverage` / `MutationScope`, extracted under an
  explicit fail-closed rule ("a guessed scope is worse than none"). A witness
  type here is a second spelling of something that exists.
- Where the user did not, the witness's discriminating content must be
  **invented**, and no auditable constraint distinguishes an invention that
  describes A from one that describes B. Red-before/green-after narrows this to
  derivation-time misreading — a real gain, and still a residual whose only
  guarantee is "the model read A correctly", which §34 forbids as the sole
  basis.

Safe containment is therefore limited to requirements that already carry
explicit discriminating structure — which is the set the product already
handles. That is stated plainly rather than dressed up as coverage.

## 4. Engine verification — lifecycle, re-audited not re-changed

Shipped in `cc840d7`; this task changed none of it and confirmed all of it.

| | before | after |
| --- | --- | --- |
| order | claim → contract → verify → contract (blind) | claim → contract (request) → verify → **record** → contract (commit) |
| evidence type | none reached the ledger | `VerifyRecord`, same shape and fingerprint as the agent's own |
| identity | — | `engine-verification:<attempt>:<check>` |
| persistence | EventLog only | `EvidenceLedgerUpdated`, before the terminal read |
| dedupe | — | same id in same attempt is a no-op; unnamed check is skipped, never given an invented fingerprint |
| freshness | — | `after_mutation_seq` at record time |
| failure | — | `exit_code = 1`; a failed gate is not an observation |

Why it cannot itself declare completion: `finalize_task_outcome` produces a
verification verdict which the contract can only **demote**. There is no
`if verification_passed { complete() }`. This is now proven end-to-end, not
asserted — see §5.

## 5. What was built this round

Two tests. No product source changed.

### 5.1 The terminal enforcement point had no end-to-end coverage

`completion_debt()` has exactly one production caller on the Direct path
(`engine.rs:2133`). The test that looked like its coverage —
`a_behavioural_obligation_with_nothing_observed_does_not_verify` — never
reaches it: it runs with no verification plan, so `conclude_direct` returns at
its K19 exit (`modified_files.is_empty() || !has_gates()`) long before the
contract is consulted. It passes for a different reason than the one its name
suggests.

Finding the shape that *does* reach it is itself the result. The contract is
asked twice on one ledger; a demotion at the second question requires the
ledger to have **changed between them**, and every scripted alternative exits
earlier:

| door | stop reason | reaches the contract? |
| --- | --- | --- |
| round ceiling / bounded continuation (measured) | `BudgetExhausted` | no — `direct_non_success_outcome` returns first |
| gate fails | verdict `Failed` | no — outcome is `Failed` before the contract |
| **claim accepted, then the runtime's own repair turn mutates** | `Completed` | **yes** |

Only `Completed`/`Answered`, or `Incomplete` **with** mutations, get past
`conclude_direct`'s early return, and `Incomplete` is produced only by the
thrash/no-progress guards — not scriptable deterministically. An accepted claim
is therefore the practical door, and an accepted claim means the request-point
gate was satisfied, so the demotion has to come from evidence recorded after it.

That last one is the case the boundary was built for, and it is now tested:

`a_repair_turn_that_breaks_the_declared_scope_cannot_verify`

- The user's task confines the work to `src/feature.rs`
  (`MutationScope { allowed_paths: ["src/feature.rs"] }`).
- The agent obeys and claims completion. The request-point gate **rightly
  accepts**: at that moment nothing is outside the scope.
- The engine's gate fails; its own repair turn writes `src/other.rs`; the
  re-run goes green.
- `report.verification.verdict() == Verified` — asserted, so the contract is
  demonstrably what changes the answer.
- `completion_debt()` is `Some`, the outcome is not `Verified`, and the refusal
  is staged as a durable `completion_contract_open` review-stage row — asserted,
  so the demotion is attributed to this wire and not to another guard.

### 5.2 GAP 2, characterized on the real path

`gap2_an_observation_of_something_else_still_discharges_a_behavioural_claim`,
described in §2.

### 5.3 A broken test in the working tree, resolved

The tree carried an uncommitted, **failing** test
(`a_green_run_cannot_verify_a_contract_that_still_owes_a_demonstration`,
109 lines). Its intent was right — cover the terminal enforcement point — and
its construction could not work: the obligation it used is open at *both*
questions, so the request-point gate refuses the claim, the loop asks for
another turn, and the scripted responses run out (`Err(… "no more responses")`).
It is replaced by §5.1, which is the same intent on a shape that reaches the
code it names. Its `ScriptedContractRuntime` helper is kept and generalized;
both new tests use it.

## 6. Representative cohort

```
REPRESENTATIVE_BEHAVIORAL_TOTAL                 = 32
REPRESENTATIVE_PRE_F7C_DISCHARGEABLE            = 32
REPRESENTATIVE_POST_F7C_VERIFIABLE              = 32   (no predicate changed)
REPRESENTATIVE_NEW_CU_COUNT                     = 0
REPRESENTATIVE_NEW_FALSE_BLOCK_COUNT            = 0

ZERO_REF_BEFORE_ENGINE_EVIDENCE                 = 26 / 32
ZERO_REF_AFTER_ENGINE_EVIDENCE                  = NOT MEASURABLE IN THIS TASK
UNGROUNDED_MATERIAL_BEHAVIOR_AFTER_ALL_EVIDENCE = NOT MEASURABLE IN THIS TASK
```

The last two are not omissions. The cohort is preserved *run records* produced
before `cc840d7`; their ledgers carry no engine-verification records and no
`runtime_evidence_complete`, so they replay as "still gathering" and are judged
exactly as they were. Answering §48/§80 needs fresh runs on the current HEAD,
which is dogfood and is excluded from this task by §114/§133.

Projected effect of the candidate rules on that cohort — the §79 number the
review needs:

| rule | blocked | effect |
| --- | ---: | --- |
| today | 0 | 32/32, GAP 2 open |
| uncited material behaviour never verifies | 26 | 2 of 3 successful runs become CU |
| red-before/green-after witness required | 32 | every run becomes CU |

## 7. Cost

No product source changed, so: no new model call, no new prompt text, no new
evidence payload, no change to any bound. The context/token-cost work of the
previous batch is untouched, as are `REASONING_PASSBACK_PROMOTION=REJECTED` and
the 64 KiB trimming threshold.

## 8. Test matrix

| case | test | result |
| --- | --- | --- |
| F7-A wrong-kind evidence | `a_behaviour_citing_only_a_mutation_is_not_discharged`, `calling_a_mutation_an_observation_does_not_make_it_one` | PASS |
| related-but-different real command (RED B) | `gap2_an_observation_of_something_else_still_discharges_a_behavioural_claim` | **reproduces the defect** |
| zero-ref, nothing observed — predicate | `an_uncited_claim_over_unobserved_work_is_refused_once_evidence_is_complete` | PASS |
| zero-ref, nothing observed — engine path | `a_behavioural_obligation_with_nothing_observed_does_not_verify` | PASS, **but for the K19 reason, not the contract's** (§5.1) |
| engine evidence reaches the ledger | `engine_verification_reaches_the_evidence_ledger` | PASS |
| engine evidence grounds a claim | `the_runtime_s_own_verification_lets_a_behavioural_claim_close` | PASS |
| engine gate fails | `a_failed_engine_gate_is_recorded_as_a_failed_observation` | PASS |
| stale check | `a_check_from_before_the_last_edit_is_not_a_witness`, `a_check_that_predates_the_work_proves_nothing_about_it` | PASS |
| unknown ref | `an_invented_citation_is_not_a_witness`, `an_arbitrary_ref_is_not_behavioural_evidence_at_commit` | PASS |
| terminal truth, end to end | `a_repair_turn_that_breaks_the_declared_scope_cannot_verify` | **new, PASS** |
| wrong pre-derived witness | — | **not built; the gate stopped before implementation** |
| valid discriminating witness | — | **not built; same reason** |
| unwitnessable → fail closed | — | **not built; same reason** |

## 9. Regressions

| gate | command | result |
| --- | --- | --- |
| engine | `cargo test -p leveler-engine --test direct_test` | 53 passed, 0 failed |
| fmt | `cargo fmt --all -- --check` | exit 0 |
| clippy | `CARGO_TARGET_DIR=target/clippy cargo clippy --workspace --all-targets` | clean |
| workspace | `cargo test --workspace --no-fail-fast` | 135 suites, **3689 passed, 1 failed** |

**The one failure was environmental, and is recorded rather than waved away.**
`leveler-browser::reliability::websocket_egress_is_gated` — a real headless-
browser integration test that navigates a local page and polls its title for 8
attempts — failed while the machine was saturated by the workspace build. It
was re-run twice afterwards: alone (`1 passed`) and with its whole suite
(`11 passed, 0 failed`), so it is neither a code defect nor order-dependent
within its suite. It shares no code path with anything this task touched: the
only source change is `crates/leveler-engine/tests/direct_test.rs`.

```
FULL_WORKSPACE_GREEN=NO   (1 environmental failure; green on isolated re-run)
```

Reported as `NO` because `cargo` exited 101. Calling it `YES` on the strength
of a re-run would be the same accounting this whole line of work exists to
stop.

F1 / F2A / F3 / F5 / F6 / F7-A / Terminal Truth / Context Cost are covered by
the workspace run; no product source changed, so nothing in them could regress
by construction.

## 10. Architecture audit

```
SECOND_RUNTIME=NO                      SECOND_SCHEDULER=NO
SECOND_EVENTLOG=NO                     SECOND_EVIDENCE_LEDGER=NO
SECOND_COMPLETION_CONTRACT=NO          SECOND_COMPLETION_PREDICATE=NO
SECOND_JUDGE=NO                        SECOND_BEHAVIOR_LEDGER=NO
SECOND_VERIFICATION_STATE_MACHINE=NO

JUDGE_PROSE_AUTHORITY=YES              ← unchanged and UNCLOSED: this is GAP 2
REQUIREMENT_PROSE_HEURISTIC_ADDED=NO
BENCHMARK_SPECIFIC_SPECIAL_CASE=NO
PRIMARY_F7C_FIX_PROMPT_ONLY=NO         ← there is no fix this round
F1_SAFETY_BOUNDARY_WEAKENED=NO
NEW_MODEL_CALL_ADDED=NO
ENGINE_VERIFICATION_IS_SECOND_COMPLETION_PREDICATE=NO
ENGINE_VERIFICATION_DIRECT_SUCCESS_PATH=NO
```

`JUDGE_PROSE_AUTHORITY` is reported `YES` deliberately. An uncited behavioural
claim still rests on the judge's reading, floored only by "the run observed
something". Reporting it `NO` because no *new* prose authority was added would
be the kind of accounting F7 exists to stop.

## 11. Machine-readable final state

```
F7C_ARCHITECTURE_GATE=REVIEW_REQUIRED

F7C_SEMANTIC_SUBSTITUTION_RED_REPRODUCED=YES
F7C_ZERO_REF_RED_REPRODUCED=YES        (closed by cc840d7 at the predicate;
                                        the engine-path test that names it does
                                        not reach the contract — see §5.1)
F7C_ENGINE_EVIDENCE_GAP_RED_REPRODUCED=YES  (closed by cc840d7)

F7C_ENGINE_VERIFICATION_EVIDENCE_GREEN=PASS
F7C_ENGINE_EVIDENCE_REAL_PATH=PASS

F7C_SEMANTIC_SUBSTITUTION_RED_TO_GREEN=NOT_ATTEMPTED
F7C_ZERO_REF_UNGROUNDED_RED_TO_GREEN=PASS
F7C_VALID_WITNESS_TRUE_POSITIVE=NOT_ATTEMPTED
F7C_WRONG_PREDERIVED_WITNESS_BLOCKED=NOT_ATTEMPTED
F7C_UNWITNESSABLE_FAIL_CLOSED=NOT_ATTEMPTED

F7C_END_TO_END_COMPLETION_PATH=PASS
F7C_TERMINAL_TRUTH_GATE=PASS
F7C_DURABILITY_GATE=NOT_APPLICABLE     (no durable shape changed)

F1_REGRESSION_GATE=PASS
F2A_REGRESSION_GATE=PASS
F3_REGRESSION_GATE=PASS
F5_REGRESSION_GATE=PASS
F6_REGRESSION_GATE=PASS
F7A_REGRESSION_GATE=PASS
ENGINE_VERIFICATION_AUTHORITY_GATE=PASS
TERMINAL_TRUTH_REGRESSION_GATE=PASS
CONTEXT_COST_REGRESSION_GATE=PASS

FMT_CLEAN=YES
CLIPPY_CLEAN=YES
FULL_WORKSPACE_GREEN=NO
  135 suites, 3689 passed, 1 failed, cargo exit 101
  the failure is leveler-browser::websocket_egress_is_gated, environmental;
  green alone (1 passed) and with its suite (11 passed) on re-run — see §9

REPRESENTATIVE_BEHAVIORAL_TOTAL=32
REPRESENTATIVE_PRE_F7C_DISCHARGEABLE=32
REPRESENTATIVE_POST_F7C_VERIFIABLE=32
REPRESENTATIVE_NEW_CU_COUNT=0
REPRESENTATIVE_NEW_FALSE_BLOCK_COUNT=0
ZERO_REF_BEFORE_ENGINE_EVIDENCE=26
ZERO_REF_AFTER_ENGINE_EVIDENCE=NOT_MEASURABLE_IN_THIS_TASK
UNGROUNDED_MATERIAL_BEHAVIOR_AFTER_ALL_EVIDENCE=NOT_MEASURABLE_IN_THIS_TASK

SECOND_RUNTIME=NO
SECOND_SCHEDULER=NO
SECOND_EVENTLOG=NO
SECOND_EVIDENCE_LEDGER=NO
SECOND_COMPLETION_CONTRACT=NO
SECOND_COMPLETION_PREDICATE=NO
SECOND_JUDGE=NO
SECOND_BEHAVIOR_LEDGER=NO
SECOND_VERIFICATION_STATE_MACHINE=NO
JUDGE_PROSE_AUTHORITY=YES
REQUIREMENT_PROSE_HEURISTIC_ADDED=NO
BENCHMARK_SPECIFIC_SPECIAL_CASE=NO
PRIMARY_F7C_FIX_PROMPT_ONLY=NO
F1_SAFETY_BOUNDARY_WEAKENED=NO
NEW_MODEL_CALL_ADDED=NO
ENGINE_VERIFICATION_IS_SECOND_COMPLETION_PREDICATE=NO

F7C_ENGINEERING_GATE=REVIEW_REQUIRED
F7_STATUS=OPEN
NEXT=ARCHITECTURE_REVIEW

F7C_START_HEAD=d243b2e2c662bc9a6f7ed4eef77d167f45b4e744
F7C_LAST_CODE_HEAD=d775c3d
F7C_LAST_CODE_TREE=f21899bef3aaa86109ed7cdc44ac8dc20331c8f3
COMMITS_CREATED=2
  d775c3d  test(engine)  the two contract facts that were asserted, not tested
  HEAD     docs(f7c)     the architecture gate and this report

The docs commit is the one carrying this file, so its own hash cannot appear
inside it. `git log -2` names both. `F7C_FINAL_HEAD` for the handoff is that
docs commit; the last hash that changes anything a build sees is d775c3d.
PUSHED=NO
FINAL_WORKTREE_CLEAN=YES

Note on §109: it scopes the narrow commit to F7C_ENGINEERING_GATE=PASS, and
the gate is REVIEW_REQUIRED. Committed anyway, on the author's instruction:
this round produced no product change, and what it did produce — the GAP 2
reproduction and the terminal-boundary coverage — is the material the review
reads. Nothing here clears a blocker.

OPEN_BETA_BLOCKER=1
OPEN_BETA_REQUIRED=1
PHASE_B=HOLD
```

`F7C_ENGINEERING_GATE` cannot be `PASS`: §106 requires proving all of A, B, C
and D, and A (related-but-different real command evidence cannot false-discharge)
is reproduced as still failing, while C and D were not attempted because the
architecture gate stopped before implementation.

## 12. Remaining risks

1. **GAP 2 is open**, and now demonstrated rather than described. A correct-
   looking observation of the wrong behaviour still discharges.
2. **The floor is low.** "Something was observed over the changed tree" is a
   real requirement and a weak one; `/usr/bin/true` satisfies it.
3. **The yield number that decides the fix does not exist.** Until fresh runs on
   the current HEAD produce it, any choice between "open gap" and "26 of 32
   blocked" is a guess.
4. **The request point remains ungated** by design; the commit point is what
   refuses.
