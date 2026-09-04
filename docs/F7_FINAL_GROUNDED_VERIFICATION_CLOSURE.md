# F7 — Final Grounded Verification Authority Floor: Closure Report

```
Current phase:
    Beta Capability Closure

Current step:
    F7 Final Grounded Verification Authority Floor

Completed:
    The authority floor shipped: a materially behavioural obligation with no
    proof standard the runtime can settle from its own record no longer
    authorizes Verified. GAP 2's reproduction inverted from red to green on the
    real completion path. `observed_the_changed_tree()` deleted — the channel,
    not just its use. Engine Verification → EvidenceLedger preserved intact.

Next:
    New Treatment Dogfood Acceptance | READY

Beta impact:
    OPEN_BETA_BLOCKER=1
    OPEN_BETA_REQUIRED=1
    PHASE_B=HOLD
```

---

## 1. The architecture decision, stated

```
GENERIC_BEHAVIORAL_WITNESS_AS_AUTHORITATIVE_PROOF = REJECTED
UNANCHORED_SEMANTIC_SATISFACTION_FOR_VERIFIED     = NOT_ALLOWED
```

The runtime does not try to decide whether behaviour A and observation B are
the same thing. It cannot: its evidence vocabulary is a command fingerprint, an
exit code, a mutation watermark and a tool-call id, and none of those says what
a command was asking of the code. Every attempt to close that gap — a witness
derived before execution, a second judge, prose comparison — either moves the
same semantic substitution earlier in the lifecycle or invents an authority the
runtime does not have. `docs/F7C_BEHAVIORAL_WITNESS_ARCHITECTURE.md` carries
that argument in full.

So the question changed. Not *"is B adequate proof of A"* — unanswerable — but
*"is there a proof standard here that the record settles"*, which is a fact the
runtime holds. The distinction the product now makes:

```
I cannot confirm it holds   ≠   I know it does not hold
       (unverified)                    (falsified)
```

A behavioural obligation the judge read as satisfied, with no such standard, is
**SemanticSatisfied**. That is reported, it is useful, and it is not authority
to call the task **Verified**.

## 2. Before / after

**Before** — `completion_contract.rs`, the commit point:

```
Material Behavior A
  ↓  judge: satisfied
  ↓  ledger.observed_the_changed_tree()  →  any VerifyRecord{exit 0, after_mutation_seq > 0}
  ↓  discharged
Verified
```

That predicate is true of a run whose only check was `/usr/bin/true`. It asks
whether the run observed *anything*; it has no way to ask whether it observed
*this*.

**After**:

```
Material Behavior A
  ↓  F7-A: a citation must witness the work            (unchanged, both points)
  ↓  request point: not judged                          (unchanged — see §5)
  ↓  commit point: authoritative_proof_holds(policy)?
       None  → no standard exists   → NOT discharged
       Some(false) → standard unmet → NOT discharged
       Some(true)  → standard met   → discharged
  ↓  completion_debt() → Some(...)
CompletedUnverified
```

## 3. Authoritative proof policies

| Policy | Runtime fact it reads | Can authorize discharge | Judge can override |
| --- | --- | --- | --- |
| `CommandSuccess { commands, mode }` | a `VerifyRecord` whose normalized fingerprint equals a named command, `exit_code == 0`, `after_mutation_seq >= last_mutation_seq` | **yes** | no |
| `TestCoverage` | a cited id that resolves in the ledger **and** a fresh successful verify | **yes** | no — the citation is checked, not believed |
| `MutationScope { allowed_paths }` | every mutated path inside the scope; empty scope permits nothing | **yes** | no |
| `Unresolved` | — | **no** | no |
| *(none)* | — | **no** | no |

"Authoritative" means exactly one thing here: a policy whose truth the runtime
reads off the `EvidenceLedger` without interpreting anyone's prose.
`Unresolved` is deliberately excluded — it is the derivation saying it could not
determine the standard, which is an absence, not a standard.

## 4. Engine verification — preserved, and demoted from universal grounding

```
ENGINE_VERIFICATION_EVIDENCE_INTEGRATION_PRESENT = YES
ENGINE_VERIFICATION_DIRECT_SUCCESS_PATH          = NO
ENGINE_VERIFICATION_IS_SECOND_COMPLETION_PREDICATE = NO
```

`cc840d7` is untouched. The engine's verification plan still runs in
`conclude_direct`, still records one `VerifyRecord` per named check into the one
ledger under `engine-verification:<attempt>:<check>`, still sets
`runtime_evidence_complete`, still records a failed gate as `exit_code = 1`.

What it can prove: this exact command ran over a tree this run had changed, and
exited thus. Through `CommandSuccess`, that discharges an obligation that
*names* the command — proven end to end by
`a_named_command_that_ran_green_still_verifies`.

What it cannot prove: that a behaviour nobody attached a standard to now holds.
"Any green engine verification" was doing exactly that, and it no longer does.

## 5. Why the floor terminates instead of looping

This is the failure mode F7-B hit and had to withdraw from, and it is designed
out rather than tested around.

The contract is asked twice on one ledger. At the **request point**
(`update_goal(complete)`, inside the agent loop) `runtime_evidence_complete` is
false and a behavioural obligation is **not judged** — unchanged from F7-C. So
the floor never refuses a completion claim the agent could not possibly satisfy,
and the agent is never told to go find proof that cannot exist.

`OpenReason::MissingAuthoritativeProof` is therefore **terminal-only by
construction**: it can only arise after the engine has finished producing
evidence, at which point the run is already concluding. The run ends
`CompletedUnverified` with `stop_reason == Completed` — asserted, not assumed,
in `gap2_an_observation_of_something_else_no_longer_discharges_a_behavioural_claim`.

## 6. What was deleted

`EvidenceLedger::observed_the_changed_tree()` is gone, not merely unused.

It had one caller — the behavioural discharge rule — and removing that authority
left it with none. An authority with no callers is an invitation to wire the
same hole back in, so the function was deleted and a comment left in its place
saying why. The rule it encoded is not recoverable by accident.

```
OBSERVED_CHANGED_TREE_IMPLIES_BEHAVIORAL_GROUNDING = NO
```

## 7. The yield consequence, stated plainly

**This is the number the review must weigh, and it is larger than a percentage.**

`leveler-agent/src/completion_contract.rs::policy_from` attaches a policy only
to `Constraint` (→ `MutationScope`) and `Verification` (→ `CommandSuccess` /
`TestCoverage` / `Unresolved`). For `RequirementKind::Behavior` it returns
`None`, always. Derivation has no path that produces a grounded behavioural
obligation.

So under this floor:

```
every behavioural obligation the current derivation produces is semantic-only
  ⇒ any contract containing one cannot reach Verified
```

Against the preserved representative set:

```
REPRESENTATIVE_TOTAL                  = 32   (all RequirementKind::Behavior)
PRE_FINAL_F7_DISCHARGEABLE            = 32
POST_FINAL_F7_VERIFIED_GROUNDED       = 0
POST_FINAL_F7_SEMANTIC_ONLY_CU        = 32
POST_FINAL_F7_OTHER_OPEN              = 0
GROUNDED_BY_AGENT                     = 0
GROUNDED_BY_ENGINE                    = 0
```

This is derived, not replayed, and the distinction matters. Those 32 are
snapshots from before `cc840d7`; they carry no `runtime_evidence_complete`, so
replaying them judges at the request point and returns exactly what it always
did. The numbers above are what a **fresh run of the same obligations on this
HEAD** would produce: 32 behavioural obligations, none carrying a policy, none
grounded at the commit point.

Verification yield for behaviour-only tasks is therefore not reduced — it is
**zero**. A task whose contract is purely behavioural now ends
`CompletedUnverified` by construction. Tasks whose users named a command, a
coverage requirement or a path scope are unaffected, and that is the whole of
what still verifies.

Per §51/§75 this was not tuned away, and per §75 it does not block the
engineering gate. It is the largest open product question this line of work has
produced and it belongs to the review, not to this task.

### 7.1 The facet question, rejected deliberately

An objective of kind `Behavior` carrying a condition of kind `Verification` with
a `CommandSuccess` policy — the shape `AcceptanceFacet`'s own doc calls "where
the proof obligation actually lives" — would ground the objective if the floor
looked at conditions. It does not, and that was a decision, not an oversight:

`go test ./...` passing is the user's acceptance bar, and it is **not** evidence
about the behaviour. Letting it ground the objective reopens GAP 2 in the most
common contract shape there is. Under *宁可少，不能假* the floor stays on the
obligation's own standard.

The review may reasonably decide the other way — it is the difference between
"yield falls" and "Verified is unreachable for most tasks". The evidence is
here; the choice is not this task's to make.

## 8. Test matrix

| case | test | result |
| --- | --- | --- |
| GAP 2, unrelated green observation, real path | `gap2_..._no_longer_discharges_a_behavioural_claim` | **RED → GREEN** |
| GAP 2 at the predicate | `an_uncited_claim_does_not_close_at_commit_without_a_proof_standard` | **RED → GREEN** |
| zero ref, nothing observed | `an_uncited_claim_over_unobserved_work_is_refused_once_evidence_is_complete` | PASS |
| real recorded command, no policy | `a_real_recorded_witness_without_a_proof_standard_does_not_close_at_commit` | **RED → GREEN** |
| open reason is its own | `an_ungrounded_behavioural_claim_reports_its_own_reason` | **RED → GREEN** |
| CommandSuccess true positive (predicate) | `named_commands_that_ran_green_discharge_the_obligation` | PASS |
| CommandSuccess true positive (real path) | `a_named_command_that_ran_green_still_verifies` | **new, PASS** |
| behavioural obligation WITH a policy | `a_behavioural_obligation_with_a_named_command_closes_at_commit` | **new, PASS** |
| TestCoverage true positive | `a_cited_test_that_ran_green_discharges_the_coverage_obligation` | PASS |
| MutationScope true positive | `a_named_file_scope_becomes_a_mechanical_policy`, `a_constraint_citing_a_mutation_is_untouched` | PASS |
| scope of the rule | `the_floor_does_not_reach_other_kinds` | **new, PASS** |
| F7-A wrong evidence kind | `a_behaviour_citing_only_a_mutation_is_not_discharged`, `calling_a_mutation_an_observation_does_not_make_it_one` | PASS |
| stale / baseline check | `a_check_that_predates_the_work_proves_nothing_about_it` | PASS |
| unknown ref | `an_invented_citation_is_not_a_witness` | PASS |
| terminal truth, end to end | `a_repair_turn_that_breaks_the_declared_scope_cannot_verify` | PASS |
| fail-closed terminates | `gap2_...` asserts `CompletedUnverified` + `stop_reason == Completed` | **new, PASS** |
| durability | `an_ungrounded_behavioural_claim_is_still_open_after_a_round_trip` | **new, PASS** |

## 9. What changed in the product

Four files, 5 including tests:

| file | change |
| --- | --- |
| `leveler-lifecycle/src/completion_contract.rs` | `authoritative_proof_holds()` (one definition, shared by `Verification` and `Behavior`); `lacks_authoritative_proof()`; `behaviour_citation_is_sound()` (F7-A's rule, extracted unchanged); the `Behavior` commit-point rule; `OpenReason::MissingAuthoritativeProof` |
| `leveler-lifecycle/src/ledger.rs` | `observed_the_changed_tree()` deleted; the new open reason's wording |
| `leveler-agent/src/executor/drive.rs` | the new open reason's arm, wording, and a `rejected_ungrounded_behavior` counter |
| `leveler-test-support/src/reconcile.rs` | the derivation autopilot's stand-in obligation moved from `behavior` to `other` — it is scaffolding, and must not carry a proof obligation of its own |

No prompt change. No new model call. No new type, store, ledger, judge,
predicate or state machine.

## 10. Regressions

| gate | result |
| --- | --- |
| F1 — CommandSuccess exactness, safe `&&`, pipes fail closed | PASS |
| F2A — one absolute reconciliation deadline | PASS |
| F3 — runtime-issued evidence identity, unknown refs fail closed | PASS |
| F5 — long-thinking non-streaming transport | PASS |
| F6 — MutationScope deterministic, mechanical beats semantic | PASS |
| F7-A — mutation evidence still cannot ground behaviour | PASS |
| Engine verification evidence | PASS |
| Terminal truth | PASS |
| Context cost | PASS |

`REASONING_PASSBACK_PROMOTION=REJECTED` and the 64 KiB trimming threshold are
untouched.

### Quality gates

| gate | command | result |
| --- | --- | --- |
| fmt | `cargo fmt --all -- --check` | exit 0 |
| clippy | `CARGO_TARGET_DIR=target/clippy cargo clippy --workspace --all-targets` | exit 0, 0 warnings, 0 errors |
| workspace | `cargo test --workspace --no-fail-fast` | **135 suites, 3695 passed, 0 failed, cargo exit 0** |
| lifecycle | `cargo test -p leveler-lifecycle --lib` | 164 passed, 0 failed |
| engine | `cargo test -p leveler-engine --test direct_test` | 53 passed, 0 failed |

This run stands on its own and does not restate the F7-C result. That run was
`3689 passed, 1 failed, cargo exit 101` on an environmental browser failure;
it is recorded as it happened in `docs/F7C_BEHAVIORAL_WITNESS_CLOSURE.md` §9
and is not rewritten here. This one is green on cargo's own exit code, the same
browser suite included.

## 11. Architecture audit

```
SECOND_RUNTIME=NO                      SECOND_SCHEDULER=NO
SECOND_EVENTLOG=NO                     SECOND_EVIDENCE_LEDGER=NO
SECOND_COMPLETION_CONTRACT=NO          SECOND_COMPLETION_PREDICATE=NO
SECOND_JUDGE=NO                        SECOND_BEHAVIOR_LEDGER=NO
SECOND_VERIFICATION_STATE_MACHINE=NO

UNANCHORED_SEMANTIC_SATISFACTION_CAN_VERIFY        = NO
OBSERVED_CHANGED_TREE_IMPLIES_BEHAVIORAL_GROUNDING = NO
REAL_COMMAND_REF_WITHOUT_POLICY_CAN_GROUND_BEHAVIOR = NO
JUDGE_PROSE_CAN_AUTHORIZE_UNGROUNDED_BEHAVIOR      = NO
ENGINE_VERIFICATION_IS_SECOND_COMPLETION_PREDICATE = NO

BEHAVIORAL_WITNESS_FRAMEWORK_ADDED=NO
REQUIREMENT_PROSE_HEURISTIC_ADDED=NO
BENCHMARK_SPECIFIC_SPECIAL_CASE=NO
PRIMARY_FIX_PROMPT_ONLY=NO
NEW_MODEL_CALL_ADDED=NO
F1_SAFETY_BOUNDARY_WEAKENED=NO
```

The product diff contains no `contains()`, regex, similarity or embedding over
requirement or evidence prose, and no benchmark literal. Checked mechanically
against the diff.

## 12. Machine-readable final state

```
ENGINE_VERIFICATION_EVIDENCE_INTEGRATION_PRESENT=YES
ENGINE_VERIFICATION_DIRECT_SUCCESS_PATH=NO
GENERIC_BEHAVIORAL_WITNESS_AS_AUTHORITATIVE_PROOF=REJECTED
UNANCHORED_SEMANTIC_SATISFACTION_FOR_VERIFIED=NOT_ALLOWED
OBSERVED_CHANGED_TREE_IMPLIES_BEHAVIORAL_GROUNDING=NO
REAL_COMMAND_REF_WITHOUT_POLICY_CAN_GROUND_BEHAVIOR=NO
JUDGE_PROSE_CAN_AUTHORIZE_UNGROUNDED_BEHAVIOR=NO

F7_FINAL_GAP2_RED_TO_GREEN=PASS
F7_FINAL_ZERO_REF_RED_TO_GREEN=PASS
F7_FINAL_REAL_COMMAND_WITHOUT_POLICY_BLOCKED=PASS
F7_FINAL_UNRELATED_ENGINE_VERIFY_BLOCKED=PASS
F7_FINAL_COMMANDSUCCESS_TRUE_POSITIVE=PASS
F7_FINAL_TESTCOVERAGE_TRUE_POSITIVE=PASS
F7_FINAL_MUTATIONSCOPE_TRUE_POSITIVE=PASS
F7_FINAL_SEMANTIC_ONLY_FAIL_CLOSED=PASS
F7_FINAL_SCOPE_OF_RULE_CORRECT=PASS
F7_FINAL_END_TO_END_PATH=PASS
F7_FINAL_FAIL_CLOSED_TERMINATES=PASS
F7_FINAL_TERMINAL_TRUTH_GATE=PASS
F7_FINAL_DURABILITY_GATE=PASS

F1_REGRESSION_GATE=PASS
F2A_REGRESSION_GATE=PASS
F3_REGRESSION_GATE=PASS
F5_REGRESSION_GATE=PASS
F6_REGRESSION_GATE=PASS
F7A_REGRESSION_GATE=PASS
ENGINE_VERIFICATION_EVIDENCE_GATE=PASS
CONTEXT_COST_REGRESSION_GATE=PASS

REPRESENTATIVE_TOTAL=32
PRE_FINAL_F7_DISCHARGEABLE=32
POST_FINAL_F7_VERIFIED_GROUNDED=0
POST_FINAL_F7_SEMANTIC_ONLY_CU=32
POST_FINAL_F7_OTHER_OPEN=0
GROUNDED_BY_AGENT=0
GROUNDED_BY_ENGINE=0

SECOND_RUNTIME=NO
SECOND_SCHEDULER=NO
SECOND_EVENTLOG=NO
SECOND_EVIDENCE_LEDGER=NO
SECOND_COMPLETION_CONTRACT=NO
SECOND_COMPLETION_PREDICATE=NO
SECOND_JUDGE=NO
SECOND_BEHAVIOR_LEDGER=NO
SECOND_VERIFICATION_STATE_MACHINE=NO
BEHAVIORAL_WITNESS_FRAMEWORK_ADDED=NO
REQUIREMENT_PROSE_HEURISTIC_ADDED=NO
BENCHMARK_SPECIFIC_SPECIAL_CASE=NO
PRIMARY_FIX_PROMPT_ONLY=NO
NEW_MODEL_CALL_ADDED=NO
F1_SAFETY_BOUNDARY_WEAKENED=NO

F7_A_WRONG_EVIDENCE_KIND=CLOSED
F7_B_GAP1_ENGINE_EVIDENCE_LIFECYCLE=CLOSED
F7_B_GAP2_SEMANTIC_ADEQUACY=CONTAINED_BY_GROUNDED_AUTHORITY_FLOOR

FULL_WORKSPACE_GREEN=YES
  135 suites, 3695 passed, 0 failed, cargo exit 0

F7_FINAL_ENGINEERING_GATE=PASS
F7_STATUS=ENGINEERING_CLOSED

F7_FINAL_START_HEAD=08ce7684bf1f78d0826bf5130d354629997ca06d
F7_FINAL_HEAD=the single commit on top of F7_FINAL_START_HEAD (`git log -1`)
F7_FINAL_TREE=that commit's tree
  A commit cannot record its own hash, and this report ships inside the one
  commit this task creates. `git log -1 --format=%H%n%T` names both.
FINAL_WORKTREE_CLEAN=YES
COMMITS_CREATED=1
PUSHED=NO

OPEN_BETA_BLOCKER=1
OPEN_BETA_REQUIRED=1
PHASE_B=HOLD
```

`F7_FINAL_ENGINEERING_GATE=PASS`: every §74 requirement is met and verified.
The gate is about authority and lifecycle, not yield — §75 — and the yield
consequence in §7 is recorded rather than traded against.

The Beta blocker stays up. Engineering proof by the author of the change is not
dogfood acceptance, and §7 is a product question nobody has answered yet.

## 13. Remaining risks

1. **Verified is now unreachable for a purely behavioural contract** (§7). Not a
   defect of this change — a consequence of it meeting a derivation that never
   attaches a behavioural proof standard. The two have to be reconciled by a
   product decision, and §7.1 names the specific lever.
2. **Semantic assessment is still produced and still reported.** It explains and
   diagnoses; it no longer authorizes. Anyone reading a completion report must
   not read "satisfied" as "proven", and the open reason is worded to make that
   hard to confuse.
3. **The floor cannot detect a wrong `CommandSuccess`.** If a user names a
   command that does not actually test what they asked for, the obligation
   grounds anyway. The runtime honours the user's stated bar; it does not audit
   it.
4. **The engineering proof is the author's.** It is not dogfood acceptance, and
   the Beta blocker stays up.
