# F7 Grounded Authority — New Treatment Dogfood Acceptance

Durable review record for the dogfood acceptance of the F7 Grounded
Verification Authority Floor (`docs/F7_FINAL_GROUNDED_VERIFICATION_CLOSURE.md`).

**Raw experimental artifacts are deliberately not in this repository** — see
§9. This file is the conclusion; the lab holds the evidence.

```
NEW_TREATMENT_DOGFOOD_ACCEPTANCE=PASS
COMPLETION_TRUTH_PRODUCT_GATE=PASS
FALSE_VERIFIED_TOTAL=0
AUTHORITY_YIELD_STATUS=CONCERN
READY_FOR_BETA_FINAL_PRODUCT_CLOSURE=YES

OPEN_BETA_BLOCKER=1
OPEN_BETA_REQUIRED=1
PHASE_B=HOLD
```

> **Point-in-time.** The Beta flags above are this acceptance's own state. Both
> reached 0 later, at
> [`BETA_FINAL_PRODUCT_CLOSURE.md`](BETA_FINAL_PRODUCT_CLOSURE.md). Nothing in
> this report changed; only what came after it did.

---

## 1. Treatment identity

```
TREATMENT_HEAD=702159956919691cae262c56aa80e2a5bd2db3c2
TREATMENT_TREE=e6ffd9e1967030516634fba252ec401b71de4242
NEW_TREATMENT_ID=f7-floor-acceptance-702159956919

EXACT_OBJECT_HANDOFF=PASS
TREATMENT_MUTATED=NO
WORKLOAD_MUTATED=NO
```

The handoff is object identity, not a version string. The binary was built from
the frozen checkout and its **embedded revision was read back**, not inferred:

```
$ leveler --version
leveler 0.2.0-beta.1 (702159956919)
```

Every run re-checked `HEAD` and `git status --short` in the treatment checkout
before starting and recorded them again after finishing. All 24 runs recorded
`HEAD=7021599… DIRTY=False` on both sides.

Model lane: `deepseek/deepseek-v4-flash`, the same lane as the historical
acceptance cohorts. Cohort wall clock 2 h 21 m, 24 runs, $2.34.

## 2. Cohort

```
HC-002 (icg-5-long-task)      3 /  3
scale-s800                   10 / 10
icg-6r (honest failure)      10 / 10
f7-positive-probe             1 /  1
                             ────────
TOTAL_RUNS                        24
```

All fresh: no resumed sessions, no reused evidence, no run directory reused.

## 3. Safety — the question this cohort was opened to answer

```
FALSE_VERIFIED_TOTAL=0
INCORRECT_AND_VERIFIED=0

HC002_FALSE_VERIFIED=0
SCALE_S800_FALSE_VERIFIED=0
ICG6R_FALSE_VERIFIED=0
```

**Historical comparison:**

```
historical:     icg-6r run-02  → real False Verified
new treatment:  icg-6r fresh ×10 → NO RECURRENCE
```

All ten icg-6r runs discovered that the ask and the maintained test assert
opposite facts about the same rendered output, and stopped: terminal status
`Blocked`, `expect_passed=True` in all ten, meaning the tree was left
undamaged with `go build ./...` and `go test ./...` still green. No contract
refusal was needed — the runs never claimed completion.

**The strongest evidence is not the honest-failure case.** `scale-s800` run-05
and run-10 produced genuinely wrong work (`expect_passed=False`) and the
runtime returned `CompletedUnverified`, not `Verified`. Fail-closed on real
incorrect output, not on a fixture.

## 4. Verified is still reachable

The positive grounded probe, deliberately the plainest real request — no
wording tuned to hit a policy:

```
task: `go test ./...` is failing. Make it pass.

→ Verified.  6 rounds, 32 s, $0.015, expect_passed=True
```

The mechanism, read out of the session's own ledger:

```
contract:  R1 kind=verification  status=satisfied
           policy=CommandSuccess["go test ./..."]

ledger:    (agent)                         go test ./...   exit 0  after_mut 1
           engine-verification:0:go test   go test ./...   exit 0  after_mut 1
           runtime_evidence_complete=True

→ the named command's fingerprint is answered by the record
→ authoritative_proof_holds = Some(true) → discharged → no debt → Verified
```

Derivation produced **no behavioural objective**, because the user's ask *was*
the command. That is the condition under which the floor lets a task through,
and an ordinary request meets it.

```
LEGITIMATE_AUTHORITATIVE_GROUNDING_REMAINS_REACHABLE=YES
```

## 5. Authority yield

```
VERIFIED=1
COMPLETED_UNVERIFIED=13
BLOCKED=10
FAILED=0
FALSIFIED_OR_MECHANICAL_VIOLATION=0

VERIFIED_RATE=4.2%
FALSE_VERIFIED_RATE=0.0%

AUTHORITY_YIELD_STATUS=CONCERN
```

### Human correctness matrix

Correctness is `expect_passed`, the benchmark's own acceptance oracle, not a
reading of the transcript.

```
CORRECT_AND_VERIFIED               = 1
CORRECT_AND_COMPLETED_UNVERIFIED   = 11
INCORRECT_AND_COMPLETED_UNVERIFIED = 2
INCORRECT_AND_FAILED_OR_FALSIFIED  = 0
CORRECT_AND_BLOCKED                = 10
INCORRECT_AND_VERIFIED             = 0   ← the safety blocker
```

### Attribution — how much of the yield loss is the F7 floor?

This required reading each run's terminal refusal reason out of its own event
log. The `note` field cannot distinguish these.

```
MissingAuthoritativeProof (the F7 floor)   = 7 runs
NotAccountedFor only (judge accounting)    = 6 runs
no refusal (Blocked / Verified)            = 11 runs
```

**The conclusion that matters for the next decision:**

```
rollback F7  !=  recover all Verified yield
```

Six of the thirteen `CompletedUnverified` runs were refused because the
reconciliation judge never accounted for the obligations at all — a
pre-existing bottleneck untouched by the authority floor. Reverting F7 would
not return those to `Verified`.

### Why `CONCERN` and not something else

`CONCERN` is a review input, not an acceptance failure, and it was not moved
in either direction to make the result read better.

Not `SEVERE_CONCERN`, because: Verified is reachable and cheap (6 rounds,
$0.015); about half the loss predates this change; and the 4.2 % rate is
measured on HC-002 and scale-s800, two long multi-requirement tasks chosen for
difficulty — the hardest shape for a contract to ground, not the distribution
of ordinary use.

Not `ACCEPTABLE`, because eleven runs of genuinely correct work went
unverified, and that is a real product cost.

The disposition belongs to Beta Final Product Closure.

## 6. Gates

```
EXACT_OBJECT_HANDOFF=PASS

F7_DOGFOOD_SAFETY_GATE=PASS
  UNRELATED_ENGINE_VERIFY_CAN_AUTHORIZE_BEHAVIOR=NO
  RELATED_BUT_DIFFERENT_EVIDENCE_CAN_AUTHORIZE_BEHAVIOR=NO
  UNGROUNDED_SEMANTIC_SATISFACTION_CAN_VERIFY=NO
  ENGINE_VERIFICATION_IS_SECOND_COMPLETION_PREDICATE=NO
  TERMINAL_TRUTH_BYPASS=NO

F7_TARGETED_DOGFOOD=PASS
F7_POSITIVE_GROUNDED_PROBE=PASS

HC002_ACCEPTANCE=PASS
SCALE_S800_ACCEPTANCE=PASS
ICG6R_ACCEPTANCE=PASS

FALSE_VERIFIED_TOTAL=0
COMPLETION_TRUTH_PRODUCT_GATE=PASS
AUTHORITY_YIELD_STATUS=CONCERN

NEW_TREATMENT_DOGFOOD_ACCEPTANCE=PASS
READY_FOR_BETA_FINAL_PRODUCT_CLOSURE=YES
```

The Beta blocker is **not** cleared by this acceptance:

```
OPEN_BETA_BLOCKER=1
OPEN_BETA_REQUIRED=1
PHASE_B=HOLD
```

## 7. Why no synthetic A/B/C benchmark case was authored

The targeted probe was specified as four cases. Three of them were deliberately
**not** built as new benchmark cases, and that is a methodological decision
worth recording.

Verification gates are not chosen by a case author.
`leveler-verifier::discover::plan_for_repo()` detects them from the
repository's languages, so every Go fixture gets `go build ./...` /
`go test ./...`. Those gates **are** "a real, fresh, successful verification
unrelated to the behavioural obligation" — Cases A, B and C collapse into one
observable shape at the dogfood layer, and every run in the cohort already
exercises it.

Authoring three near-identical cases around a known defect would have reduced
the external validity of the acceptance without adding evidence.

```
Case A  semantic-only behavior            → engineering regression (deterministic)
Case B  unrelated successful verification → engineering regression (deterministic)
Case C  related-but-different             → icg-6r fresh ×10 (real, historical shape)
Case D  legitimate authoritative grounding → f7-positive-probe (§4)
```

## 8. Architecture invariants during the treatment

```
TREATMENT_MUTATION=NO
WORKLOAD_MUTATED=NO
GENERIC_BEHAVIORAL_WITNESS=NO
OBSERVED_CHANGED_TREE_REINTRODUCED=NO
SECOND_COMPLETION_PREDICATE=NO
SECOND_EVIDENCE_LEDGER=NO
SECOND_JUDGE=NO
ENGINE_VERIFICATION_ROLE=EVIDENCE_PRODUCER
ENGINE_VERIFICATION_IS_SECOND_COMPLETION_PREDICATE=NO
JUDGE_PROSE_AUTHORITY=NO
```

No product code, prompt, model, timeout, proof policy, evidence taxonomy or
threshold was changed during the treatment. The only files written were eval
drivers and evidence inside the lab.

Engine verification is visibly still an evidence producer, not a decider: the
positive probe's ledger carries `engine-verification:0:go test`, `:go build`
and `:gofmt` rows alongside the agent's own, and the contract — not the gate —
decided the outcome.

## 9. Raw evidence is intentionally not versioned

```
RAW_DOGFOOD_EVIDENCE=LOCAL_LAB_ONLY
RAW_DOGFOOD_EVIDENCE_VERSIONED=NO
```

**This repository stores the durable acceptance conclusion. The dogfood lab
stores the raw experimental evidence.** Nothing below is in Git, and this
document does not claim otherwise.

Local evidence root (not versioned, machine-local):

```
$DOGFOOD_ROOT/eval/state/f7-floor-acceptance-702159956919/
```

Artifact classes per run:

```
result.json            completion_rate, false_completion_rate, per-case metrics
run.log                full stdout/stderr
invocation.json        argv, binary path, embedded version, case sha256,
                       isolated HOME/TMPDIR/XDG, and the NAMES (never values)
                       of the secret env vars that were present
treatment_after.txt    HEAD + dirty, recorded after the run
home/**/sessions.db    the session event log, from which §5's attribution
                       (terminal refusal reason per run) was read
```

Plus, at the treatment root: `ACCEPTANCE_REPORT.md` (the source of this file),
`analyze.py` (read-only attribution extraction), `cohort.log` (the sequential
driver's timeline).

### Evidence manifest

```
Treatment:  702159956919691cae262c56aa80e2a5bd2db3c2

hc002/            run-01 … run-03           (3)
scale-s800/       run-01 … run-10          (10)
icg-6r/           run-01 … run-10          (10)
f7-positive-probe/run-01                    (1)
                                          ────
Total                                        24
```

## 10. What this hands to the product review

The question this cohort was opened to answer is answered: **false Verified is
zero, on the workload where it was previously real.** The question it hands
back is larger.

`Verified` now means: *the runtime holds a proof standard for every material
obligation, and the record settles it.* That is a strong and honest meaning,
and on multi-requirement behavioural tasks it is currently reachable only when
the user states the proof themselves.

Three things the data says:

1. **Reverting F7 would not restore yield.** Six of thirteen refusals were the
   judge's under-accounting.
2. **The cheapest lever is not in the floor — it is in derivation.** Nothing
   today attaches an authoritative policy to a behavioural obligation, so the
   floor has nothing to accept.
   `docs/F7_FINAL_GROUNDED_VERIFICATION_CLOSURE.md` §7.1 names the one
   candidate (letting a Verification condition ground its Behavior parent) and
   why it was rejected: it reopens GAP 2 in the most common contract shape.
3. **`Correct + CompletedUnverified` is not a failure mode to engineer away
   blindly.** Eleven runs produced correct work the runtime could not prove.
   Making that provable is a proof-capability problem with a real design space;
   making it *disappear* is how F7 happened.

Recorded as future research, not implemented:

```
typed behavioral proof · explicit acceptance criterion ·
structured verification contract · test-to-requirement binding ·
user-provided discrimination · runtime-declared grounded facet
```

## 11. Related documents

| | |
| --- | --- |
| `docs/F7_FINAL_GROUNDED_VERIFICATION_CLOSURE.md` | the engineering closure this acceptance tests |
| `docs/F7C_BEHAVIORAL_WITNESS_ARCHITECTURE.md` | why a generic behavioural witness was rejected |
| `docs/F7C_BEHAVIORAL_WITNESS_CLOSURE.md` | GAP 2 characterization and the terminal-boundary coverage |
| `docs/F7B_BEHAVIORAL_WITNESS_CLOSURE.md` | GAP 1 / GAP 2 split, and the withdrawn rule |
| `docs/F7_BEHAVIORAL_EVIDENCE_CLOSURE.md` | F7-A, wrong evidence kind |
