# Completion Evidence Binding / Contract Closure

The work package that came out of post-closure C2: a tree that passes the
frozen oracle, a judge that read the objective as satisfied, fresh green
checks on the ledger — and three refused completion claims ending in a forced
closeout.

```
COMPLETION_AUTHORITY_ROOT_CAUSE=THE_REQUEST_POINT_WAS_NEVER_REOPENED

PROOF_PRODUCTION=PASS
PROOF_BINDING=DEFECT      (derivation gives a behavioural obligation no standard)
PROOF_FRESHNESS=PASS
PROOF_CONSUMPTION=PASS    (a policy, when present, is consumed correctly)

MODEL_SUMMARY_DEPENDENCY=NO
TRUNCATED_OUTPUT_AUTHORITY_LOSS=NO

FALSE_VERIFIED_REGRESSION=0
```

Nothing here lowers the bar for `Verified`. The debt is unchanged and still
withholds the claim; what changed is that the runtime stopped demanding work
that cannot possibly discharge it.

---

## 1. The trace

post-closure C2 (`06-C2--codeleveler`), read from its own ledger:

| | |
| --- | --- |
| last mutation | seq 31, `src/__tests__/nested-null.test.tsx` |
| fresh green checks | VER 32/33/34, `after_mutation_seq = 31`, exit 0 — one of them `jest … nested-null -t Con…`, the regression case itself |
| `has_fresh_successful_verify()` | true — and the refusal records say so: `[workspace_facts.freshness=fresh]` |
| R1 | `kind: behavior`, **`evidence_policy: null`**, cited evidence `strength: semantic` whose refs include `call_00_StGMZ…` = VER 33 |
| refusals | 3 — two `satisfied by reading only — the runtime has no proof standard for it`, one `not satisfied` |
| final tree | passes the frozen `c2-check.sh` |

So the receipts existed, were fresh, were cited, and the runtime resolved the
citation to its own record. What did not exist was a *standard*: R1 was
derived without an `evidence_policy`, and `authoritative_proof_holds(None, …)`
answers `None` — there is nothing to settle.

## 2. Which of the three failures this is

- **A. PROOF_NOT_PRODUCED** — no. VER 32/33/34 ran and are on the ledger.
- **B. PROOF_PRODUCED_BUT_NOT_BOUND** — yes, at derivation. The derivation
  prompt asks for a proof standard only for a `verification` obligation
  (`command_success` / `test_coverage`) and a `constraint` (`mutation_scope`).
  A `behavior` obligation is never offered one, so a bug report — which is
  what C2 is — produces an obligation nothing can discharge. Had R1 carried
  `TestCoverage`, `authoritative_proof_holds` would have discharged it from
  the record alone: a cited ref that resolves, plus a fresh green check, both
  of which were there.
- **C. PROOF_BOUND_BUT_NOT_CONSUMED** — no. Where a policy exists it is read
  off the ledger without consulting anyone's prose. `evidence_strength` *is*
  model-authored, but the `Behavior` branch never reads it, so the model's
  summary is not what decided this.

That is the binding defect, and it is F7 working as designed: the runtime
refuses to guess that a green suite proves a particular sentence.

## 3. The defect that actually cost the run

`EvidenceLedger::runtime_evidence_complete` is what tells the completion
predicate whether it is being asked at the **request** point (inside the agent
loop, before the verification plan has run) or at the **commit** point (the
terminal boundary, after). Its own documentation says so.

It was only ever set to `true`, and never cleared.

So after the first verification cycle, every later in-loop completion claim
was judged at the commit standard — and at that standard a behavioural
obligation with no proof policy can never pass. The gate refused and told the
agent:

> An obligation to demonstrate something … is discharged by actually running
> it and showing it green over the current tree — not by stating that it was
> done. Finish them and complete again.

For this obligation that instruction cannot be followed. C2 answered it with
three more green jest runs, was refused for the same reason each time, and
was force-stopped.

**C1 is the control the same cohort provides.** Its flag was still `false`
when it claimed, so the behavioural obligation was discharged at the request
point, the close was accepted, and the terminal boundary then found the debt
and recorded `completion_contract_open R1 … no proof standard for it`,
ending `CompletedUnverified`. Same fact, two opposite responses, decided by
whether a verification cycle had happened to run first.

## 4. The change

**Reopen the request point when the tree moves.** `record_mutation` and
`note_mutation_op` clear `runtime_evidence_complete`: the plan's observations
describe the tree it observed, and a mutation makes them no longer describe
the current one. This restores the two-moment distinction the flag exists for.
The engine still sets it before the terminal boundary asks, so the commit
floor is untouched.

**Do not make impossible demands.** `OpenReason::dischargeable_by_more_work()`
is false for `MissingAuthoritativeProof` and true for every other reason. The
gate refuses on a judge that withholds completion, on an omitted requirement,
and on any open obligation the agent can act on — not on one the runtime
itself says has no standard.

`completion_debt()` is unchanged. An obligation with no standard is still
debt, is still reported, and still forces `CompletedUnverified` at the
terminal boundary. Withholding the claim and demanding more work are now two
different things.

## 5. Tests

| | |
| --- | --- |
| the flag reopens on a mutation, on a re-edit, and not on a check | `a_mutation_after_the_verification_plan_reopens_the_request_point`, `a_re_edit_also_reopens_the_request_point`, `recording_a_check_does_not_reopen_the_request_point` |
| an obligation with no standard is `MissingAuthoritativeProof`, and another green check does not change it | `an_obligation_with_no_proof_standard_cannot_be_discharged_by_more_work` |
| …and is still debt | `an_impossible_obligation_still_withholds_the_verified_claim` |
| every other reason is actionable | `every_other_open_reason_is_actionable` |
| the gate does not refuse on it, alone; refuses when mixed with an actionable one; still refuses a withholding judge and an omitted requirement | `an_obligation_with_no_proof_standard_does_not_refuse_the_claim`, `an_actionable_obligation_still_refuses_the_claim`, `a_refusing_judge_or_an_omitted_requirement_still_refuses` |
| **end to end: accepted, and still not verified** | `a_behaviour_with_no_proof_standard_is_accepted_but_never_verified` |

```
FMT=PASS
CLIPPY=PASS
FULL_PRODUCT_TEST_GATE=3754 passed, 135 result lines, 1 failure
  the failure is the pre-existing stale `client_command.schema.json` from
  f5a63b2; unrelated to this change and untouched.
```

## 6. Field replay (exp10) — non-regression, not a demonstration

C2, sealed start `84188ebb`, three control (`c04407b01e28`) and three
treatment, in parallel.

| run | arm | requests | refusals | ending | GOOD |
| --- | --- | ---: | ---: | --- | --- |
| c1 | control | 76 | 1 | Completed:73 | PASS |
| c2 | control | 62 | 2 | CloseoutForced:59 | PASS |
| c3 | control | 107 | 2 | 100 · CloseoutForced:4, "completeness could not be established" | PASS |
| f1 | treatment | 51 | 0 | Completed:49 | PASS |
| f2 | treatment | 100 | 0 | Completed:98 | PASS |
| f3 | treatment | 57 | 0 | Completed:55 | PASS |

Control 245 requests and 5 refusals; treatment 208 and 0. Six of six pass the
oracle, none reaches `Verified`, every ledger still carries its behavioural
debt with `runtime_evidence_complete = true` at the boundary.

**The refusal count is not evidence for this change.** Read by reason, none
of control's five refusals is the class this fixes: one
`no check demonstrates it over the current tree`, four `not satisfied` — all
of them things the agent can act on, and all still refused after the change.
The failing condition (a second window opened after a verification cycle, over
an obligation the judge reads as satisfied) did not occur in any of the six.

So exp10 says the change costs nothing and breaks nothing. What it does is
proven by the tests in §5 and by the post-closure C2 trace in §1, where two of
the three refusals were exactly the impossible demand.

## 7. What stays open

```
CONTRACT_DERIVATION_GIVES_BEHAVIOUR_NO_STANDARD=OPEN
```

§2's binding defect is not fixed here. A bug report whose expected outcome is
plainly testable — C2 states the value that must be submitted — still yields
an obligation with no proof standard, so the honest ending is
`CompletedUnverified` even when a regression test for exactly that case ran
green. Closing it means letting the derivation attach a proof standard to a
behavioural obligation whose wording names a checkable outcome, which is a
change to what the contract can claim and needs its own work package and its
own false-Verified evidence. Recorded, not attempted.

```
MODEL_REQUEST_TURN_AMPLIFICATION_CLOSURE=NOT_YET_PASS
NEW_FORMAL_TREATMENT_FROZEN=NO
FAST_GOOD_CHEAP_CLOSURE=NOT_PASSED
READY_FOR_BETA_READINESS_GATE=NO
```
