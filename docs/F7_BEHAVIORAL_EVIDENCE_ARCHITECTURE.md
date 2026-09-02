# F7 — Behavioral Evidence Grounding: architecture decision

Date: 2026-09-02. Reads `F7_BEHAVIORAL_EVIDENCE_CHARACTERIZATION.md`.

```
F7_ARCHITECTURE_GATE=PASS
```

## 1. Current authority model

| Layer | Owns |
| --- | --- |
| Runtime | evidence identity, provenance, freshness, the recorded facts (which paths a tool call changed; which command ran, its exit status, its mutation watermark) |
| Judge | interpretation: whether those facts satisfy the user's wording |
| `discharged()` | the single predicate that says whether a Satisfied obligation is really discharged |

F1 fixed which commands may be authoritative. F3 made evidence identity
runtime-issued, so an invented ref fails closed. F6 made a mechanically known
fact beat a judge's contrary reading.

## 2. The exact F7 gap

`resolve_refs` maps a judge's citation to a `tool_call_id` and **drops the
candidate's kind**. The runtime knows whether an id is a command that ran or a
file that changed; it discards that at the boundary where the obligation is
bound. `discharged()` then applies no rule at all to behavioural kinds
(`_ => true`).

So a behavioural obligation can be discharged by citing a file edit as the
witness of a behaviour. That is a **category error the runtime can detect
without reading one word of prose** — and it is exactly what run-02 did, three
times.

## 3. Options considered

**A. Require a behavioural obligation to cite a verification witness.**
Rejected on measured evidence, not on taste: 22 of the 32 behavioural
obligations in the preserved successful HC-002 cohort cite nothing at all
(run-03 cites nothing for all 10). This rule blocks every one of them. That is
the §56 "broad legitimate completion collapse", demonstrated rather than
feared. It might be recoverable — the judge could cite the verifications that
do exist — but that is a bet on model behaviour, which §18 forbids as a
primary fix.

**B. Require `strength: observed` to cite a verification.** Rejected: HC-002
run-03 discharges 10 behavioural obligations at `observed` with no refs, so
this collapses the same cohort. It also makes a judge-declared field
load-bearing, which is the wrong direction.

**C. Add an `ObservedBehavior` evidence policy and a behaviour-witness record
carrying program output.** Rejected for this step. It is a real capability
(and the thing O4-B also wants), but it is a new evidence subsystem, new
durable state, new prompt contract and a new derivation obligation — a large
speculative framework built to make one test green (§70). Nothing in the
preserved evidence shows the judge would use it correctly, and §21 forbids
pretending arbitrary semantics are mechanical.

**D. Prompt: "do not confuse invalid with zero-value."** Rejected by §18, and
it does not generalize past this fixture.

**E. Stronger or second judge.** Rejected by §19.

**F. Sound citation: a cited witness must be capable of witnessing the
claim.** Selected.

## 4. Selected design

One rule, in the runtime, at the single existing predicate:

> A behavioural obligation that **cites** evidence is discharged only if at
> least one cited id resolves to an **observation** — a command the runtime
> recorded running over the tree. A citation that resolves only to mutations
> is a category error: a file changing is not the behaviour changing.

An obligation that cites nothing is unchanged. That is deliberate and it is
the honest limit of this step: see §7.

Data flow, all of it existing:

```
EvidenceCandidate { id, tool_call_id, kind: "verification" | "change" }
        │  judge cites E-ids
        ▼
resolve_refs   → tool_call_ids                        (unchanged; F3 intact)
        ▼
RequirementEvidence { strength, detail, refs }        (unchanged schema)
        ▼
discharged(kind = Behavior, …, ledger)
        │  ledger.verifications ∋ ref ?               ← the new question
        ▼
completion_debt()  →  terminal status                 (unchanged)
```

`EvidenceLedger` already keys both `mutations` and `verifications` by
`tool_call_id`, so classifying a ref needs no new state, no schema change and
no new record type.

## 5. Why this is the minimal authority fix

It restores a distinction the runtime **already owned** and had discarded. It
adds no fact, no store, no call and no model. It is the F3 rule carried one
step further: F3 says a cited id must exist; F7 says a cited id must be able
to mean what it is cited for.

Runtime keeps Observation. Judge keeps Interpretation. The judge may still
argue that a recorded `go test ./...` demonstrates the requirement — that is
interpretation, and interpretation is its job. What it may no longer do is
nominate a file edit as the observation.

## 6. The §25 question, answered directly

*How does this prevent run-02 from becoming Verified even if the judge tries
to bind "invalid record" as evidence of "zero-value record"?*

It does not engage with that argument at all, which is the point. R1, R1.F1
and R1.F2 each cite exactly one id, and that id is the `apply_patch`
mutation. The runtime rejects the citation because a mutation cannot witness a
behaviour — before any question of what the prose means. All three stay open,
`completion_debt() > 0`, `Verified=NO`.

The answer is **not** "the model should notice the difference now."

## 7. What this does not close, stated plainly

An obligation that cites **nothing** still discharges on the judge's word.
Had run-02's judge omitted its refs, this rule would not have caught it.

That hole is real and it is left open on measured grounds: closing it blocks
22 of 32 behavioural obligations in the preserved successful cohort. Closing
it properly needs option C — a behaviour-witness class that actually exists,
so that "cite a witness" is a requirement a correct run can satisfy. That is
the natural home for O4-B as well, and it should be designed with the yield
evidence a real cohort provides, not inferred here.

Recorded so the next step starts from the truth: **F7 as implemented closes
mis-kinded citation, not uncited semantic discharge.**

## 8. Interaction with F1 / F3 / F5 / F6

| | Effect |
| --- | --- |
| F1 | none. Which commands may be authoritative is untouched; this reads the ledger F1 governs. |
| F3 | strengthened, same direction. F3 drops an id the runtime never issued; F7 drops an id that cannot bear the claim. |
| F5 | none. No transport or timeout change. |
| F6 | none, and no overlap. F6 is a mechanical fact refuting a judge; F7 is a citation that cannot support one. `MutationScope` still short-circuits before the kind rules. |

## 9. Persistence and restart

No durable state added. `RequirementEvidence` and `EvidenceLedger` keep their
shapes, so existing snapshots replay unchanged and a contract recovered after
restart is re-evaluated by the same predicate against the same ledger. A
restart therefore cannot turn an open obligation into a discharged one.

## 10. Model requests

```
NEW_MODEL_CALL_ADDED=NO
```

The fix is a predicate over facts the runtime already holds.
