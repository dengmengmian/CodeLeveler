# F7-C — Behavioral Witness Contract: Architecture Decision

```
F7C_ARCHITECTURE_GATE=REVIEW_REQUIRED
```

Baseline HEAD at task start: `d243b2e2c662bc9a6f7ed4eef77d167f45b4e744`
Baseline TREE at task start: `80912059df0453cb118e5948be0a9e7fe49de8af`
`eeed4d9b1b45` (F7-B closure) is present in this ancestry.

This document is the §33 architecture gate. It stops before implementing a
Behavioral Witness Contract, and states why. The stop is not "more design
needed": it is a specific claim, with the measurement that supports it, that
the witness as scoped **cannot be made mechanically authoritative for the
obligations that actually need it**, and that the yield question the decision
turns on cannot be answered from anything this repository holds.

---

## 0. What was already true when this task started

`cc840d7` had already landed the first of the four hard requirements. The
engine's verification plan — real commands over the changed tree, run in
`conclude_direct` between the agent's completion claim and the terminal
decision — now writes `VerifyRecord`s into the one `EvidenceLedger`, and sets
`runtime_evidence_complete`. `completion_debt()` is asked twice on one ledger
that grew in between: one predicate, one authority, two moments.

So the work of this task was the remaining three, and all three are one
question: **GAP 2, semantic adequacy.**

---

## 1. Why `refs=[]` is currently authoritative

`completion_contract.rs::discharged`, `RequirementKind::Behavior`:

```rust
if !cited.is_empty() {
    return cited.into_iter().any(|id| ledger.witnesses_behavior(id));
}
if !ledger.runtime_evidence_complete {
    return true;            // the REQUEST point: evidence is still being made
}
ledger.observed_the_changed_tree()   // the COMMIT point
```

An uncited behavioural claim is not judged at the request point, because the
runtime's own verification runs after it — F7-B implemented the refusal there,
watched thirteen engine tests spin their loops until their scripts ran out, and
withdrew it. At the commit point it needs `observed_the_changed_tree()`: some
recorded command, exit 0, run after work began.

That is a floor, and it is a very low one. It asks whether the run observed
*anything*; it cannot ask whether it observed *this*.

## 2. Why F7-A cannot solve semantic adequacy

F7-A decides a **category** question the runtime can answer from its own
record: a `MutationRecord` is not a `VerifyRecord`, so a file edit can never
witness a behaviour. `witnesses_behavior` adds three more of the same kind —
exit code, ordering against the first mutation, and resolvability of the id.

Every one of these is a fact the ledger holds. Semantic adequacy is not:

```
the ledger knows   tool_call_id · normalized program+args · exit code
                 · mutation seq at run time · which paths a call changed
the ledger never   stdout · stderr · program input · test source · diff content
knows              what a command was ASKING of the code
```

"This command establishes category A rather than neighbouring category B" is
not derivable from that vocabulary, at any strength of rule.

## 3. How Engine Verification becomes EvidenceLedger input

Shipped in `cc840d7`; unchanged by this task, and re-audited here.

```
conclude_direct
  ├ verify()
  │   ├ VerificationCheck / VerificationFinished → EventLog   (unchanged)
  │   └ record_verification_evidence
  │        ├ one VerifyRecord per NAMED check → EvidenceLedger
  │        │   id  = engine-verification:<attempt>:<check>
  │        │   fp  = normalize_command_fingerprint(program, args)
  │        │   exit= 0 iff CheckStatus::Passed
  │        │   seq = last_mutation_seq() at record time
  │        └ runtime_evidence_complete = true
  ├ ≤ DIRECT_REPAIR_ATTEMPTS repair turns, each re-verifying under a new attempt id
  ├ finalize_task_outcome            → a verification verdict, nothing more
  ├ last_persisted_ledger → completion_debt()   ← the decision
  └ closure_review_stage
```

A check the plan does not name is skipped rather than given an invented
fingerprint. Re-recording the same check in the same attempt is a no-op. No new
type, no new store, no schema migration, no new model call.

## 4. How one canonical completion predicate is preserved

`EvidenceLedger::completion_debt()` is the only thing that decides, and it has
exactly one production caller on this path. `finalize_task_outcome` produces a
verification verdict which the contract can only **demote**; there is no
`if verification_passed { complete() }` anywhere.

`runtime_evidence_complete` is an **input** to that one predicate, not a second
judge: it says which of the two moments the predicate is being asked at.

This is now tested end-to-end and not only asserted —
`a_repair_turn_that_breaks_the_declared_scope_cannot_verify` (§9).

## 5. What a Behavioral Witness would actually mean

A witness worth the name must be **discriminating**: it must separate "A holds"
from "A does not hold". Restating the requirement is not a witness —

```
requirement   zero-value rows are omitted from the summary
witness       verify that zero-value rows are omitted from the summary   ← no authority added
```

so a witness needs concrete content the requirement prose does not have: a
stimulus, an operation, an expected observable relation. That content has to
come from somewhere. There are exactly two sources.

**Source 1 — the user supplied it.** An example input, a boundary value, an
expected output, a named command, a named test, a named path, an explicit
acceptance detail. Then the witness is a *quotation*, and its provenance is
auditable against the frozen goal.

**Source 2 — the model invented it.** Then the witness is an *inference* about
what the requirement means, and nothing downstream can audit it.

## 6. Source 1 is already implemented, and it is not a witness type

Everything derivable from Source 1 is already expressible in the contract that
exists, and derivation already extracts it under a fail-closed rule:

| the user wrote | existing policy | derivation rule today |
| --- | --- | --- |
| a command that must pass | `CommandSuccess { commands, mode }` | "list them **exactly as written**" |
| "covered by a test" | `TestCoverage` | both halves or neither |
| the part of the tree the work may touch | `MutationScope { allowed_paths }` | "**only** when the task names the paths … a guessed scope is worse than none" |

A `BehavioralWitnessSpec` carrying stimulus/operation/expected-observation
would, for this class, be a second spelling of `EvidencePolicy` — a second
proof vocabulary over the same ledger, which §10 of the task forbids and which
buys nothing.

**So the witness type is only interesting for Source 2 — and Source 2 is
exactly where it cannot be made authoritative.**

## 7. How witness A/B substitution at derivation time is contained — it is not

This is the §19 question, answered without hedging.

Four candidate containments were considered.

**C1 — provenance: the discriminating detail must appear in the frozen goal.**
Rejected. It checks that the detail came from the user; it does not check that
it came from *this obligation's* sentence. A goal with two bullets — A "omit
zero-amount rows", B "reject malformed rows" — lets a witness quoted from B's
bullet be attached to A. Provenance passes, substitution happened. Making the
span attribution itself authoritative just moves the model's claim into an
integer.

**C2 — red-before / green-after** (F7-B §5 option C, recorded there as the
right target). A witness frozen at derivation must FAIL on the pre-work tree
and PASS after. This is genuinely strong and it is mechanically checkable on
both halves. It rules out a pre-existing suite that would have passed anyway,
and it rules out a witness for behaviour the run did not change.

It does not rule out A→B. If the executor implements B and B was not previously
implemented, a witness for B is red before and green after. It passes.

What C2 *does* contain is the failure mode F7 was opened on: `icg-6r` run-02,
where the executor discovered A was unsatisfiable **during execution** and
reinterpreted it as B. A witness frozen at derivation predates that discovery.
That is a real and valuable containment — and it is containment of
execution-time reinterpretation, not of derivation-time misreading. The residual
is "the derivation model read A correctly", which is precisely the explanation
§34 forbids as the *only* basis.

It is worth being exact about why that residual is new. Deriving requirement
*text* is already model work, but the derivation prompt constrains it to
quotation ("Keep the user's own wording; do not soften, generalise, merge, or
split"), and the result is auditable against the goal. Deriving a *witness* from
a requirement the user gave no proof material for is invention, and invention
has no such audit. Witness derivation adds a degree of freedom the contract
does not currently have.

**C3 — derive the requirement back from the witness and compare.** A second
semantic evaluator. Forbidden (§8, §34).

**C4 — fail closed: no Source-1 material, no witness, no authoritative
Verified.** This is safe and it is the only safe one. Its entire cost is yield,
and §8 measures that cost in §9 below.

## 8. What happens when a safe witness cannot be derived

Under C4 the obligation is `SemanticOnly`: the judge may still describe it, and
that description may not authorize `Verified`. The terminal outcome is
`CompletedUnverified` — useful work delivered, completion not claimed.

No new enum is needed to express it. `EvidencePolicy::Unresolved` already means
"the proof standard could not be determined" and already fails closed; a
behavioural obligation with no Source-1 material is that case.

## 9. How existing successful semantic tasks avoid blanket failure — they do not,
## and the number cannot be measured here

This is the second, independent reason for the stop.

The representative cohort, from the preserved successful HC-002 runs
(`docs/F7B_BEHAVIORAL_WITNESS_CLOSURE.md` §3):

```
behavioural obligations           32
  cited a verification             6   (all in run-01)
  cited nothing                   26   (run-02: 15, run-03: 11)
runs establishing a red baseline   0
```

Applying each candidate rule to that cohort:

| rule | obligations blocked | cohort effect |
| --- | ---: | --- |
| today (`observed_the_changed_tree`) | 0 | 32/32 dischargeable, and GAP 2 open |
| C4 — uncited material behaviour never verifies | 26 | 2 of 3 successful runs become CU |
| C2 — red-before/green-after witness required | 32 | every run becomes CU |

C2, the only candidate that meaningfully narrows A→B, blocks the entire
representative cohort. That is §79's "material closure collapse", measured
rather than feared.

**And the re-measurement §48/§80 asks for cannot be produced in this task.**
The cohort is preserved *run records*, produced before `cc840d7`. Their ledgers
carry no engine-verification records and no `runtime_evidence_complete`, so
they replay as "still gathering" and are judged exactly as they were — the
F7-C closure report says so in its own §11. Learning how many of the 26 the
engine's verification would now ground requires **fresh runs on the current
HEAD**, which is dogfood, and dogfood is excluded from this task by §114/§133.

```
ZERO_REF_BEFORE_ENGINE_EVIDENCE                   = 26 / 32
ZERO_REF_AFTER_ENGINE_EVIDENCE                    = NOT MEASURABLE HERE
UNGROUNDED_MATERIAL_BEHAVIOR_AFTER_ALL_EVIDENCE   = NOT MEASURABLE HERE
```

Choosing between "GAP 2 stays open" and "26 of 32 stop verifying" without that
number would be picking a product-viability outcome on a guess.

## 10. Why this is not a second planner / evaluator / evidence system

Nothing was added. This task changed no product source. The design that was
*considered* would have lived inside `CompletionRequirement` /
`AcceptanceFacet` as a proof standard, executed through the existing ToolHost,
recorded as ordinary `VerifyRecord`s, and judged by the same
`completion_debt()`. That containment is sound; it is not what fails. What
fails is §7.

---

## 11. Decision

```
F7C_ARCHITECTURE_GATE=REVIEW_REQUIRED
```

Two §107 conditions hold, independently, and either alone is sufficient:

1. **Safe witness derivation cannot be defined without trusting the same
   semantic model authority that caused F7.** For obligations where the user
   supplied proof material, the existing `EvidencePolicy` already covers it and
   a witness type adds nothing. For obligations where they did not — the class
   that needs the witness — the discriminating content must be invented, and no
   auditable constraint catches an invention that describes B (§7).

2. **The yield question the decision turns on is not answerable from this
   repository.** The measurement that would say whether fail-closed is
   affordable requires fresh runs on the current HEAD (§9).

Per §132 this stops before implementation. What was done instead is in
`docs/F7C_BEHAVIORAL_WITNESS_CLOSURE.md`: the semantic-substitution defect is
now reproduced as a named test on the real completion path, and the terminal
enforcement point — which had no end-to-end coverage — now has one.

## 12. What the review has to decide

1. Accept `REVIEW_REQUIRED` and commission an **acceptance-detail cohort**: a
   task set whose goals carry explicit stimulus/expected-output material, on
   which C2 (red-before/green-after over Source-1 witnesses) can be measured
   for both true-positive closure and false blocking. C2 is the right target;
   it needs a cohort designed to produce witnesses, not a rule bolted onto one
   that was not.
2. Or accept GAP 2 as a **documented Beta limitation** with the floor as it
   stands, and say so in the Beta risk register rather than in a closure report.
3. Or accept the C4 collapse deliberately, after the §9 number is measured on
   fresh runs — which is a dogfood task, not this one.

Not on the table: a stronger judge, a second judge, a vote, prose comparison, a
benchmark special case, or declaring the gap closed because a case went green.
