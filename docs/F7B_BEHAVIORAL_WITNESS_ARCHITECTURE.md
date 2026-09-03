# F7-B — Behavioral Witness / Semantic Adequacy: architecture decision

Date: 2026-09-03. Follows `F7_BEHAVIORAL_EVIDENCE_ARCHITECTURE.md` (F7-A).

```
F7B_ARCHITECTURE_GATE=REVIEW_REQUIRED
```

F7-B was asked to close two gaps. It closes neither outright: one is not
mechanically decidable with the facts the runtime holds (§7), and the rule for
the other cannot live where the predicate lives without breaking the mainline
completion path (§6, Rule B). What it does deliver is a real tightening of
citations and the repair of a yield regression F7-A shipped (§4).

`REVIEW_REQUIRED` is the honest reading of §66, not a hedge.

## 1. Where a behavioural obligation is decided today

```
derive_contract (before the first round)
    ↓  frozen requirement text, kind, evidence_policy
executor runs; ToolHost records MutationRecord / VerifyRecord
    ↓
reconcile_completion  → ReconcileInput
    ↓
parse_outcome → resolve_refs(candidate ids → tool_call_id, unknown dropped)
    ↓
apply outcome → RequirementStatus::Satisfied + RequirementEvidence{strength,detail,refs}
    ↓
discharged(kind = Behavior, …, ledger)      ← the only authority
    ↓
completion_debt() → terminal status
```

After F7-A that last step reads: an obligation citing evidence needs one
citation that `witnesses_behavior` — a recorded command, exit 0, fresh
relative to the last mutation. An obligation citing nothing discharges.

## 2. What the judge sees when `refs=[]`

`refs=[]` does **not** mean "no evidence was visible". `ReconcileInput`
carries, every time:

| Field | Content |
| --- | --- |
| `recent_evidence` | the last 3 tool results, tail-bounded to 1,500 chars each |
| `recent_claims` | the executor's recent prose |
| `modified_files` | distinct paths the run changed |
| `fresh_verification` | whether a check succeeded since the last edit |
| `evidence_candidates` | the runtime-issued `E1…En` |
| contract | the frozen requirement text |

```
ZERO_REF_SEMANTIC_INPUT_SOURCES = bounded recent tool output
                                + executor claims
                                + modified path list
                                + a freshness boolean
```

So an uncited discharge is the judge reading a transcript tail and not
anchoring its conclusion to an id. That is weak, but it is not nothing — and
it is why a rule of "no ref, no discharge" is the wrong shape.

## 3. What the runtime already stores

| Fact | Stored? |
| --- | --- |
| tool_call_id | yes |
| program + args (normalized fingerprint) | yes |
| exit code | yes |
| mutation sequence at the time of the check | yes |
| paths a tool call changed | yes |
| whether a check predates all work (`after_mutation_seq == 0`) | yes |
| **stdout / stderr** | **no** |
| **what input a program was given** | **no** |
| test source, diff content, file content | no |

`baseline_green_verifications()` and `only_baseline_green_evidence()` already
exist and already encode the discriminating idea: *a check that passed before
anything changed has demonstrated that the code already satisfied it, so it is
not proof that the work did something.* Today that notion is used once, at the
terminal boundary, to demote `Verified` → `CompletedUnverified`.

## 4. A yield regression F7-A shipped, found here

F7-A's closure report claimed `32 of 32` behavioural obligations remain
dischargeable. **That was measured wrong.** It classified by "does the
citation name a verification", while the implemented predicate additionally
requires freshness relative to the *last* mutation.

Re-measured against the preserved cohort with the predicate as implemented:

| Run | cited & kept | **cited & blocked** | uncited | any green check after work began |
| --- | ---: | ---: | ---: | --- |
| hc002 run-01 | 7 | 0 | 0 | yes |
| hc002 run-02 | 0 | **4** | 15 | yes |
| hc002 run-03 | 0 | 0 | 11 | yes |

run-02's three checks (`go build ./...`, `go test ./...`,
`go test -v ./internal/report/`) all ran green after its substantive edits.
Freshness was then destroyed by a later write to `.sanity/navsvc` — a harness
sanity artifact, not product code. So four obligations of an externally
accepted run are blocked by a file touch unrelated to any of them.

F7-B must repair this, not preserve it.

## 5. Options considered

**A. `refs=[]` never discharges.** Rejected: 26 of 32 obligations in the
preserved cohort are uncited, including all 11 of run-03. This is the §8
collapse.

**B. `refs=[]` requires a check fresh against the last mutation.** Rejected
for the same reason as §4: run-02 has none, so all 15 of its uncited
obligations would also fall. One of three successful runs becomes CU.

**C. Freeze a discriminating witness at derivation — a check that must fail
now and pass after.** This is the strong form, and it would close semantic
substitution properly: a check for the wrong behaviour would not have been
failing beforehand. Rejected *for now*, not on principle: no run in the
preserved cohort establishes a red baseline for any obligation, so requiring
one blocks 32 of 32. It is the right target and it is recorded as such.

**D. Compare requirement prose to evidence prose.** Forbidden by §6, and it
would not work: "invalid" and "zero-value" have high lexical overlap and
opposite extensions.

**E. Second judge / stronger model / vote.** Forbidden by §4.

**F. Two mechanical rules over facts the ledger already holds.** Selected.

## 6. Selected design — one rule, after one was tried and withdrawn

**Rule A — a citation must observe work, not predate it.** `witnesses_behavior`
changes its freshness test from "at or after the *last* mutation" to "after
*any* mutation". A baseline-green check — one that passed before anything
changed — is still rejected, which is the discrimination that matters. A check
invalidated only by a later unrelated write is no longer discarded, which
repairs §4. F7-A's kind rule is untouched: a mutation is never a witness.

**Rule B — an uncited claim needs the run to have observed the changed tree.
Implemented, tested, and WITHDRAWN.** It is recorded here because the reason it
cannot exist is itself the finding.

`update_goal(complete)` consults `discharged()` from inside the agent loop. On
the Direct path the runtime's own verification plan runs *after* that claim,
in `conclude_direct`, and its results never enter the `EvidenceLedger` — only
verification-class commands the agent itself ran do. So a rule requiring the
agent to have observed its own work makes `update_goal(complete)` unreachable
for every task that verifies the way the product is designed to verify. It is
not a modest yield cost: thirteen engine tests span their loops until their
scripted responses ran out, and they are the mainline Direct shape.

Moving the rule to the terminal boundary, where the plan has run, would mean a
second completion predicate — the thing §4 refuses, and the thing that made
this class of defect possible in the first place.

So the uncited path stays ungated, and two tests are named for that fact
rather than deleted, one at the predicate and one on the real completion path.

```
                    cited?
                   /      \
                 no        yes
                 |          |
            discharge   some cited id is a recorded command,
        (the open gap)  exit 0, observed after work began
                                    |
                                discharge
```

## 7. What this does NOT close — stated before implementing

The §19 red-team question: requirement A; evidence B is real, fresh,
successful, runtime-issued, correctly kinded, and observed after work began —
but B exercises a related-but-different behaviour, and the judge says B proves
A.

**This design does not reject that.** It rejects citations that cannot
discriminate: baseline-green, wrong kind, invented, failing. It cannot reject a
real observation of the wrong thing, because deciding that "a check exercising
category B does not establish category A" is semantic adequacy of natural
language, and §6 rightly forbids faking it with string comparison.

Nor does it reject an uncited claim, per §6 above.

Closing it needs option C: a witness contract frozen at derivation that a
correct run can satisfy and an incorrect one cannot — most plausibly
red-before / green-after, which is mechanically checkable on both halves. No
run in the preserved cohort has that shape, so adopting it now would block
everything. It needs a cohort designed to produce discriminating witnesses,
not a rule bolted onto one that was not.

Per §66, that makes the engineering gate `REVIEW_REQUIRED`, not `PASS`.

## 8. Interaction with F1 / F3 / F6 / F7-A / O4-B

| | Effect |
| --- | --- |
| F1 | none; which commands may be authoritative is untouched |
| F3 | intact; an invented id still resolves to nothing and cannot witness |
| F6 | none; `MutationScope` still short-circuits before the kind rules |
| F7-A | kind rule preserved exactly; only its freshness test is corrected, and the baseline rejection it relied on is preserved |
| O4-B | unchanged. Option C is the shared answer: a witness class that exists lets O4-B bind real work and stops F7-B asserting unreal work |

## 9. Persistence, restart, cost

No schema change, no new store, no new model call, no new prompt authority.
`RequirementEvidence` and `EvidenceLedger` keep their shapes, so snapshots
replay and a contract recovered after restart is re-evaluated by the same
predicate against the same ledger. Nothing is added to any prompt, so the
context-cost work of the previous batch is untouched.
