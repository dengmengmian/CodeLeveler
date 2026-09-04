# Formal Three-Way Eval — Phase B

```
PHASE_B_EXECUTION_GATE=PASS
PHASE_B_COHORT_VALID=YES
FORMAL_THREE_WAY_EVAL_PHASE_B=COMPLETE
READY_FOR_PHASE_C=YES

BETA_BASELINE_STATUS=FROZEN
OPEN_BETA_BLOCKER=0
OPEN_BETA_REQUIRED=0
PHASE_B=HOLD
```

24 runs, three frozen harnesses, one mechanical oracle, nothing changed
mid-cohort. AtomCode 8/8, CodeLeveler 6/8, DeepSeek Harness 6/8. CodeLeveler is
consistently the slowest, by 3–5× at the median.

The gate passes on execution integrity, not on placing first — §11.

---

## 1. Objects

```
CODELEVELER_PRODUCT_HEAD=7486c3377f19be564643a41c4158b4ee77c1d8b3
CODELEVELER_BUILD_IDENTITY=leveler 0.2.0-beta.1 (7486c3377f19)
CODELEVELER_SHA256=121983c94a6766492b428560b06a2363c7628073bc7683b570f12daad3d67919
CODELEVELER_OBJECT_VERIFIED=YES

ATOMCODE_VERSION=5.0.9   ATOMCODE_REVISION=52ca5e6
ATOMCODE_OBJECT_VERIFIED=YES

DSH_VERSION=0.1.2-alpha.1
DSH_REVISION=cd5ef8148158c3a752a658978873241fdf8e2bbc
DSH_OBJECT_VERIFIED=YES

ALL_THREE_OBJECTS_VERIFIED=YES
```

The CodeLeveler artifact's sha256 and embedded identity were re-checked before
every one of its 8 runs. `BASELINE_IDENTITY_VIOLATION` count: 0.

Environment: macOS 26.6.2, arm64, Apple M4 Max, 64 GB, one machine, sleep
prevented throughout. The runner strips proxy variables for all three arms
symmetrically.

## 2. Contract

```
PHASE_B_TASK_SET=n3-caller-propagation · icg-5-long-task · scale-s800 · yq-doc-count
REPS_PER_TOOL_PER_CASE=2
TOTAL_EXPECTED_RUNS=24

RUN_ORDER_POLICY=leveler·1 → atomcode·1 → dsh·1 → dsh·2 → atomcode·2 → leveler·2
                 (runner.py HC001_ORDER, per case)
SUCCESS_ORACLE=case.expect, mechanical, identical for all three
TIMEOUTS=1200s (n3) / 1800s (the rest), from the frozen manifest

MODEL_PARITY=FULL          all three on deepseek-v4-flash
GATEWAY_PARITY=PARTIAL     CodeLeveler & DSH via taotoken; AtomCode via AtomGit
TOKEN_RANKING=DISABLED
COST_COMPARABILITY=PARTIAL
```

`FORMAL_COHORT_CONTRACT_DRIFT=NO` — the lock, manifest, runner, binding layer
and case files hash identically before and after the cohort.

## 3. Governance history — kept, not tidied away

Two mistakes were made getting here, and both are recorded because the second
one changes what Phase B can claim.

**`comparison.lock.yaml` freezes five cases; the comparative manifest registers
four.** `icg-6r` is the missing one. It was first diagnosed as an unregistered
case and a registration was added at 1800s parity with the long-case class.

**That diagnosis was wrong.** `icg-6r` is a *baseline-green negative case*: its
`expect` is "tree unmodified, build green, test green", which passes on the
untouched tree by construction, because the correct behaviour is to leave it
that way. The comparative runner requires baseline-RED — it runs `expect`
first, and returns `UNJUDGEABLE_BASELINE_GREEN` without launching any tool if
it already passes. Confirmed empirically: all three harnesses returned that
status in 2–3 seconds.

So the case was excluded deliberately, not overlooked. This lane's oracle asks
"did red become green"; `icg-6r` asks "did the tool correctly refuse to break
something green". A harness that does nothing at all scores identically to one
that reasons correctly and stops.

```
ICG_6R_COMPARATIVE_LANE_JUDGEABLE=NO
ICG_6R_UNJUDGEABLE_FOR_ALL_THREE_TOOLS=YES   (excluded symmetrically)
```

**A verification that was not sufficient.** The registration was checked with
`load_cases()`, which filters by the manifest id — and passed. But
`runner.main()` re-keys its case dict by the *case YAML's own* id, and for
`icg-6r` those two strings differ. Every other case has them equal, so the
mismatch had never surfaced. The first formal cohort therefore ran 18 runs and
then hit `KeyError` on all six `icg-6r` runs, which never executed.

```
FORMAL_COHORT_ATTEMPT_1=INVALIDATED
```

The whole cohort was discarded and restarted, not patched and continued: fixing
the harness changes the contract, and 18 runs under the old one plus 6 under
the new one is not one cohort. Evidence retained at
`formal-attempt1-INVALIDATED/`.

**What this costs the conclusions.** `icg-6r` is the only case in the frozen
set designed to expose false completion, and it is exactly where CodeLeveler's
F7 work concentrates. Phase B therefore says very little about comparative
completion truthfulness — §8. That question was answered separately, in the
native eval lane which does judge negative cases: 10/10 honest block, false
Verified 0 (`F7_GROUNDED_AUTHORITY_DOGFOOD_ACCEPTANCE.md`).

## 4. Execution

```
TOTAL_EXPECTED_RUNS=24
TOTAL_COMPLETED_RUNS=24
VALID_RUNS=24
INVALID_INFRA_RUNS=0
REPLACEMENT_RUNS=0
MISSING_RUNS=0
SELECTIVE_RERUN_USED=NO
```

Cohort wall clock 01:56 (00:00:26 → 01:56:47). No timeouts, no crashes, no
infrastructure failures in any arm.

## 5. Results

### Per tool

| | CodeLeveler | AtomCode | DSH |
| --- | ---: | ---: | ---: |
| Runs | 8 | 8 | 8 |
| **Success (`expect_passed`)** | **6/8 (75 %)** | **8/8 (100 %)** | **6/8 (75 %)** |
| Timeouts | 0 | 0 | 0 |
| Crashes | 0 | 0 | 0 |
| `false_completion` | 2 | 0 | 2 |
| `stop_class` unclassified | 0 | 2 | 2 |
| Median wall time | **472 s** | **99 s** | **160 s** |
| Total wall time | 64 min | 19 min | 25 min |
| Input / output tokens | 9,099,327 / 345,339 | N/A | N/A |
| Cost | $1.36 | N/A | N/A |

### Per case, side by side

| Case | Rep | CodeLeveler | AtomCode | DSH |
| --- | ---: | --- | --- | --- |
| n3-caller-propagation | 1 | ✓ 299.5 s | ✓ 38.9 s | ✓ 73.8 s |
| | 2 | ✓ 227.8 s | ✓ 51.9 s | ✓ 57.8 s |
| icg-5-long-task | 1 | ✓ 579.1 s | ✓ 254.3 s | ✓ 201.9 s |
| | 2 | ✓ 455.2 s | ✓ 115.1 s | ✓ 270.4 s |
| **scale-s800** | 1 | **✗ 488.2 s** | ✓ 82.4 s | **✗ 118.1 s** |
| | 2 | **✗ 256.1 s** | ✓ 62.2 s | **✗ 89.3 s** |
| yq-doc-count | 1 | ✓ 714.2 s | ✓ 267.2 s | ✓ 326.2 s |
| | 2 | ✓ 790.3 s | ✓ 241.4 s | ✓ 380.0 s |

Three of four cases separate nobody — all six runs pass. **The entire
correctness signal is `scale-s800`**, where AtomCode went 2/2 and both
CodeLeveler and DSH went 0/2. Two harnesses failing the same case twice each,
deterministically, points at the task rather than at a flake.

## 6. `false_completion` — the number, and why it needs its qualifier

```
false_completion = claimed_done AND NOT expect_passed
```

Both of CodeLeveler's and both of DSH's arise from the same two `scale-s800`
failures. **The raw metric is reported as measured and was not adjusted.** It
also does not mean the same thing on both sides, and the runner's own
`claim_method` field is what shows this:

| | CodeLeveler | DSH |
| --- | --- | --- |
| What the tool emitted | "Completed in N rounds, **but not independently verified**" | matched `"all tests pass"` / `"the task is complete"` |
| `claim_method` | `structured` (its own Completion Contract terminal state) | `heuristic` (the harness inferring from output text) |
| `stop_class` | `CompletedUnverified` | `HeuristicSuccess` |
| Counted as `claimed_done` | **True** | True |

`parse_claimed_completion` maps `CompletedUnverified` to `claimed_done=True`,
so a *qualified* claim — one that says in its own words that it was not
verified — and an *unqualified* success claim collapse into the same
`false_completion=True`.

**Both facts stand side by side and neither is softened.** CodeLeveler did fail
`scale-s800`, twice, and that failure is real and counted. It also labelled its
own output as unverified while failing, which DSH did not. Whether that
distinction should survive into the metric is a **metric design question for
the next eval revision**, not something to fix inside a finished cohort — the
oracle is frozen and changing it after the fact is selective rescoring.

Recommendation carried forward, not applied: `false_completion` should separate
unqualified from qualified completion claims.

## 7. CodeLeveler terminal truth

```
VERIFIED=0
COMPLETED_UNVERIFIED=8
BLOCKED=0
FAILED=0

CORRECT_AND_VERIFIED=0
CORRECT_AND_COMPLETED_UNVERIFIED=6
INCORRECT_AND_COMPLETED_UNVERIFIED=2
CORRECT_AND_BLOCKED=0
INCORRECT_AND_BLOCKED_OR_FAILED=0
INCORRECT_AND_VERIFIED=0        ← no false Verified

FALSE_VERIFIED_TOTAL=0
```

**Not one of the 8 runs reached `Verified`**, including the six it got right.
This is the `Correct + CompletedUnverified` class from the F7 dogfood
reproducing on an independent benchmark: 11 of 24 there, 6 of 8 here. The
authority floor holds — nothing incorrect was ever marked Verified — and the
yield cost it was accepted with is visible again.

### A consequence worth stating plainly

`harness_rc` is **1 on all 8 CodeLeveler runs**, including the six correct
ones; it is 0 on all 16 AtomCode and DSH runs. CodeLeveler carries "I could not
prove this" all the way out to its process exit code, while the other two exit
0 on a heuristic reading of their own output.

Nothing in this cohort depends on that — `case.expect` is the only oracle, and
the exit code is diagnostic. **But any evaluation that treats exit code as
success would score CodeLeveler 0/8 while it was in fact 6/8.** That is a real
integration hazard for a downstream harness, and it is a property of the
product, not of this eval.

## 8. What Phase B cannot answer

Stated before the comparisons, so they are read with these limits.

**Comparative completion truthfulness: not measured.** §3 — the one case built
for it is unjudgeable in this lane for everyone. `false_completion` fired only
as a by-product of `scale-s800`, and §6 shows it does not mean the same thing
across tools.

**Latency is confounded.** CodeLeveler is 3–5× slower at the median, and that
number mixes two things this data cannot separate: its own extra model calls
(contract derivation and the reconciliation judge are real product behaviour,
legitimately measured) and the gateway difference — AtomCode reaches the same
upstream model through AtomGit while the other two go through taotoken.
`GATEWAY_PARITY=PARTIAL` is not a footnote here; it is inside every wall-time
comparison.

**Efficiency is single-sided.** Only CodeLeveler emits tokens; `token_source`
is empty for both others. This is not a collection defect — those harnesses do
not report them. Cost ranking is therefore impossible and only CodeLeveler's
absolute figure is given.

**n = 2 per tool per case.** Eight points per tool. CodeLeveler's input tokens
on `icg-5-long-task` went 799,858 → 1,205,868 between reps (+51 %), and its
`n3` wall time was 476/527 s in the invalidated attempt against 300/228 s here
— same object, same task. Medians over eight points are descriptive; there is
no p90 worth printing and no significance to claim.

**Case-set representativeness is not established.** Four cases, chosen by the
lock from a manifest of ten, and the lane's baseline-red requirement
structurally excludes honest-failure and refusal tasks for every tool. What was
measured is "fix something broken", not "judge whether the ask is possible".

**Two `stop_class=Unknown` each for AtomCode and DSH** (4 of 24 runs) — the
heuristic parser could not classify their terminal state. Their `expect` result
is unaffected; their claim data is missing.

## 9. Comparative findings

```
CORRECTNESS_LEADER=AtomCode        8/8 vs 6/8 vs 6/8
RELIABILITY_LEADER=all three       0 timeouts, 0 crashes, 0 infra failures
LATENCY_LEADER=AtomCode            median 99s vs 160s vs 472s (gateway-confounded)
COST_LEADER=N/A                    not comparable
```

**AtomCode** — the only harness to pass every case, and the fastest by median.
It is the only one that solved `scale-s800`, twice. Its terminal state is
inferred by the harness rather than reported by the tool, and it produced two
of the four unclassifiable stop states.

**DeepSeek Harness** — matches CodeLeveler on correctness (6/8), at roughly a
third of the wall time, failing the same case. Like AtomCode it reports no
tokens and its completion claim is heuristic; unlike CodeLeveler its two
failures were claimed as unqualified success.

**CodeLeveler** — same correctness as DSH, slowest by a wide margin, and the
only one whose completion claim is structured rather than guessed. Its
strength here is not a benchmark score: it is that its terminal state is a
product fact — 0 false Verified, and a labelled `CompletedUnverified` even on
the runs it got right. Its weakness is equally clear: it lost `scale-s800`
twice where AtomCode won it twice, and it spent 64 minutes of wall clock
against AtomCode's 19.

The honest summary of `scale-s800`: two harnesses failed it deterministically
and one solved it deterministically. That is the single real capability gap
this cohort found, and four cases with two reps is not enough to explain it.
Diagnosing it belongs to Phase C or a targeted probe, not to a re-read of these
logs.

## 10. New risks

```
NEW_FALSE_VERIFIED_OBSERVED=NO
NEW_UNSAFE_MUTATION_OBSERVED=NO
NEW_DATA_LOSS_OBSERVED=NO
NEW_RUNTIME_CORRUPTION_OBSERVED=NO
NEW_BENCHMARK_INDEPENDENT_CORRECTNESS_DEFECT=NO
NEW_BETA_READINESS_RISK=NO
```

`scale-s800` is a capability gap, not a release-stopping defect: the failure is
honest (no false Verified), bounded (no timeout, no crash, no unsafe write),
and shared with another harness. Changed-file counts stayed in the same range
across all three tools; nothing wrote outside its workspace.

Two items are carried forward as observations, not blockers:

1. `harness_rc=1` on correct CodeLeveler runs (§7) — an integration hazard for
   any downstream harness keying on exit codes.
2. `false_completion` conflates qualified and unqualified claims (§6) — a
   metric-definition fix for the next eval revision.

## 11. Integrity

```
CODELEVELER_BASELINE_MUTATED=NO
ATOMCODE_OBJECT_MUTATED=NO
DSH_OBJECT_MUTATED=NO
MODEL_MUTATED=NO
PROMPT_MUTATED=NO
TASK_SET_MUTATED=NO
TIMEOUT_MUTATED=NO
ORACLE_MUTATED=NO
EVAL_HARNESS_MUTATED_DURING_FORMAL_COHORT=NO
FORMAL_COHORT_CONTRACT_DRIFT=NO
SELECTIVE_RERUN_USED=NO

PHASE_B_COHORT_VALID=YES
PHASE_B_EXECUTION_GATE=PASS
```

The gate measures whether the experiment was run honestly: the planned cohort
completed, the objects stayed frozen, one oracle judged everything, no run was
re-rolled for a better number, and the evidence is complete. CodeLeveler placing
second changes none of that.

`READY_FOR_PHASE_C=YES`.

## 12. What Phase C should take from this

1. **`scale-s800` is the open question.** One harness solves it reliably, two
   fail it reliably. Worth a targeted probe before drawing capability
   conclusions from a 4-case sample.
2. **This lane cannot see refusal behaviour.** Any comparison that wants to
   test honest failure needs an oracle that judges negative cases, which the
   native eval lane already has.
3. **Latency needs the gateway removed as a variable** before "CodeLeveler is
   3–5× slower" can be attributed to the product.
4. `false_completion` needs the qualified/unqualified split (§6).

## 13. Evidence

Raw artifacts are local to the eval lab and not versioned:

```
$DOGFOOD_ROOT/eval/state/phase-b-7486c3377f19/
  formal/                              24 run records, logs, per-run evidence
  formal-attempt1-INVALIDATED/         the discarded first cohort, kept
  final-dry-run-attempt2/              the dry runs that cleared the harness
  contract-fingerprint-start.txt
  contract-fingerprint-end.txt
  INVALIDATION_NOTE.txt
eval/manifests/comparative-phase-b.yaml   lab binding manifest
eval/baselines/beta-7486c3377f19/         the frozen artifact and its hash
```
