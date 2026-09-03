Current phase:
    Beta Capability Closure

Current step:
    F7-B Behavioral Witness / Semantic Adequacy Closure

Completed:
    F1 · F2A · F3 · F4 · F5 · F6 · F6 Independent Revalidation ·
    HC-002 fresh ×3 · scale-s800 fresh ×10 · Scale Human Review Checkpoint ·
    F7-A Wrong Evidence Kind (ENGINEERING_CLOSED) ·
    F7-B characterization · F7-B architecture · F7-B red matrix ·
    F7-B implementation · F7-B real-path green · F7-A yield repair

Next:
    New Treatment Dogfood Acceptance | BLOCKED

Beta impact:
    OPEN_BETA_BLOCKER=1
    OPEN_BETA_REQUIRED=1
    PHASE_B=HOLD

---

# F7-B — Behavioral Witness / Semantic Adequacy: engineering closure

Commit `a8f9af5`. Companion: `F7B_BEHAVIORAL_WITNESS_ARCHITECTURE.md`.

**F7-B did not close either gap it was given.** One is not mechanically
decidable with the facts the runtime holds. The rule for the other was
written, tested, and withdrawn because it cannot live where the completion
predicate lives without making completion unreachable on the mainline path.
What shipped is a real tightening of citations and the repair of a yield
regression F7-A introduced.

```
F7B_ENGINEERING_GATE=REVIEW_REQUIRED
```

## 1. Why F7-A was insufficient

F7-A stopped a mutation being cited as a behavioural witness. Two shapes
remained.

**`refs=[]`.** An obligation the judge marks satisfied and anchors to no id
discharges. 26 of the 32 behavioural obligations in the preserved successful
cohort are uncited, so this is the normal case, not an edge.

It is worth being precise about what "uncited" means. The judge is not shown
nothing: `ReconcileInput` carries the last 3 tool results tail-bounded to
1,500 chars each, the executor's recent prose, the modified path list, a
freshness boolean, the runtime-issued candidate ids and the frozen contract.

```
ZERO_REF_SEMANTIC_INPUT_SOURCES = bounded recent tool output
                                + executor claims
                                + modified path list
                                + a freshness boolean
```

So an uncited discharge is the judge reading a transcript tail and declining
to anchor. Weak, but not baseless — which is why "no ref, no discharge" is the
wrong shape.

**A real command that proves a neighbouring behaviour.** Evidence that is
recorded, fresh, successful, runtime-issued and correctly kinded can still
exercise category B while the obligation is about category A. Nothing in the
runtime's vocabulary distinguishes them.

## 2. What the runtime can and cannot know

It knows: tool_call_id, normalized program+args, exit code, the mutation
sequence a check ran at, which paths a call changed, and whether a check
predates all work.

It does not know: stdout, stderr, what input a program was given, test source,
diff content.

`baseline_green_verifications()` already existed and already encoded the
useful idea — *a check that passed before anything changed says what the code
already did, not what the work achieved.* It was used once, at the terminal
boundary. F7-B moves that idea into the citation rule, where it decides.

## 3. A yield regression F7-A shipped, found and fixed here

F7-A's closure report claimed `32 of 32` behavioural obligations remained
dischargeable. **The measurement was wrong.** It classified by "does the
citation name a verification"; the shipped predicate also required freshness
against the *last* mutation.

| Run | cited & kept | **cited & blocked by F7-A** | uncited |
| --- | ---: | ---: | ---: |
| hc002 run-01 | 7 | 0 | 0 |
| hc002 run-02 | 0 | **4** | 15 |
| hc002 run-03 | 0 | 0 | 11 |

run-02's three checks all ran green after its substantive edits. Freshness was
then destroyed by a later write to `.sanity/navsvc`, a harness artifact. Four
obligations of an externally accepted run were blocked by a file touch
unrelated to any of them. F7-B repairs it.

## 4. What shipped

**One rule.** `witnesses_behavior` tests "after ANY mutation" instead of "at
or after the LAST one". A baseline-green check is still rejected — that is the
discrimination that matters. A check invalidated only by a later unrelated
write is no longer discarded.

```
BEFORE   cited id must be a command, exit 0, fresh against the newest edit
AFTER    cited id must be a command, exit 0, observed after work began
```

F7-A's kind rule is untouched: a mutation is never a behavioural witness.

**One rule withdrawn, and why it cannot exist here.** "An uncited claim needs
the run to have observed the changed tree" was implemented and passed its own
tests. Then thirteen engine tests span their loops until their scripts ran
out.

`update_goal(complete)` consults `discharged()` from *inside the agent loop*.
On the Direct path the runtime's verification plan runs afterwards, in
`conclude_direct`, and its results never enter the `EvidenceLedger` — only
verification-class commands the agent itself ran do. So the rule made
completion unreachable for every task that verifies the way the product is
designed to verify. Not a modest yield cost: the mainline shape.

Putting the rule at the terminal boundary instead, where the plan has run,
means a second completion predicate — the thing this architecture refuses, and
the thing that made this class of defect possible.

Two tests are named for the gap rather than deleted, one at the predicate and
one on the real completion path, so it stays visible.

## 5. The §19 red-team question, answered

*Requirement A; evidence B is real, fresh, successful, runtime-issued,
correctly kinded — but B exercises a neighbouring behaviour, and the judge
says B proves A. Why does the new architecture reject it?*

**It does not.** It rejects citations that cannot discriminate — baseline-green,
wrong kind, invented, failing — and that is all. Deciding that "a check
exercising category B does not establish category A" is semantic adequacy of
natural language; §6 rightly forbids faking it with string comparison, and no
mechanical fact the runtime holds separates the two.

Closing it needs a witness contract frozen at derivation that a correct run
can satisfy and an incorrect one cannot — most plausibly red-before /
green-after, mechanically checkable on both halves. No obligation in the
preserved cohort has that shape, so adopting it now blocks 32 of 32. It needs
a cohort designed to produce discriminating witnesses.

## 6. The §20 question, answered

*How does a legitimate refs=[] success still close?* Unchanged — the uncited
path is ungated. And the four run-02 obligations F7-A had blocked now close
again.

## 7. Closure yield

Measured on the preserved cohort, by the predicate as implemented:

```
REPRESENTATIVE_BEHAVIORAL_REQUIREMENTS = 32
PRE_F7B_DISCHARGEABLE                  = 28   (F7-A blocked 4)
POST_F7B_DISCHARGEABLE                 = 32
NEW_FALSE_BLOCK_COUNT                  = 0
```

F7-B raises yield rather than lowering it, because it repairs F7-A's
over-reach and adds no new block that any legitimate obligation trips.

## 8. Cost

No prompt bytes added, no candidate fields added, no new model call, no new
durable state. The context-cost work of the previous batch is untouched.

## 9. Tests

| | Command | Result |
| --- | --- | --- |
| fmt | `cargo fmt --all -- --check` | exit 0 |
| lifecycle | `cargo test -p leveler-lifecycle` | 156 passed, 0 failed |
| agent | `cargo test -p leveler-agent` | 562 passed, 0 failed, 1 ignored |
| engine | `cargo test -p leveler-engine` | 231 passed, 0 failed |
| clippy | `CARGO_TARGET_DIR=target/clippy cargo clippy --workspace --all-targets` | clean |
| workspace | `cargo test --workspace --no-fail-fast` | **135 suites, 0 failed** |

| Control | Result |
| --- | --- |
| baseline-green citation rejected | PASS |
| later unrelated write does not destroy a witness (the F7-A repair) | PASS |
| uncited claim over unobserved work — **still completes** | pinned, gap open |
| uncited claim after the work was observed closes | PASS |
| true positive: cited recorded check discharges | PASS |
| unknown ref fails closed (F3) | PASS |
| failed check is not a witness | PASS |
| wrong kind — mutation only (F7-A) | PASS |
| facet held to the same standard | PASS |
| durability across a JSON round trip | PASS |
| CommandSuccess / TestCoverage / MutationScope unchanged | PASS |

## 10. Machine-readable final state

```
F7B_ARCHITECTURE_GATE=REVIEW_REQUIRED
F7B_ZERO_REF_FALSE_DISCHARGE_RED_TO_GREEN=FAIL
F7B_SEMANTIC_SUBSTITUTION_RED_TO_GREEN=FAIL
F7B_TRUE_POSITIVE_GROUNDED_WITNESS=PASS
F7B_ZERO_REF_SUCCESS_COMPATIBILITY=PASS
F7B_REAL_FALSE_SATISFIED_BLOCKED=FAIL
F7B_REAL_TRUE_SATISFIED_CLOSES=PASS
F7B_TERMINAL_TRUTH_GATE=PASS
F7B_DURABILITY_GATE=PASS
F1_REGRESSION_GATE=PASS
F2A_REGRESSION_GATE=PASS
F3_REGRESSION_GATE=PASS
F5_REGRESSION_GATE=PASS
F6_REGRESSION_GATE=PASS
F7A_REGRESSION_GATE=PASS
CONTEXT_COST_REGRESSION_GATE=PASS
FULL_WORKSPACE_GREEN=YES
SECOND_RUNTIME=NO
SECOND_EVENTLOG=NO
SECOND_EVIDENCE_LEDGER=NO
SECOND_COMPLETION_PREDICATE=NO
SECOND_JUDGE=NO
SECOND_BEHAVIOR_LEDGER=NO
SECOND_FACT_STORE=NO
JUDGE_PROSE_AUTHORITY=NO
REQUIREMENT_PROSE_HEURISTIC_ADDED=NO
BENCHMARK_SPECIFIC_SPECIAL_CASE=NO
PRIMARY_F7B_FIX_PROMPT_ONLY=NO
F1_SAFETY_BOUNDARY_WEAKENED=NO
NEW_MODEL_CALL_ADDED=NO
F7B_ENGINEERING_GATE=REVIEW_REQUIRED
F7_STATUS=OPEN
F7B_FINAL_HEAD=5533178d2f74d4fe45f5569cf0f015e8dc9b2a7f
F7B_FINAL_TREE=abc709b0205c355fb5e9c17a1cb00ee74942ecea
WORKTREE_CLEAN=YES
OPEN_BETA_BLOCKER=1
OPEN_BETA_REQUIRED=1
PHASE_B=HOLD
```

The three red-to-green gates read FAIL because the shapes they name still
discharge. Reporting them PASS on the strength of the tightening that did ship
would be the same substitution this whole workstream exists to stop.

## 11. What review has to decide

1. Accept that uncited semantic discharge stays open until a witness class
   exists, or fund the red-before/green-after witness contract and the cohort
   that can produce it.
2. Whether the runtime's own verification plan should feed the
   `EvidenceLedger`. It is a real observation of the changed tree that the
   completion predicate cannot currently see, and its absence is what forced
   the withdrawal in §4.

## 12. Frozen, not revisited here

```
REASONING_PASSBACK_PROMOTION=REJECTED   (input +34 %, output +61 %, turns +16 %,
                                         first edit +5, cost +35 %, acceptance tie)
TRIMMING_TRIGGER=NOT_REACHED            (reclaimable 25.5 KB / 28.6 KB vs 64 KiB)
TRIMMING_THRESHOLD_CHANGE=NO
```

No dogfood was run. No tag, release, version bump, Phase B or three-way Eval.
