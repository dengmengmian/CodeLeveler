# Reviewer Efficiency / Policy Composition Closure

Work package 2 of the Fast / Good / Cheap engineering closure.

```
REVIEWER_EFFICIENCY_POLICY_COMPOSITION_CLOSURE=PASS

REVIEWER_REPORT_GATE_MISMATCH_FIXED=YES
REVIEWER_POLICY_COMPOSITION=PASS
REVIEWER_PARENT_DEADLINE_AWARE=YES
REVIEWER_PARTIAL_SETTLEMENT=PASS
ZERO_FINDING_WASTE_CONTROLLED=YES
REVIEWER_ACCOUNTING_ATTRIBUTION=PASS

REVIEWER_WALL_REDUCTION_CONFIRMED=YES   −66%
REVIEWER_TOKEN_REDUCTION_CONFIRMED=YES  −72% input
GOOD_NON_REGRESSION=PASS
```

A reviewer's whole job is to report what it found. It was being held to the
plan ceremony written for agents that *do* things, and spent its budget
discovering it needed a plan before it could say what it already knew.

**Same task, same starting tree, both arms in parallel: the reviewer's model
calls fall 23 → 7, its input tokens 294,184 → 81,214, its wall time 434s → 148s,
and it still comes back `COMPLETED_WITH_FINDINGS` in both.**

---

## 1. What Phase C proved

```
REVIEWER_STARTS=2   REVIEWER_BUDGET_EXHAUSTED=2   REVIEWER_ACCEPTED_FINDINGS=0
REVIEWER_INPUT_TOKENS≈993K
one run: reviewer occupied ≈1158s, 62% of the whole run's wall clock
another: finished at 3485s against a 3600s timeout — 115s of margin
```

Both spawns ended `INCOMPLETE_PARTIAL (stopped: its token or cost budget ran
out)` having delivered nothing.

## 2. Four candidate root causes; two confirmed, two not

Recording the two that were **not** confirmed matters as much as the two that
were — they were on the work package's list, and fixing a mechanism that was
already correct would have been churn dressed as progress.

### `REVIEWER_PLAN_GATE_ROOT_CAUSE = CONFIRMED`

`drive.rs` exempted exactly four tools from the plan gate:

```rust
"update_plan" | "request_user_input" | "ask_user" | REQUEST_PERMISSIONS_TOOL
```

`report_finding` is not among them and is not an explore tool, so a reviewer
whose task looked multi-step was refused on its own terminal call, told to
`update_plan` first, and spent rounds on ceremony.

The system prompt makes the contradiction explicit — it already tells the
Explorer *"Call report_finding the moment you confirm each concrete discovery —
findings reported early survive even if your run is cut short"* — while the gate
refused exactly that.

### `REVIEWER_DEADLINE_ROOT_CAUSE = CONFIRMED (partially)`

Residual propagation already existed:

```rust
residual_limits.max_duration = Some(sub_cap.min(residual));
```

What was missing is a **settlement reserve**. The child was handed the parent's
*entire* remainder, leaving nothing for the parent to fold the result in,
reconcile, and write the outcome. That is the 3485-of-3600 run.

### `REVIEWER_BUDGET_EXHAUSTION_SETTLEMENT_ROOT_CAUSE = NOT_CONFIRMED`

This mechanism is already correct. `handlers.rs` captures typed findings from
every ledger snapshot and returns them on all three exit paths; the code even
says so — *"INCOMPLETE_PARTIAL keeps its findings"*. Phase C's reviewer returned
zero findings because none was ever **accepted**, not because accepted ones were
dropped. No change made.

### `REVIEWER_ZERO_FINDING_TAIL_ROOT_CAUSE = NOT_CONFIRMED_AS_A_SEPARATE_DEFECT`

The long tail was the plan gate, not an independent spin-until-exhausted bug.
Adding a "no findings for N rounds, kill it" heuristic would have been a guess,
and the work package forbids one without evidence. No change made; the tail is
controlled by removing what caused it.

## 3. The fix, and the hook it hangs on

The interesting part is **what to key the exemption on**.

The first attempt read `output_contract.findings` off the child profile. That
turned out to be a global bypass in disguise: **all four profiles declare
`findings: true`**, so keying on it would have exempted the Worker and the
Default agent too — precisely the thing this must not be.

The hook that actually discriminates comes from the gate's own stated purpose.
It already lets every read-only navigation tool through, because *"plan is an
execution aid, not a license to navigate code"*. A read-only child is nothing
but navigation and a report: it holds no mutating tool, so there is no work for
a plan to sequence.

```rust
pub(crate) fn role_reports_findings(role: AgentRole, tool: &str) -> bool {
    let profile = ChildProfile::resolve(role);
    tool == REPORT_FINDING_TOOL && profile.read_only() && profile.output_contract.findings
}
```

| role | read-only | declares findings | exempt |
| --- | --- | --- | --- |
| Reviewer | yes | yes | **yes** |
| Explorer | yes | yes | **yes** |
| Worker | no | yes | no |
| Default | no | yes | no |

Read off the profile, not a role list, so the two cannot drift apart. No prompt
was changed: teaching a reviewer to satisfy the gate would have been teaching it
to work around a runtime defect.

### Settlement reserve

```rust
pub(crate) const CHILD_SETTLEMENT_RESERVE: Duration = Duration::from_secs(60);
...
let for_child = residual.saturating_sub(CHILD_SETTLEMENT_RESERVE);
residual_limits.max_duration = Some(sub_cap.min(for_child));
```

A child is a tail, not a claim on the deadline. Near the deadline the grant
shrinks with the remainder and reaches zero rather than eating the parent's
ability to finish; with plenty of time the profile cap still binds, so the
reserve is a floor on the parent's side, not a new ceiling on the child's.

## 4. A/B evidence

Control is the WP1 binary, treatment is this change. Same task, same initial
tree (`1196df21`), run in parallel: an LRU cache whose `put()` leaves a
duplicate in the recency list when it updates an existing key, so a live key is
evicted early. Both arms were told to spawn a reviewer.

| Metric | Before | After | Delta |
| --- | ---: | ---: | ---: |
| Plan-gate refusals of `report_finding` | 11 | **0** | −11 |
| Reviewer model requests | 23 | 7 | **−70%** |
| Reviewer input tokens | 294,184 | 81,214 | **−72%** |
| Reviewer cached input | 272,896 | 66,432 | — |
| Reviewer output tokens | 49,055 | 16,645 | −66% |
| Reviewer cost (µUSD) | 20,376 | 7,600 | **−63%** |
| Reviewer model wall | 433.8s | 148.3s | **−66%** |
| `report_finding` tool calls | 40 | 14 | −65% |
| Findings reported | 20 | 7 | see below |
| Reviewer termination | `COMPLETED_WITH_FINDINGS` | `COMPLETED_WITH_FINDINGS` | unchanged |
| Whole run: rounds | 43 | 37 | −14% |
| Whole run: requests | 68 | 46 | −32% |
| Whole run: input tokens | 1,358,454 | 919,983 | −32% |
| Whole run: cost (µUSD) | 52,239 | 30,446 | −42% |

The finding count drops with the retries, not with the coverage: control's 40
`report_finding` calls produced 20 reports because most were refused and
re-attempted. Treatment's 14 calls produced 7 reports on first ask. Both arms
terminate `COMPLETED_WITH_FINDINGS`.

Note the control arm did **not** reproduce Phase C's total collapse — it still
got findings through after 11 refusals. Phase C's reviewer had a harder task and
ran out first. The defect is the same; its cost varies with how much budget the
ceremony leaves.

### GOOD non-regression

```
control    unittest OK   independent probe PROBE_PASS
treatment  unittest OK   independent probe PROBE_PASS
```

The probe is written here, not by either agent: build an `LRUCache(2)`, insert
two keys, update the first, insert a third, and assert the *true* LRU was the
one evicted. Both arms fix the bug, add a test, and pass. Terminal state
`CompletedUnverified` in both, as before. No new false Verified.

## 5. Tests

| | | |
| --- | --- | --- |
| R1 | a reviewer may report a finding with no plan | `a_reviewer_may_report_a_finding_without_a_plan` |
| R2/R3 | not a global bypass — Worker and Default unaffected, and no other tool exempted for anyone | `the_exemption_is_the_roles_output_contract_not_the_tool_name` |
| — | exemption ≡ read-only ∧ declares findings, for every role | `every_role_that_declares_findings_is_exempt_and_only_those` |
| R4 | plenty of time → profile cap still binds | `a_parent_with_plenty_of_time_still_caps_the_child_at_its_profile` |
| R5 | near the deadline → the grant shrinks to zero | `a_parent_near_its_deadline_grants_a_child_nothing` |
| R6/R7 | a child never receives the parent's whole remainder | `a_child_never_receives_the_parents_whole_remainder` |
| R8/R9/R10 | partial settlement | pre-existing and verified unchanged — see §2 |
| R21 | reviewer usage is attributable | WP1's `a_child_request_is_attributed_to_its_agent`, and the A/B table above is read straight from `model_requests` |

```
FMT=PASS
CLIPPY=PASS   (workspace, all-targets, all-features, -D warnings)
FULL_PRODUCT_TEST_GATE=PASS   3712 tests, 135 result lines, 0 failures
ACCOUNTING_AUTHORITY_CLOSURE_STILL_PASS=YES
```

Every number in the A/B table came from `model_requests` rows, split by
`agent_id`. Before WP1 that table would have been empty of child rows and this
comparison would have had to be assembled by hand from stdout.

## 6. Limitations

**The parent's own wall cap is often unset.** The settlement reserve only binds
when `parent_wall.cap` is `Some`. In Phase C the 3600s limit was enforced by the
harness as a subprocess timeout, not by the runtime's own budget, so the child
saw no parent deadline to shrink against and took its full 20-minute profile
cap. The reserve fixes the case where a cap exists; it cannot invent one.

**The 60-second reserve is a chosen constant.** It is sized for folding a result,
a reconciliation judge call, and writing the outcome. It is not derived from
measured settlement durations, because settlement timings are not recorded —
the same observability gap WP1 noted as `TIME_TO_CORRECT_RESULT`.

**One A/B pair, one task.** The direction is unambiguous and the mechanism is
proven by the refusal count going 11 → 0, but the magnitudes are one
observation.

**Advisory accounting difference is untouched.** Still
`advisory_lane_absent_from_runtime_cumulative_counters`, as WP1 recorded.

## 7. What this licenses

```
REVIEWER_COST_REDUCTION_CONFIRMED=YES
REVIEWER_TAIL_REDUCTION_CONFIRMED=YES

CODELEVELER_IS_CHEAPER_THAN_ATOMCODE=NOT_CLAIMED
FAST_GOOD_CHEAP_CLOSURE=NOT_PASSED
```

The main agent's round amplification is untouched, and it is the larger number:
the whole run still took 37 rounds where a competitor took 12–47 requests for
the same class of task. That is WP3.

```
NEW_FORMAL_TREATMENT_FROZEN=NO
NEXT_ENGINEERING_WORK_PACKAGE=MODEL_REQUEST_TURN_AMPLIFICATION_CLOSURE
```
