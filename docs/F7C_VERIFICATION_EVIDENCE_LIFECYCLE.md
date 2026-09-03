# F7-C — Verification Evidence Lifecycle: architecture

Date: 2026-09-04. Follows `F7B_BEHAVIORAL_WITNESS_ARCHITECTURE.md`.

## 1. The finding

F7-B concluded that an uncited behavioural claim could not be gated, because
gating it made completion unreachable. That conclusion was right about the
symptom and wrong about the cause. The cause was not the rule. It was that the
runtime's own verification was invisible to the decision that needed it.

**`request complete` and `commit complete` were already separate.** Nothing had
to be built:

| Point | Where | What it is |
| --- | --- | --- |
| Request | `update_goal(complete)` → readiness gate → `completion_debt()` | the agent asking to finish |
| Commit | `conclude_direct` → terminal `last_persisted_ledger` → `completion_debt()` | the runtime deciding |

Between them, `conclude_direct` runs the verification plan — real commands over
the changed tree, the strongest observation the runtime makes on its own
account. Those runs reached the EventLog as `VerificationCheck` rows and
`VerificationFinished`, and **never the `EvidenceLedger`**. So the commit-point
decision was taken without the runtime's own evidence, and a rule asking a
behavioural obligation for a witness could not be satisfied by the very check
the product runs to satisfy it.

F7-B put its rule at the request point, where the evidence does not exist yet.
That is why thirteen engine tests span their loops.

## 2. Lifecycle, before and after

```
BEFORE
  agent edits
  update_goal(complete) ──► completion_debt()      ← decides, sees agent evidence only
  turn ends
  conclude_direct
    └ verify()  ──► EventLog                       ← real observation, unreachable
    └ terminal completion_debt()                   ← decides again, still blind to it

AFTER
  agent edits
  update_goal(complete) ──► completion_debt()      ← REQUEST: judged on what exists
  turn ends
  conclude_direct
    └ verify()
        └ VerificationCheck events (unchanged)
        └ record_verification_evidence
             ├ VerifyRecord per named check, into the EvidenceLedger
             └ runtime_evidence_complete = true
    └ terminal completion_debt()                   ← COMMIT: sees everything
```

## 3. Single completion authority

`CompletionContract::completion_debt()` remains the only thing that decides.
It is asked twice, on one ledger, which grew in between. That is one authority
evaluated at two moments, not two authorities.

What was added is an *input*, not a judge: `EvidenceLedger.runtime_evidence_complete`
says which of the two moments this is. A rule that needs runtime evidence stays
silent while it is false, because refusing there asks for proof that does not
exist yet.

Explicitly not added: no `if verification_passed { complete() }`, no engine
finalizer decision, no second predicate. A green gate recorded as evidence
still cannot discharge an obligation the contract holds open — pinned by a
test.

## 4. Evidence model

No new type, no new store. The engine's checks become `VerifyRecord`s, the same
shape the agent's own verification commands produce, with the same
`normalize_command_fingerprint` so the two are comparable.

Provenance is the record id: `engine-verification:<attempt>:<check name>`.
It answers who produced it (the engine, not a tool call), which attempt, and
which check; the fingerprint answers what ran; `exit_code` the result;
`after_mutation_seq` the tree it observed. Re-recording the same check in the
same attempt is a no-op, so a repeated completion attempt does not grow the
ledger.

A check the plan does not name is skipped rather than given an invented
fingerprint: an unmatchable string in the evidence vocabulary is worse than an
absent record.

## 5. GAP 1 — closed

A behavioural obligation is judged at the commit point, and there it needs an
observation of the changed tree — from the agent or from the runtime, whoever
ran it. Not "any ref": citing the edit is citing the work, not an observation
of it, which is F7-A's rule and still holds.

```
cited?  ─ yes ─►  a cited id must be a recorded command, exit 0,
                  observed after work began
        ─ no  ─►  still gathering evidence?  ─ yes ─► not judged
                                             ─ no  ─► the run must have
                                                      observed the changed tree
```

## 6. GAP 2 — bounded, not closed

Unchanged and deliberately so. Evidence that is real, fresh, successful,
runtime-issued and correctly kinded can still exercise a neighbouring
behaviour. Deciding otherwise is semantic adequacy of natural language.

What F7-C narrows: the space of things that can stand in for behavioural proof
is now typed and provenanced — a recorded command with a fingerprint, an exit
status and a mutation watermark — rather than "whatever the judge read in the
transcript tail". That is a smaller space, not a decided one.

**Semantic sufficiency is not a deterministic runtime fact unless backed by an
executable contract, a formal specification, a trusted semantic judge, or human
approval.** None of those exist here, and none were faked.

## 7. Future producers

`ExecutableAcceptanceScenario`, `FormalContractWitness`, `HumanApprovalWitness`,
`SemanticJudgeWitness`, `ExternalSystemWitness` all fit the shape this
establishes: an evidence producer writing into the one ledger, read by the one
contract. None of them gets completion authority of its own. The engine
verification is now the first non-agent producer, and it took no new
completion path to add.

## 8. Not done here

`BehaviorWitness` / `RedGreenWitness` as a distinct typed witness is not
implemented. The lifecycle had to be correct first, and with it correct the
existing `VerifyRecord` carries what a witness needs for the obligations that
exist today. Adding a red-before/green-after witness type is the natural next
step and now has somewhere to live; it is not needed to close GAP 1.
