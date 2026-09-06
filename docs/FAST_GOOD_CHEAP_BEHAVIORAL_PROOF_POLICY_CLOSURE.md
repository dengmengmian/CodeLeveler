# Behavioral Obligation Proof Policy Closure

The work package that went looking for a binding defect and found the
architecture already correct. **No product code changed.** What it produced is
a verdict with source evidence, a measurement, and seven tests that pin the
boundary at the point where it actually decides — none of which existed.

```
BEHAVIOR_PROOF_BINDING_ROOT_CAUSE=NO_DEFECT — the task supplies no binding,
                                   and the runtime correctly refuses to invent one

GENERIC_BEHAVIOR_PROOF_POLICY=NONE (unchanged, now pinned)
MECHANICALLY_TESTABLE_BEHAVIOR_POLICY=SUPPORTED — through a user-grounded standard
AGENT_CREATED_TEST_AUTHORITY=SAFE
UNRELATED_TEST_DISCHARGE=BLOCKED

FALSE_VERIFIED_REGRESSION=0
```

---

## 1. Why verification and constraint carry a standard and behaviour does not

`policy_from` (`leveler-agent/src/completion_contract.rs`) has two gates, both
explicit:

```rust
if kind == RequirementKind::Constraint {
    if !raw.proof.trim().eq_ignore_ascii_case("mutation_scope") { return None; }
    …                       // and only when the task named the paths
}
if kind != RequirementKind::Verification { return None; }
```

The derivation prompt matches: it asks *"For a `verification` obligation, say
how it can be proven"* (`command_success` when the task names commands,
`test_coverage` when it asks for coverage, otherwise omit), and asks a
`constraint` for `mutation_scope` *"ONLY when the task names the part of the
tree the work may touch"*. A `behavior` entry is never asked.

**One rule explains all three: authority comes from the user's own wording.**
`command_success` exists because the user named the command. `mutation_scope`
exists because the user named the paths. A behaviour sentence names nothing —
relating it to a check is a question of meaning, and `discharged`'s own comment
says the runtime will not answer it:

> no rule over exit codes and mutation watermarks can tell those apart, and the
> runtime does not try — deciding whether one sentence means another is not a
> fact it holds.

So `behavior` gets no standard **not because the kind is second-class**, but
because nothing in the task grounded one.

## 2. The architecture already supports a grounded behavioural standard

`a_behavioural_obligation_with_a_named_command_closes_at_commit` has been in
the tree since F7: a `Behavior` requirement carrying `CommandSuccess`, whose
named command ran green over the current tree, discharges at the commit point
exactly as a verification obligation would.

And the derivation's grouping rule routes the grounded case correctly without
touching `policy_from`'s kind gate: a "must pass" condition inside an objective
becomes an **acceptance facet** with `kind: verification`, and facets get their
own `policy_from(&c, kind)` call. A user who writes *"the field must submit a
defined value, and `pnpm test` must pass"* gets a behaviour objective with a
provable facet. Nothing is lost.

`MECHANICALLY_TESTABLE_BEHAVIOR_POLICY=SUPPORTED`. The gap was never in the
consumption or the binding path.

## 3. The C2 question, answered

C2 is `react-hook-form` issue 13674, read from `goals/C2.goal.md`:

| does the task … | |
| --- | --- |
| name a command that must pass | **no** |
| ask for the behaviour to be covered by a test | **no** |
| name a file scope | **no** |
| state a precise, deterministic, observable outcome | **yes** — `address.street` must be `null` or `''`, never `undefined` |

So the acceptance criterion is *expressible* but not *grounded*. To give C2 a
proof policy the runtime would have to either invent the acceptance test
itself, or accept the test the agent wrote for its own work. The second is the
risk §12 of the work package names in one line — the agent authorizing its own
proof — and the first is a different product.

```
C2_BEHAVIOR_PROOF_POLICY=NONE, correctly
C2_NATIVE_TERMINAL_STATE=CompletedUnverified is the right product outcome
```

Raising Authority Yield by forcing a standard here is the exact move F7 exists
to prevent, and this package does not make it.

## 4. Authority yield, measured

Every completion contract persisted by every run of this programme — the
formal cohort, the post-closure cohort, and exp2 through exp10:

```
BEHAVIOR_OBLIGATIONS_TOTAL        90
DELIVERABLE_OBLIGATIONS_TOTAL      2
WITH_TYPED_PROOF_POLICY            0
PROOF_DISCHARGED                   0
WITHOUT_AUTHORITY                 92
```

Not one `verification` or `constraint` obligation was ever derived, and not one
`Unresolved` policy was ever produced. That is not a derivation failure: C1, C2
and C3 are all GitHub bug reports, and none of them names a check. **For this
task class the authority yield is structurally zero, and correctly so.**

It also means the corpus has never exercised the grounded path in the field.
The evidence that it works is the unit test in §2.

## 5. One correction this package makes to its own hypothesis

The audit proposed reclassifying `EvidencePolicy::Unresolved` as
`MissingAuthoritativeProof` — "the standard could not be determined" reads like
"there is no standard", and both are unanswerable by running anything.

Implementing it broke two existing tests, and the tests were right. A
**verification** obligation is by definition an obligation to demonstrate
something: even with no standard written down, the agent has real moves — write
the check, run it, or report `blocked` naming what cannot be done. Making it
non-actionable would let *"I added a test"* close an obligation with no test
behind it, which is precisely `scale-s800`'s original failure.

The distinction §22 asks for already exists and is already correct:

| | reason | actionable |
| --- | --- | --- |
| standard exists, receipt missing | `MissingMechanicalEvidence` | yes |
| verification with no standard | `MissingMechanicalEvidence` | yes — write it, or report blocked |
| behaviour with no standard | `MissingAuthoritativeProof` | **no** |

The change was reverted. `an_unproven_verification_stays_actionable_but_an_unprovable_behaviour_does_not`
pins the line so the next attempt does not have to rediscover it by breaking it.

## 6. Tests added (product code unchanged)

| | | |
| --- | --- | --- |
| B1 | "make the architecture cleaner", "improve maintainability", "use an idiomatic implementation" — green suite, judge satisfied, commit point → still `MissingAuthoritativeProof` | `generic_behaviour_is_never_authoritatively_discharged` |
| B3 | a real, runtime-issued, fresh, green check **about something else**, whose citation resolves → does not discharge | `an_unrelated_green_check_does_not_discharge_a_behaviour` |
| B4 | the agent writes its own test and runs it green, cites it → does not discharge | `an_agent_written_test_does_not_authorize_its_own_discharge` |
| B10 | wrong implementation behind a green suite → still debt, so the terminal boundary still refuses verified | `a_wrong_implementation_behind_a_green_suite_is_still_debt` |
| positive | the user's own words name the check → the behaviour obligation carries a standard and the record settles it | `a_user_named_check_grounds_a_behaviour_obligation` |
| §22 | unproven verification stays actionable; unprovable behaviour does not | `an_unproven_verification_stays_actionable_but_an_unprovable_behaviour_does_not` |

The five negative cases were all already correct — every one passed on first
run against unmodified product code. They are pinned now because they were the
things this package was asked to guarantee, and nothing was asserting them **at
the commit point** (the existing behaviour tests run at the request point,
where the floor deliberately does not apply).

## 7. No engineering replay, and why

The work package asks for a C2 replay with three treatment reps. There is
nothing to replay: the binary this package produces is behaviourally identical
to `9bdd22d`. A replay could only measure model variance, at the cost of an
hour and a round of API spend, and would invite reading that variance as an
effect. The false-Verified evidence stands on `a_wrong_implementation_behind_a_green_suite_is_still_debt`
and on the field record already in hand — 21 runs across the post-closure
cohort, exp10 and the non-parity arm, not one reaching `Verified`.

```
REQUEST_COMMIT_PHASE_SEMANTICS_STILL_PASS=YES
ACCOUNTING_AUTHORITY_CLOSURE_STILL_PASS=YES
REVIEWER_CLOSURE_STILL_PASS=YES

FMT=PASS
CLIPPY=PASS
FULL_PRODUCT_TEST_GATE=3760 passed, 158 result lines, 1 failure
  the pre-existing stale `client_command.schema.json` from f5a63b2, unrelated
  and untouched. 3754 -> 3760 is this package's six new tests.
```

## 8. What this licenses, and what it does not

```
BEHAVIORAL_OBLIGATION_PROOF_POLICY_CLOSURE=PASS
  — as "the boundary is correct and now pinned", not as "yield was raised"

COMPLETION_BEHAVIOR_PROOF_BINDING=CLOSED_AS_NOT_A_DEFECT
```

The open item from the previous package is answered rather than fixed: a
behaviour obligation has no proof standard because the task gave it none. If a
future package wants authority for a bug report like C2, the only honest routes
are a user-supplied acceptance check or an independently-authored regression
test — not a runtime that decides for itself which green check proves which
sentence.

```
C3_CONVERGENCE_GAP=OPEN
MODEL_REQUEST_TURN_AMPLIFICATION_CLOSURE=NOT_YET_PASS
NEW_FORMAL_TREATMENT_FROZEN=NO
FAST_GOOD_CHEAP_CLOSURE=NOT_PASSED
READY_FOR_BETA_READINESS_GATE=NO

NEXT_ENGINEERING_WORK_PACKAGE=CONVERGENCE_PROGRESS_AUTHORITY_CLOSURE
```
