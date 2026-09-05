# Phase B — `scale-s800` Comparative Failure Analysis

Diagnosis only. No product code, baseline, harness, oracle or score was
changed, and no new run was executed.

```
PRIMARY_CAUSE=WORKSPACE_HYGIENE (verification artifacts left inside the repo)
PRIMARY_CAUSE_CONFIDENCE=HIGH

ALL SIX RUNS FIXED THE BUG CORRECTLY.
The 2/2 vs 0/2 split is entirely the oracle's file-scope constraint.
```

**Every one of the four "failing" runs passes `go test ./...`, builds, and
produces the exact expected four-line CLI output.** That was verified by
running those assertions against the preserved worktrees, not inferred from the
oracle's script order.

---

## 1. The case

```
SCALE_S800_CASE_ID=scale-s800
TIMEOUT=1800s
FORMAL_RUNS=6   (3 tools × 2 reps)
```

`TASK_OBJECTIVE` — a window must cover `[start, start+width)`. An event a
second before a boundary is landing in the next minute's row; fix it.

`SUCCESS_ORACLE` — five assertions under `set -euo pipefail`, in this order:

```
1. go test ./...                          passes
2. build + CLI emits exactly four lines    t=0 hits 3 / t=60 hits 12 /
                                           t=60 misses 16 / t=120 misses 32
3. changed files non-empty
4. EVERY changed path is internal/window/* or docs/*     ← the scope rule
5. a *_test.go is among the changed files
```

Assertion 4 is the one this analysis is about. Its own comment says a
distractor package edited "instead — **or as well —** fails here".

`PRIMARY_DIFFICULTY` — as designed, finding one bug in ~800 files. **As
measured, that was not the difficulty**: all three tools found it every time.

## 2. Outcomes

```
CODELEVELER=0/2   ATOMCODE=2/2   DSH=0/2
INFRA_FAILURES=0  TIMEOUTS=0  CRASHES=0
```

## 3. Failure delta — mechanically located

Every failure is the **same assertion**, and none is behavioural:

| Run | `expect_tail` |
| --- | --- |
| leveler r1 | `changed outside the windowing package: internal/pipeline/pipeline_test.go` |
| leveler r2 | `changed outside the windowing package: .e2e-events.txt` |
| dsh r1 | `changed outside the windowing package: cmd/telemetryd/main.go` |
| dsh r2 | `changed outside the windowing package: internal/pipeline/pipeline_test.go` |

`git status` at oracle time, all six runs:

| Run | Core fix | Extra |
| --- | --- | --- |
| atomcode r1 | `M window.go` `M window_test.go` | — |
| atomcode r2 | `M window.go` `M window_test.go` | — |
| leveler r1 | `M window.go` `M window_test.go` | `?? internal/pipeline/pipeline_test.go` |
| leveler r2 | `M window.go` `M window_test.go` | `?? .e2e-events.txt` `?? .e2e-telemetryd` |
| dsh r1 | `M window.go` `M window_test.go` | `M cmd/telemetryd/main.go` `?? cmd/telemetryd/main_test.go` |
| dsh r2 | `M window.go` `M window_test.go` | `?? internal/pipeline/pipeline_test.go` |

**All six made the identical two core edits.** The fix and the required
boundary test are present everywhere.

### The behavioural assertions were re-run, not assumed

`set -e` means a failure at assertion 4 implies 1–3 passed, but that is an
inference. The assertions were executed directly against each preserved
worktree (`DERIVED`, no model calls, no new trajectory):

| Run | `go test ./...` | build | four-line CLI output |
| --- | --- | --- | --- |
| leveler r1 | PASS | PASS | PASS |
| leveler r2 | PASS | PASS | PASS |
| dsh r1 | PASS | PASS | PASS |
| dsh r2 | PASS | PASS | PASS |

So `scale-s800` did not measure a capability gap in finding or fixing the
defect. It measured whether a tool leaves the workspace clean.

## 4. Earliest stable divergence

```
EARLIEST_STABLE_DIVERGENCE=
  where end-to-end verification artifacts are created —
  inside the repository, or outside it
```

Not discovery, not planning, not the edit. All three tools reached a correct
fix; they differed in where they put the scaffolding they used to check it.

**AtomCode — both reps, verbatim from its transcript:**

```
r1:  printf '59 hits 1\n60 hits 2\n119 hits 5\n120 hits 3\n' > /tmp/events_boundary.txt
r2:  go build -o /tmp/telemetryd .
r2:  git diff --stat && rm -f /tmp/telemetryd /tmp/events.txt
```

Outside the tree, and in r2 explicitly removed afterwards. AtomCode also
reasoned about scope before editing:

> "I shouldn't go fix 700 clones… The acceptance 'command-line report reflects
> the rule end to end' = the telemetryd main.go path, which uses
> internal/window. **Only fix there.**"

**CodeLeveler — r2, the causal chain in its own log:**

```
→ update_goal {"status":"complete", …}
⛔ gate refused completion_reconciliation: verdict=Uncertain;
   obligations still open: R4 (The command-line report reflects the rule
   end to end.) — not satisfied; Go test, source implementation, and boundary
   test coverage are evidenced, …
→ apply_patch  *** Add File: .e2e-events.txt
→ run_command  go build -o .e2e-telemetryd ./cmd/telemetryd
```

The completion gate asked for end-to-end evidence; the agent produced it by
creating a fixture and a binary **inside the repository**. Those two files are
exactly what assertion 4 rejects.

r1 is the same shape with a different artifact: after its own refusals it added
`internal/pipeline/pipeline_test.go` — a reasonable integration test, in the
wrong package for this oracle. (It created `.e2e-events.txt` too and did clean
that one up; the test file it left.)

**A control that prevents over-attribution.** Gate refusal is not the
differentiator — it is CodeLeveler's normal behaviour:

| Case | leveler r1 | leveler r2 | outcome |
| --- | --- | --- | --- |
| n3-caller-propagation | 2 refusals | 2 | both PASS |
| icg-5-long-task | 3 | 2 | both PASS |
| yq-doc-count | 2 | 2 | both PASS |
| scale-s800 | 2 | 2 | both FAIL |

All 8 runs were refused 2–3 times. The six that passed were refused too. What
is unique to `scale-s800` is that it is **the only case in the set whose oracle
constrains which files may change** — everywhere else the same debris was
harmless.

So the mechanism is a conjunction, and both halves are needed:

```
(gate demands citable end-to-end evidence)
  × (agent satisfies it with in-repo artifacts)
  × (this case, alone, penalises extra files)
```

## 5. Root cause

```
PRIMARY_CAUSE=WORKSPACE_HYGIENE
  end-to-end verification artifacts created inside the repository
PRIMARY_CAUSE_CONFIDENCE=HIGH
  both CodeLeveler reps show it; both AtomCode reps show the opposite
  (/tmp, plus explicit rm in r2); it is the exact file named in the
  oracle's failure message

SECONDARY_CAUSE=VERIFICATION_ARTIFACT_PLACEMENT_UNDER_GATE_PRESSURE
  the artifacts were created in direct response to a completion-gate refusal
  naming the end-to-end obligation
SECONDARY_CAUSE_CONFIDENCE=MEDIUM
  the causal chain is explicit in r2's log; r1 shows the same shape with a
  different artifact, so the mechanism repeats but the artifact does not

CONTRIBUTING_FACTORS=
  - the case is the only one in the set with a file-scope oracle, so this
    behaviour was invisible in the other three
  - a known, previously DEFERRED finding: D3 recorded "agent temp-file
    hygiene (scripts/.tmp-*, chrome.log)" as MINOR. It has now cost a
    benchmark run.

NON_CAUSAL_OBSERVATIONS=
  - CodeLeveler is 3-8x slower here (488s/256s vs 62-82s). Slowness did not
    cause the failure; both runs finished well inside 1800s with the fix done.
  - harness_rc=1 on both runs. Diagnostic only; the oracle never reads it.
  - 0 Verified. Does not explain 0 correct — see §7.
```

`GENERALIZABLE_TO_CODELEVELER=YES` — the behaviour is producing verification
scaffolding inside the working tree, which is not specific to this fixture. Its
*consequence* is specific to oracles that constrain the change set; a real user
would see stray files rather than a failure.

## 6. CodeLeveler and DSH are not the same failure

```
CL_AND_DSH_SHARED_FAILURE_SURFACE=YES     both fail assertion 4
CL_AND_DSH_SHARED_ROOT_CAUSE=PARTIAL
```

| | Nature |
| --- | --- |
| leveler r1 | added a test in the wrong package — **extra work, wrong location** |
| leveler r2 | left its own verification scratch — **hygiene** |
| dsh r1 | **modified `cmd/telemetryd/main.go`** — a real out-of-scope source edit, a different and arguably more serious miss |
| dsh r2 | added a test in the wrong package — same shape as leveler r1 |

Three of the four are "extra artifacts alongside a correct fix". `dsh r1` is
the only run that edited product source outside the permitted scope.

## 7. CodeLeveler truthfulness

```
CL_SCALE_RUNS=2
CL_CORRECT=0                 (by the oracle; the fix itself was correct — §3)
CL_VERIFIED=0
CL_COMPLETED_UNVERIFIED=2
CL_BLOCKED=0
CL_FALSE_VERIFIED=0
CL_HARNESS_RC=1, 1

SCALE_S800_FALSE_VERIFIED=0
TRUTHFULNESS_HELD=YES
```

Both runs ended `CompletedUnverified` with the output "Completed in N rounds,
**but not independently verified**". Nothing incorrect was marked Verified.

`INCORRECT_BUT_HONESTLY_UNVERIFIED` is the classification, and it is worth
being precise about what it means here: the *code* was correct and the *task*
was failed on a constraint. The runtime did not know either of those things —
it declined to claim verification for its own reasons, which happened to be
right for a reason it could not see.

## 8. Relation to known Beta limitations

| | Relation | Why |
| --- | --- | --- |
| `AUTHORITY_YIELD_CONCERN` | **INDIRECT** | The completion gate's demand for end-to-end evidence is the same mechanism as the yield concern, and it is what prompted the in-repo artifacts. But the refusal is normal — it happened on all 8 runs, 6 of which passed. It is a trigger here, not the cause. |
| `RECONCILIATION_YIELD_CONCERN` | **UNRELATED** | No `NotAccountedFor` refusal appears in either scale-s800 run; the refusals named specific obligations with evidence attached. |
| `EXIT_CODE_SEMANTICS` | **UNRELATED** | `harness_rc=1` on both, as on all 8 runs including the 6 that passed. The oracle is `case.expect`; the exit code is diagnostic. |
| D3 temp-file hygiene (DEFERRED, MINOR) | **DIRECT** | This is that finding, reproduced on an independent benchmark and now with a measurable cost. |

## 9. Confounds

```
MODEL_PARITY=FULL              all three on deepseek-v4-flash
GATEWAY_PARITY=PARTIAL         AtomCode via AtomGit; the others via taotoken
GATEWAY_CONFOUND_PRESENT=YES
REPS_PER_TOOL=2
TOKEN_COMPARABILITY=PARTIAL
CAUSAL_CERTAINTY=LIMITED
```

`HARNESS_LEVEL_SIGNAL=STRONG` for this finding, and the reason is that the
evidence is not statistical: the oracle **names the offending file**, and the
transcript **shows the tool creating it**. That chain does not depend on n, and
no gateway difference explains why one tool writes to `/tmp` and another writes
to the repository root.

`MODEL_VARIANCE_REMAINING=YES` — the two CodeLeveler runs produced *different*
offending artifacts (a test file, then scratch files), so the specific artifact
is variable even though the behaviour repeats.

### A correction to the Phase B report

Phase B recorded that AtomCode and DSH "do not report tokens". **AtomCode does**
— its output ends with `[done] 82.2s tokens=390.44K turns=13 tool_calls=22` and
per-call `prompt=… completion=… cached=…`. The runner's
`parse_token_heuristic` failed to extract them, so `token_source` was empty.

This is a harness parsing gap, not an absence of data. It changes no Phase B
score and the frozen report is not edited; it is recorded here so the next eval
revision can fix the parser rather than repeat the claim. DSH remains
unchecked.

## 10. Comparative matrix

| Dimension | CodeLeveler | AtomCode | DSH |
| --- | --- | --- | --- |
| Task interpretation | correct both reps | correct both reps, with explicit scope reasoning | correct both reps |
| Search coverage | found the defect both reps | found it both reps | found it both reps |
| Mutation coverage | core fix + test, both reps | core fix + test, exactly | core fix + test, both reps |
| Scope discipline | **violated both reps** | held both reps | **violated both reps** |
| Verification | end-to-end, artifacts **in-repo** | end-to-end, artifacts in `/tmp`, cleaned in r2 | end-to-end; r1 also edited `main.go` |
| Recovery | n/a — never learned it had failed | n/a — did not fail | n/a |
| Completion timing | after gate refusals, 488s / 256s | 82s / 62s | 118s / 89s |

`ATOMCODE_STABLE_SUCCESS_PATTERNS` (both reps):
1. verification scaffolding outside the working tree
2. the change set held to exactly the two files the task's scope allows

`ATOMCODE_ONE_OFF_PATTERNS` (r2 only, `POSSIBLE_FACTOR`):
3. explicit `rm -f` of its own temporary files
4. `git diff --stat` as a final change-set review

Pattern 4 is worth naming: it is the only observed instance of a tool
**reviewing its own change set before finishing** — which is precisely what
would have caught all four failures.

## 11. Phase C observation hooks

Observation only. No Phase C task is to be added or altered for these.

```
1. WORKSPACE_HYGIENE_AT_COMPLETION
   Does the tool leave files it created for its own verification? Record
   `git status --porcelain` at terminal state for every run, every tool.

2. CHANGE_SET_SELF_REVIEW
   Does the tool inspect its own diff before claiming completion? AtomCode did
   once and it is the behaviour that separates the outcomes.

3. VERIFICATION_ARTIFACT_PLACEMENT
   In-tree vs out-of-tree scaffolding, and whether it is cleaned up.

4. SCOPE_CONSTRAINT_ADHERENCE
   Where a task names a permitted change surface, is the final change set
   inside it? Distinguish "extra artifact" from "out-of-scope source edit" —
   dsh r1 shows they are different failures.

5. GATE_PRESSURE_SIDE_EFFECTS
   When CodeLeveler's completion gate demands more evidence, what does the
   agent create to satisfy it, and where?
```

## 12. Decision

```
ENGINEERING_BEFORE_PHASE_C_REQUIRED=NO
NEW_BETA_READINESS_RISK=NO
```

Nothing found here is release-stopping. No unsafe mutation — the stray files
are inside the agent's own workspace, which it is entitled to write to. No data
loss, no corruption, no false Verified. The defect was found and fixed
correctly in every run.

What was found is a **workspace-hygiene weakness with a measurable competitive
cost**, and it is the D3 finding that was deferred as MINOR — now with a price
attached. That is a post-Beta engineering input, not a blocker, and this task
does not design the fix.

The one thing worth saying plainly about the benchmark itself: `scale-s800` is
titled a scale case and its stated difficulty is finding one bug among ~800
files. On this evidence it did not measure that at all — all three tools found
it, twice each. What it actually discriminated was cleanup discipline. That is
a real property worth measuring, but the case's name and stated intent do not
describe what it measures, and any conclusion drawn from "CodeLeveler fails the
scale case" would be wrong.

## 13. Evidence

Raw artifacts are lab-local and unversioned. Derived analysis in this document
is marked `DERIVED` where the assertions were re-executed.

```
$DOGFOOD_ROOT/eval/state/phase-b-7486c3377f19/formal/
  scale-s800--{leveler,atomcode,dsh}--r{1,2}.jsonl      run records
  ev-scale-s800--*/*/scale-s800-r*/git.status           change sets at oracle time
  ev-scale-s800--*/*/scale-s800-r*/git.diff             the fixes themselves
  ev-scale-s800--*/*/scale-s800-r*/harness-output.log   transcripts
  ev-scale-s800--*/*/scale-s800-r*/ws/                  preserved worktrees
                                                        (§3's re-run used these)
```
