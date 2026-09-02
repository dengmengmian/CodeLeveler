Current phase:
    Beta Capability Closure

Current step:
    F7 Behavioral Evidence Grounding — Engineering Closure

Completed:
    F1 Evidence Binding · F2A Absolute Reconciliation Deadline ·
    F3 Runtime-owned Evidence Identity · F4 180s Gate Viability ·
    F5 Long-Thinking Transport · F6 Mechanical Fact Authority ·
    F6 Independent Revalidation · HC-002 fresh ×3 · scale-s800 fresh ×10 ·
    Scale Human Review Checkpoint · F7 characterization · F7 architecture ·
    F7 red → green · F7 controls · F7 regressions

Next:
    F7 Dogfood Targeted Revalidation | READY

Beta impact:
    OPEN_BETA_BLOCKER=1
    OPEN_BETA_REQUIRED=1
    PHASE_B=HOLD

---

# F7 — Behavioral Evidence Grounding: engineering closure

Commit `af36157470fbf42daf2b8a520f0cc600b923689e`, tree
`add21522ab2f999aead9a17164079c09342bf53c`. Not pushed (`main` is the
repository's protected default; pushing is a separate decision).

Companions: `F7_BEHAVIORAL_EVIDENCE_CHARACTERIZATION.md` (what run-02 did),
`F7_BEHAVIORAL_EVIDENCE_ARCHITECTURE.md` (why this design and not the others).

## 1. Root cause

icg-6r run-02 completed an unsatisfiable goal. Independently reconstructed
from the run's own artifacts: the implementation filtered `!record.Valid`
while the task asked for `Value == 0`, the frozen oracle disagreed
(`ASK_ORACLE_EXIT=1`), and the runtime reported Verified.

The contract was not the problem. All four obligations were derived with the
user's wording intact (`ORIGINAL_GOAL_PRESERVED=YES`, `OBLIGATION_LOSS=NO`),
and every mechanically decidable one — the mutation scope, the two commands —
was judged **correctly**.

The substitution entered at evidence binding. R1, R1.F1 and R1.F2 are
`kind: behavior` with `evidence_policy: null`, and each was discharged citing
one id: `call_00_miak…`, the `apply_patch` call that made the edit. R1.F1's
detail describes *"the recorded run … printed only `good count=1 total=7`"* —
**no such run exists in the ledger**. The two recorded commands are
`go build ./...` and `go test ./...`. The judge described an observation that
never happened and named a file edit as its witness.

Two runtime facts made that acceptable:

- `resolve_refs` maps a cited candidate id to its `tool_call_id` and **drops
  the candidate's kind**. The runtime knew the id was a mutation; it discarded
  the distinction at the boundary where it decided.
- `discharged()` had no rule for behavioural kinds at all — `_ => true`. Once
  the judge said Satisfied, it was.

The canonical completion predicate itself was sound. It ran, found `open=0`,
and completed on an obligation that should not have been satisfied.

```
GATE_BYPASS=NO
GATE_WRONG_INPUT=YES
SINGLE_CANONICAL_COMPLETION_PATH=YES
```

## 2. Before / after

```
BEFORE
  requirement A (behaviour, no policy)
      ↓  judge cites the edit, calls it an observation of A
  refs resolve (the id exists) — kind discarded
      ↓  discharged(): behaviour has no rule
  Satisfied → completion_debt() == 0 → Verified

AFTER
  requirement A (behaviour, no policy)
      ↓  judge cites the edit, calls it an observation of A
  runtime asks: can this id witness behaviour?
      ↓  it is a mutation — a file changing is not the behaviour changing
  citation unsound → obligation open → completion_debt() > 0 → Verified=NO
```

The rule never engages with what the prose means. It is decided before any
question of whether "invalid" and "zero-value" are the same set.

## 3. Architecture

**What changed.** Two functions, 41 lines of product code.

`EvidenceLedger::witnesses_behavior(id)` — whether an id names an
**observation**: a command this ledger recorded running green over the tree as
it now stands. Stale and failing runs are not witnesses; neither is a
mutation.

`discharged()` gains a `RequirementKind::Behavior` arm: an obligation that
**cites** evidence needs at least one citation that witnesses behaviour. An
obligation that cites nothing is unchanged.

**Why it is minimal.** It restores a distinction the runtime already owned and
had thrown away. No new fact, no new store, no new call, no new model. It is
F3's rule carried one step: F3 says a cited id must exist, F7 says a cited id
must be able to mean what it is cited for. Runtime keeps Observation; the
judge keeps Interpretation, and may still argue that a recorded check
demonstrates the requirement — what it may no longer do is nominate an edit as
the observation.

**Schema, persistence, restart.** No schema change. `RequirementEvidence` and
`EvidenceLedger` keep their shapes, so old snapshots replay and a contract
recovered after restart is re-evaluated by the same predicate against the same
ledger. Pinned by test: an open obligation is still open after a JSON round
trip.

**O4-B.** Same missing layer, opposite side. O4-B is real behaviour that
cannot be bound to proof; F7 is proof asserted for behaviour that did not
happen. A behaviour-witness class that actually exists would serve both. Not
built here — see §5.

## 4. Safety

```
PRIMARY_F7_FIX_PROMPT_ONLY=NO
SECOND_JUDGE_ADDED=NO
SECOND_EVIDENCE_LEDGER=NO
SECOND_BEHAVIOR_FACT_STORE=NO
REQUIREMENT_PROSE_HEURISTIC_ADDED=NO
BENCHMARK_SPECIFIC_SPECIAL_CASE=NO
F1_SAFETY_BOUNDARY_WEAKENED=NO
```

The product diff was scanned for `zero-value`, `Valid`, `icg`, `Aggregate`,
`summary.go`, `ask-oracle`, `contains(`, regex and keyword-overlap patterns:
zero hits. The only matches anywhere are in test prose (a comment naming
run-02) and one assertion reading `preview.contains("R1")`.

## 5. Closure-yield risk — answered

**Did F7 make ordinary successful semantic requirements broadly
unverifiable? No.** Measured, not assumed: every behavioural obligation in the
preserved successful HC-002 cohort, classified by what its refs resolve to.

| Run | Behavioural obligations | Cite a verification | Cite only a mutation | Cite nothing |
| --- | ---: | ---: | ---: | ---: |
| hc002 run-01 | 7 | 7 | 0 | 0 |
| hc002 run-02 | 15 | 3 | 0 | 12 |
| hc002 run-03 | 10 | 0 | 0 | 10 |
| **icg-6r run-02** | **3** | **0** | **3** | **0** |

```
SEMANTIC_REQUIREMENTS_PREVIOUSLY_DISCHARGEABLE = 32 of 32
SEMANTIC_REQUIREMENTS_AFTER_F7_DISCHARGEABLE   = 32 of 32
```

Not one obligation in the successful cohort cites a mutation alone. The defect
is the only place in the preserved evidence that does.

**The limit this buys, stated plainly.** An obligation that cites **nothing**
still discharges on the judge's word — 22 of those 32 do. Had run-02's judge
omitted its refs, this rule would not have caught it. Requiring a witness
closes that too and blocks all 22, which is the §56 collapse, demonstrated
rather than feared. Closing it properly needs a behaviour-witness class that a
correct run can actually cite, which is also what O4-B wants and which should
be designed against real cohort yield, not inferred here.

**F7 as implemented closes mis-kinded citation, not uncited semantic
discharge.** That is pinned by a test named for it, so it is not mistaken for
an oversight later.

## 6. Tests

Commands and cargo's own exit status.

| | Command | Result |
| --- | --- | --- |
| fmt | `cargo fmt --all -- --check` | exit 0 |
| lifecycle | `cargo test -p leveler-lifecycle` | **152 passed, 0 failed**, exit 0 |
| agent | `cargo test -p leveler-agent` | **560 passed, 0 failed, 1 ignored** |
| engine | `cargo test -p leveler-engine` | **231 passed, 0 failed** |
| workspace | `cargo test --workspace --no-fail-fast` | **60 suites passed, 0 failed — STOPPED before completion** |
| clippy | `cargo clippy` | **NOT RUN — skipped at the owner's instruction** |

The workspace run reached 60 green suites with zero failures across ~1h50m and
was stopped at the owner's instruction while running `leveler-tui`; the
remaining crates (tui, web, media, browser) have no dependency on the changed
code. Clippy was skipped the same way. Both are reported as what they are.

```
FULL_WORKSPACE_GREEN=NO   (not run to completion; 60/60 observed suites green)
CLIPPY=SKIPPED
```

### F7 controls

| Control | Test | Result |
| --- | --- | --- |
| Red reproduction | 6 lifecycle tests in the defect's shape | reproduced pre-fix, green post-fix |
| Primary green | `a_behaviour_citing_only_a_mutation_is_not_discharged` | PASS |
| Judge label is not authority | `calling_a_mutation_an_observation_does_not_make_it_one` | PASS |
| Real completion path | `a_behaviour_obligation_citing_the_edit_cannot_complete` (agent, through `update_goal` → judge → contract gate → terminal) | PASS |
| True positive | `a_behaviour_citing_a_recorded_check_is_discharged` | PASS |
| Mixed citation | `a_citation_naming_both_keeps_its_witness` | PASS |
| Unknown ref | `an_invented_citation_is_not_a_witness` | PASS |
| Stale | `a_check_from_before_the_last_edit_is_not_a_witness` | PASS |
| Failed check | `a_failed_check_is_not_a_witness` | PASS |
| No evidence (the known limit) | `an_uncited_behaviour_claim_still_discharges_and_that_is_the_known_limit` | PASS |
| Facet parity | `a_facet_citing_only_a_mutation_is_not_discharged` | PASS |
| Durability | `a_mis_cited_obligation_is_still_open_after_a_round_trip` | PASS |

### Regressions

| Gate | Evidence |
| --- | --- |
| F1 | `CommandSuccess` matching, safe `&&`, pipe/redirect fail-closed — untouched code, covered by the lifecycle and agent suites |
| F2A | one absolute deadline, retry and repair budgets — untouched, agent suite green |
| F3 | runtime-issued identity, unknown ref fails closed, `TestCoverage` — lifecycle suite green, plus F7's own unknown-ref control |
| F5 | `LongThinkingNonStreaming` and the patient client — untouched, agent suite green |
| F6 | `MutationScope` short-circuits before the kind rules; behavioural wording is never converted into file scope — `mutation_scope_tests` green, plus `a_constraint_citing_a_mutation_is_untouched` |
| Terminal truth | no route bypasses contract debt — `SINGLE_CANONICAL_COMPLETION_PATH=YES`, engine suite green |

## 7. Machine-readable final state

```
F7_CHARACTERIZATION_GATE=PASS
F7_ARCHITECTURE_GATE=PASS
F7_RED_REPRODUCED=YES
F7_PRIMARY_RED_TO_GREEN=PASS
F7_REAL_COMPLETION_PATH_GREEN=PASS
F7_TRUE_POSITIVE_CONTROL=PASS
F7_NO_EVIDENCE_FAIL_CLOSED=PASS
F7_STALE_EVIDENCE_FAIL_CLOSED=PASS
F7_UNKNOWN_REF_FAIL_CLOSED=PASS
F7_TERMINAL_TRUTH_GATE=PASS
F7_DURABILITY_GATE=PASS
F1_REGRESSION_GATE=PASS
F2A_REGRESSION_GATE=PASS
F3_REGRESSION_GATE=PASS
F5_REGRESSION_GATE=PASS
F6_REGRESSION_GATE=PASS
TERMINAL_TRUTH_REGRESSION_GATE=PASS
FULL_WORKSPACE_GREEN=NO
SECOND_RUNTIME=NO
SECOND_EVENTLOG=NO
SECOND_EVIDENCE_LEDGER=NO
SECOND_COMPLETION_PREDICATE=NO
SECOND_JUDGE=NO
SECOND_BEHAVIOR_FACT_STORE=NO
JUDGE_PROSE_AUTHORITY=NO
REQUIREMENT_PROSE_HEURISTIC_ADDED=NO
BENCHMARK_SPECIFIC_SPECIAL_CASE=NO
F1_SAFETY_BOUNDARY_WEAKENED=NO
NEW_MODEL_CALL_ADDED=NO
F7_ENGINEERING_GATE=PASS
F7_STATUS=ENGINEERING_CLOSED
OPEN_BETA_BLOCKER=1
OPEN_BETA_REQUIRED=1
PHASE_B=HOLD
```

`FULL_WORKSPACE_GREEN=NO` records a run that was stopped, not a run that
failed. `F7_ENGINEERING_GATE=PASS` rests on the three crates the change
touches, all green in full, plus 60 green workspace suites before the stop.

## 8. Not done here, by instruction

No cohort was rerun (icg-6r, HC-002, scale-s800). No tag, publish, release,
version bump, Phase B or three-way Eval. The Beta blocker is **not** cleared:
engineering proof by the owner of the change is not dogfood acceptance.

One sequencing note for the next batch: the two context-cost A/Bs
(`prune_tool_results`, `keep_reasoning`) must be run **after** F7, because
their primary metric is task success and F7 changes what the completion gate
accepts. Recorded in `CONTEXT_COST_MEASUREMENT_PLAN.md` §3b.
