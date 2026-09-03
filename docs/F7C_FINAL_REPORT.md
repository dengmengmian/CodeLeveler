# F7-C FINAL REPORT

Baseline HEAD: eeed4d9b1b45 (F7-B closure)
Final HEAD:    cc840d79a1b5e6b0aa4ab46aa4544e78c54540a6
Final TREE:    ef3699f5f52542debbe9874536e5a510e7422c07

## 1. Root Cause

The runtime's own verification was invisible to the decision that needed it.

`conclude_direct` runs the verification plan — real commands over the changed
tree — between the agent's completion claim and the terminal contract check.
Those runs reached the EventLog as `VerificationCheck` and
`VerificationFinished` rows and **never the `EvidenceLedger`**. The terminal
check reads `last_persisted_ledger`, so the one decision that matters was
taken without the strongest observation the runtime makes on its own account.

F7-B concluded that an uncited behavioural claim could not be gated. That was
right about the symptom and wrong about the cause: the rule was fine, its
position was not. It sat at the request point, where the evidence it demanded
does not exist yet, so thirteen engine tests span their loops until their
scripts ran out.

## 2. Before Lifecycle

```
agent edits
update_goal(complete)
  └ readiness gate → completion_debt()        ← decides on agent evidence only
turn ends
conclude_direct                 engine.rs:1948
  ├ verify()                    engine.rs:2225
  │   └ VerificationCheck events → EventLog   ← real observation, unreachable
  │   └ VerificationFinished     → EventLog
  ├ up to DIRECT_REPAIR_ATTEMPTS repair turns + re-verify
  ├ finalize_task_outcome
  ├ last_persisted_ledger → completion_debt() ← decides again, still blind
  └ closure_review_stage
```

## 3. After Lifecycle

```
agent edits
update_goal(complete) → completion_debt()     ← REQUEST: judged on what exists
turn ends
conclude_direct
  ├ verify()
  │   └ VerificationCheck events → EventLog   (unchanged)
  │   └ record_verification_evidence
  │        ├ VerifyRecord per named check → EvidenceLedger
  │        └ runtime_evidence_complete = true
  ├ repair turns re-verify and re-record under a new attempt id
  ├ last_persisted_ledger → completion_debt() ← COMMIT: sees everything
  └ closure_review_stage
```

## 4. Single Completion Authority

`CompletionContract::completion_debt()`, asked twice on one ledger that grew
in between. One authority at two moments, not two authorities.

`EvidenceLedger.runtime_evidence_complete` is an **input**, not a judge: it
says which moment this is. No `if verification_passed { complete() }`, no
engine finalizer decision, no second predicate. Audited: no bypass.

## 5. Evidence Model

No new type, no new store, no schema migration. Engine checks become
`VerifyRecord`s — the same shape the agent's own verification commands
produce, with the same `normalize_command_fingerprint`, so the two are
comparable rather than parallel.

Provenance is the record id `engine-verification:<attempt>:<check>`: who
produced it, which attempt, which check. The fingerprint says what ran,
`exit_code` the result, `after_mutation_seq` the tree it observed.
Re-recording the same check in the same attempt is a no-op, so a repeated
completion attempt does not grow the ledger. A check the plan does not name is
skipped rather than given an invented fingerprint.

## 6. Behavior Witness

**Not implemented as a distinct type.** The lifecycle had to be correct first,
and with it correct the existing `VerifyRecord` carries what a witness needs
for the obligations that exist today: identity, command, result, and the
mutation watermark that says it observed changed code.

`RedGreenWitness` now has somewhere to live — an evidence producer writing
into the one ledger — and adding it needs no new completion path. It was not
needed to close GAP 1, and building it speculatively would have been a proof
system nobody had asked a question of yet.

What a verification observation proves: this command ran over a tree this run
had changed, and exited thus. What it does not prove: that the behaviour it
exercised is the behaviour an obligation names.

## 7. GAP 1

**Before:** a behavioural obligation with `refs=[]` discharged on the judge's
word, with nothing behavioural behind it.

**Now:** at the commit point it needs an observation of the changed tree —
from the agent or from the runtime, whoever ran it. At the request point it is
not judged, because the evidence is produced after that moment by design.

**Red test:** `a_behavioural_obligation_with_nothing_observed_does_not_verify`
— real Direct path, no gates, nothing observed → not Verified.

**Green evidence:** `the_runtime_s_own_verification_lets_a_behavioural_claim_close`
— same path with a gate → Verified, and `runtime_evidence_complete` is set,
so the decision knew which moment it was.

**Status: CLOSED.**

## 8. GAP 2

Still not mechanically decidable: evidence that is real, fresh, successful,
runtime-issued and correctly kinded can exercise a neighbouring behaviour.

Bounded improvement this round: what can stand in for behavioural proof is now
typed and provenanced — a recorded command with a fingerprint, an exit status
and a mutation watermark — instead of whatever the judge read in a transcript
tail. A smaller space, not a decided one.

No heuristic was introduced: no test-name matching, no path overlap, no
keyword or embedding similarity, no command-string patterns, no LLM judge.

**Semantic sufficiency is not a deterministic runtime fact unless backed by an
executable contract, a formal specification, a trusted semantic judge, or human
approval.**

**Status: BOUNDED.**

## 9. Direct Path

Covered by the real path, not a helper: `spec` with a verification plan →
`engine.run` → `conclude_direct` → `verify` → ledger → contract → outcome.
Tests: `engine_verification_reaches_the_evidence_ledger`,
`the_runtime_s_own_verification_lets_a_behavioural_claim_close`,
`a_behavioural_obligation_with_nothing_observed_does_not_verify`,
`a_failed_engine_gate_is_recorded_as_a_failed_observation`.

## 10. Agent Path

Both paths reach `conclude_direct` (engine.rs:992, 1470, 1519), so they share
one completion authority and one evidence lifecycle. The agent-side request
point is covered by the F7-B tests, which still pass unchanged.

## 11. Regression Matrix

Recomputed against the preserved HC-002 cohort with the F7-C predicate applied
exactly as implemented:

```
Representative behavioural obligations: 37 (32 objectives + facets)
Before F7-C: 37 dischargeable
After  F7-C: 37 dischargeable
New false negatives: 0
```

Old snapshots carry no `runtime_evidence_complete`, so they replay as "still
gathering" and are judged exactly as they were.

## 12. Tests

| | Command | Result |
| --- | --- | --- |
| targeted | F7-C tests in `direct_test` | 6 passed |
| lifecycle | `cargo test -p leveler-lifecycle` | 159 passed, 0 failed |
| agent | `cargo test -p leveler-agent` | 562 passed, 0 failed, 1 ignored |
| engine | `cargo test -p leveler-engine` | 12 suites, 0 failed |
| workspace | `cargo test --workspace --no-fail-fast` | **135 suites, 0 failed** |
| fmt | `cargo fmt --all -- --check` | exit 0 |
| clippy | `CARGO_TARGET_DIR=target/clippy cargo clippy --workspace --all-targets` | clean |

One agent test failed once during this work and was diagnosed as a fixture
collision from running two cargo processes at once, not a code defect: it uses
a fixed temp directory name. It passes in isolation and in the clean rerun.

## 13. Architecture Audit

```
second completion authority:   NO
verification auto-complete:    NO
refs nonempty shortcut:        NO
string heuristic:              NO
path heuristic:                NO
test-name heuristic:           NO
new model call:                NO
direct-path bypass:            NO
```

The only `starts_with` in the diff is in a test, selecting engine-produced
records for an idempotence assertion; it takes part in no decision.

## 14. Remaining Risks

1. **GAP 2 is open by design.** A correct-looking observation of the wrong
   behaviour still discharges. Closing it needs an executable acceptance
   contract, not a rule.
2. **Uncited claims still rest on a weak floor.** "Something was observed over
   the changed tree" is a real requirement but a low bar; it does not tie the
   observation to the obligation.
3. **The request point remains ungated.** That is deliberate, and it means an
   agent can still claim completion optimistically; the commit point is what
   refuses.
4. **`BehaviorWitness` is unbuilt.** The place for it exists; the type does not.

## 15. Gate

```
F7C_ENGINEERING_GATE=PASS
F7_STATUS=REVIEW_REQUIRED
OPEN_BETA_BLOCKER=1
```

F7-C's own gate passes: every item in §27 is met and verified. F7 as a whole
is `REVIEW_REQUIRED` rather than CLOSED, because GAP 2 is bounded, not
decided — and per §28 that call belongs to the F7 review, not to this task.
The Beta blocker is not cleared: engineering proof by the author of the change
is not dogfood acceptance.
