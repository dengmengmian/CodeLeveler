# Formal Three-Way Eval — Phase C Real-Task Contract

```
PHASE_C_TASK_SELECTION=PASS
PHASE_C_CONTRACT_FREEZE=PASS
PHASE_C_CONTRACT_STATUS=FROZEN
READY_FOR_PHASE_C_FORMAL_COHORT=YES

PHASE_C_FORMAL_COHORT_STARTED=NO
NEW_FORMAL_AGENT_RUNS=0

PHASE_C_TASK_MANIFEST_FINGERPRINT=
2dfedd5b9c51da2555ef83afcc1d874d5cb6fd3ac85092b641f02688d287ab32
```

Three real tasks, from three third-party repositories, each pinned to a commit
that predates its upstream fix. Every one was verified to fail its acceptance
check at the pinned start and pass it at the upstream fix, **by running them**.
No model was called and no agent run was executed.

---

## 1. Governance decisions this contract implements

Recovered contract was incomplete; the missing fields were human calls and were
made. They are inputs here, not conclusions.

```
D1=THREE_WAY_COMPARATIVE_REAL_TASKS
D2=3 REAL TASKS, EXPLICITLY SELECTED AND PINNED
D3=MECHANICAL_CHECKS + BLIND HUMAN ACCEPT/REJECT
D4=1 REP_PER_TOOL_PER_TASK
D5=DEFAULT_TIMEOUT=3600s
D6=USE_FROZEN_CODELEVELER_ARTIFACT (sha256 121983c9…)
D7=INHERIT_PHASE_B_RESET_INVALID_RERUN_POLICY
```

Document roles, frozen:

```
A1_ROLE=PHASE_C_GOVERNANCE_ANCESTOR        (PRE_BETA_EVAL_PLAN.md)
A2_ROLE=REAL_TASK_METHODOLOGY_AND_CANDIDATE_SOURCE   (REAL_USAGE_BETA_001.md)
A2_IS_PHASE_C_SOURCE_OF_TRUTH=NO
A2_RELEASED_BINARY_REQUIREMENT=SUPERSEDED_FOR_PHASE_C
```

Excluded from the comparative cohort because no fair three-way form exists:

```
BROWSER_TASK_IN_PHASE_C_COMPARATIVE=NO
RESTART_TASK_IN_PHASE_C_COMPARATIVE=NO
CL_ONLY_CAPABILITY_TASK_IN_PHASE_C_COMPARATIVE=NO
```

## 2. The three tasks

| | C1 | C2 | C3 |
| --- | --- | --- | --- |
| Category | backend, cross-file correctness | frontend, user-visible behaviour | medium-long repository task |
| Repository | `go-task/task` | `react-hook-form` | `cli/cli` |
| Issue | [#1230](https://github.com/go-task/task/issues/1230) (2023-06-22) | [#13674](https://github.com/react-hook-form/react-hook-form/issues/13674) (2026-08-23) | [#13399](https://github.com/cli/cli/issues/13399) (2026-05-12) |
| What it is | `method: timestamp` skips a task whose generated file was deleted | a `Controller` under a `null` parent submits `undefined` | `gh cs ports forward` crashes with `concurrent map writes` |
| Repo size | 691 files | 296 files | 1399 files |
| Baseline suite | 122 tests, 11 s | 1277 tests, 2.7 s | 245 packages, 2 m 34 s |
| Reference fix | 6 files, +135 | 3 files, +72 −9 | 3 files, +109 −78 |

Three languages, three verification styles (CLI behaviour, jsdom DOM assertion,
race detector), three repository scales. None is a fixture; all three issues
were filed by users against the real project, years or months before this eval
existed.

### C1

```
C1_TASK_ID=C1
C1_CATEGORY=backend_cross_file_correctness
C1_REPOSITORY=go-task/task
C1_ISSUE_REFERENCE=https://github.com/go-task/task/issues/1230
C1_ISSUE_CREATED_AT=2023-06-22
C1_PINNED_START_HEAD=8fa9dc04aca4a372bd4c5a7ba84a2bc3e7eab82a
C1_PINNED_START_TREE=c1534212ffb4b40dd91cca0245c264e63f4165b6
C1_GOAL_SHA256=f6817b4c644e2b7c651c1683af03f13bff07c841292c28aab1b231145a07dc43
C1_GOAL_TRANSFORMATION=NONE
C1_BASELINE_BUILD=PASS
C1_BASELINE_TESTS=PASS      (go test ./…, 12 packages, 0 failures)
C1_ACCEPTANCE_CHECKS=oracles/c1-check.sh
C1_ORACLE_BASELINE_RED=YES
C1_ORACLE_REFERENCE_GREEN=YES
C1_TIMEOUT=3600
C1_PREVIOUS_REPO_EXPOSURE=YES   (§6)
C1_PREVIOUS_TASK_EXPOSURE=NO
```

Acceptance builds the agent's tree, requires the repository suite to stay green,
then drives the binary through the issue's own scenario in a scratch directory
outside the workspace: run once (task runs), run again (reports up-to-date),
delete the generated file, run a third time — **it must re-run**. At the pinned
start it does not, with the message the issue describes.

### C2

```
C2_TASK_ID=C2
C2_CATEGORY=frontend_user_visible_behavior
C2_REPOSITORY=react-hook-form/react-hook-form
C2_ISSUE_REFERENCE=https://github.com/react-hook-form/react-hook-form/issues/13674
C2_ISSUE_CREATED_AT=2026-08-23
C2_PINNED_START_HEAD=84188ebbeaa1c4dc9442bc1c846a9ebe14c405bb
C2_PINNED_START_TREE=9c5326521af1f4dce2096b2e90aaa9ccd8be2626
C2_GOAL_SHA256=87e23125b7b3a04ec324b0ea5554c2928b136e3d4e63c0f7383fc3f14c01f3f6
C2_GOAL_TRANSFORMATION=MINIMAL_NORMALIZATION
C2_BASELINE_BUILD=PASS
C2_BASELINE_TESTS=PASS      (120 suites / 1277 tests, 0 failures)
C2_ACCEPTANCE_CHECKS=oracles/c2-check.sh
C2_ORACLE_BASELINE_RED=YES
C2_ORACLE_REFERENCE_GREEN=YES
C2_TIMEOUT=3600
C2_PREVIOUS_REPO_EXPOSURE=NO
C2_PREVIOUS_TASK_EXPOSURE=NO
```

**What was removed from the goal, and why.** The issue is unusually thorough:
it carries a Codesandbox link and a "Likely origin" section that quotes an
upstream diff and names the file that diff touched. Both are gone. The diff is
patch material and the rule against exposing it is absolute; the sandbox is an
external service a headless agent cannot reach. Everything else — the version
table, the reproduction steps, the JSX, the expected-behaviour paragraph, the
"why it is easy to miss" analysis — is verbatim.

The acceptance test asserts only the requirement the issue states: the nested
field must submit a **defined** value, `null` or `''` both acceptable. It does
not require the implementation upstream chose.

### C3

```
C3_TASK_ID=C3
C3_CATEGORY=medium_long_repository_task
C3_REPOSITORY=cli/cli
C3_ISSUE_REFERENCE=https://github.com/cli/cli/issues/13399
C3_ISSUE_CREATED_AT=2026-05-12
C3_PINNED_START_HEAD=6dae3077b89c9858c5778c0b37a116b1091f8782
C3_PINNED_START_TREE=199f1b8cd17c9b2c19f24e9bbf194b31be71b54e
C3_GOAL_SHA256=473d66440fadcddaa5c106a3f5900a04edda9b452e9b41cfbd8ce1aab25115ea
C3_GOAL_TRANSFORMATION=NONE
C3_BASELINE_BUILD=PASS
C3_BASELINE_TESTS=PASS      (245 packages, 0 failures)
C3_ACCEPTANCE_CHECKS=oracles/c3-check.sh
C3_ORACLE_BASELINE_RED=YES   (Go race detector reports the reported race)
C3_ORACLE_REFERENCE_GREEN=YES
C3_TIMEOUT=3600
C3_ENV=GOTOOLCHAIN=local     (same for all three tools, §6)
C3_PREVIOUS_REPO_EXPOSURE=NO
C3_PREVIOUS_TASK_EXPOSURE=NO
```

The agent cannot reproduce this by hand — it needs a live codespace. It has to
reason from the stack trace to the shared-state problem and fix it. Acceptance runs
the repository suite, then a concurrency test built only from the package's
exported constructors, under `-race`. At the pinned start the detector reports
the race at `port_forwarder.go:164`, the line the issue's own stack trace names.

## 3. Candidates reviewed and rejected

```
CANDIDATES_REVIEWED=17
CANDIDATES_REJECTED=14
SELECTED_TASKS=3
```

| Candidate | Real issue | Repro SHA | Mech. oracle | 3-way fair | Size | Exposure | Keep |
| --- | --- | --- | --- | --- | --- | --- | --- |
| go-task #1230 timestamp re-run | ✓ | ✓ | ✓ | ✓ | 6f/+135 | repo only | **C1** |
| rhf #13674 Controller null parent | ✓ | ✓ | ✓ | ✓ | 3f/+72−9 | none | **C2** |
| cli #13399 concurrent map writes | ✓ | ✓ | ✓ | ✓ | 3f/+109−78 | none | **C3** |
| go-task #2588 CLI var priority | ✓ | ✓ | ✓ | ✓ | 5f/+39−8 | repo only | shortlist, not needed |
| go-task #2102 `USER_WORKING_DIR` | ✓ | ✓ | ✓ | ✓ | 4f | repo only | `SOLUTION_IN_ISSUE` — reporter names the variable to add |
| go-task #1909 defer var interpolation | ✓ | ✓ | ✓ | ✓ | **2 src lines** | repo only | `TOO_TRIVIAL` |
| go-task #1566 prefix on up-to-date | ✓ | ✓ | ✓ | ✓ | **6 src lines** | repo only | `TOO_TRIVIAL` |
| go-task #1881 malformed include | ✓ | ✓ | ✓ | ✓ | 4 src lines | repo only | `TOO_TRIVIAL` |
| go-task #2894 matrix `ref:` race | ✓ | ✓ | ✓ | ✓ | — | **same task as R009** | `PREVIOUSLY_EXPOSED` |
| cli/cli auth-token host:port | ✓ | ✓ | ✓ | ✓ | 4f | none | `BASELINE_RED_IN_TASK_AREA` — see §4 |
| rhf #13646 `valueAsDate` min/max | ✓ | ✓ | ✓ | ✓ | 7 src lines | none | `TOO_TRIVIAL` |
| rhf #13645 field-array root error | ✓ | ✓ | ✓ | ✓ | 24 src lines | none | shortlist, not needed |
| gohugoio/hugo | ✓ | ✓ | ~ | ✓ | — | **R003 executed** | `PREVIOUSLY_EXPOSED` |
| johnkerl/miller | ✓ | ✓ | ~ | ✓ | — | **R002 executed** | `PREVIOUSLY_EXPOSED` |
| plait-board/drawnix | ✓ | ✓ | ~ | ~ | — | **R004 executed** | `PREVIOUSLY_EXPOSED` |
| TailAdmin dashboard | ~ | ✓ | ✗ | ✓ | — | prepared R010 | `NO_AUDITABLE_ACCEPTANCE` — template, no test suite |
| memos / cargo / casdoor / hoppscotch / ripgrep | ✓ | ✓ | ~ | ✓ | — | prepared, not run | not needed once 3 qualified |

Three of the rejections are worth stating plainly, because they are the ones
that would have flattered a particular result if left in:

- **`TOO_TRIVIAL` ×4.** Four go-task and react-hook-form fixes are 2–7 source
  lines. Any of them would probably have been solved 3/3 and measured nothing.
- **`SOLUTION_IN_ISSUE`.** go-task #2102's reporter proposes the variable name
  to introduce. The task would test transcription, not engineering.
- **`PREVIOUSLY_EXPOSED` ×4.** hugo, miller and drawnix were actually run in
  Batch #1; go-task #2894 *is* R009's task. All four are out.

## 4. Two findings from the exposure and baseline audit

**R009's status line is wrong.** `batch-01/R009/TASK_CARD.md` says
`PREPARED / FROZEN — no Real Usage outcome`, but
`batch-01/R009_REAL_USAGE_REPORT.md` records an actual run on 2026-08-16
against `b9b50ca7`, CodeLeveler baseline `c3bf11b`, 3 PASS / 0 FAIL in 54
minutes. The report is the evidence; the card is stale. This matters here
because R009's repository is `go-task/task` — C1's repository.

That gives C1 `REPO_PREVIOUSLY_SEEN=YES`, and it is admissible anyway:

```
same task?                     NO — R009 is a matrix `ref:` race under
                               concurrent deps at SHA b9b50ca7;
                               C1 is method:timestamp at 8fa9dc04
run by the frozen specimen?    NO — R009 ran on baseline c3bf11b,
                               not 7486c337
run by AtomCode 5.0.9?         NO
run by DSH cd5ef814?           NO
memory carried across runs?    NO — LEVELER_HOME is run_dir/home/.leveler,
                               created fresh per run and seeded with config
                               only (evals/adapters/launch.py:143)
```

The residual exposure is a prior model context on the same repository, three
weeks ago, on a different bug, under a different baseline. It is disclosed
rather than hidden, and go-task's upstream fix for #2894 was struck from the
candidate pool the moment R009's task card was read.

**A candidate was dropped for a red baseline.** `cli/cli`'s auth-token
host:port fix looked ideal until its pinned parent turned out to have
`internal/attachments` failing to build — and failing on `tokenGetter`, the
exact interface that task touches. An agent would have started in a red tree in
its own work area. Rejected; the port-forwarding task at `6dae3077` has all 245
packages green.

## 5. Cohort shape

```
PHASE_C_TASK_COUNT=3
PHASE_C_REPS_PER_TOOL=1
PHASE_C_TOTAL_EXPECTED_RUNS=9
PHASE_C_RUN_ORDER_POLICY=BALANCED_ROTATION
DEFAULT_TIMEOUT=3600s
PER_TASK_TIMEOUT_OVERRIDE=none
```

```
C1:  CodeLeveler → AtomCode  → DSH
C2:  AtomCode    → DSH       → CodeLeveler
C3:  DSH         → CodeLeveler → AtomCode
```

Each tool runs once first, once second, once third, so time-of-day and
provider-load order bias is spread rather than concentrated.

```
FRESH_ISOLATED_WORKSPACE_PER_RUN=YES
SAME_PINNED_INITIAL_HEAD=YES
SAME_INITIAL_TREE=YES
NO_CROSS_TOOL_WORKSPACE_REUSE=YES
ORDINARY_FAILURE_RERUN=NO
SELECTIVE_RERUN=FORBIDDEN
ORIGINAL_INVALID_RUN_RETAINED=YES
```

## 6. Scoring

```
SUCCESS_EVALUATION=MECHANICAL + BLIND HUMAN ACCEPT/REJECT
PRIMARY_OUTCOME=PASS / FAIL / INVALID_INFRA
PARTIAL_PROGRESS=YES/NO      (diagnostic only, not a primary outcome)
HUMAN_REVIEWER=USER
HUMAN_REVIEW_BLINDED=YES
```

```
CORRECT = mechanical hard requirements PASS  AND  human ACCEPT
mechanical FAIL                  → INCORRECT, no human appeal
mechanical PASS + human REJECT   → INCORRECT
```

Mechanical layer per task: repository build, repository test suite, and the
task-specific behaviour check. **Workspace hygiene is not in any oracle.** None
of these three issues asks for a constrained change surface, so hygiene stays an
observation dimension — the scale-s800 diagnosis is a reason to watch it, not a
licence to score it.

Review packet per anonymised variant: goal, mechanical result, final diff, final
`git status`, untracked files, test/build summary, and optionally the agent's
final message. Hidden: tool name, log branding, cost, latency, terminal truth.
Full trajectories are for diagnosis afterwards, not for the accept/reject call —
a reviewer who watches an agent think can talk themselves into a bad changeset.

Blind mapping to `Variant A/B/C` is stored in the lab, outside anything the
reviewer sees, and is unsealed only when the report is written.

## 7. Frozen identities

```
CODELEVELER_SHA256=121983c94a6766492b428560b06a2363c7628073bc7683b570f12daad3d67919
CODELEVELER_BUILD_IDENTITY=leveler 0.2.0-beta.1 (7486c3377f19)
CODELEVELER_OBJECT_VERIFIED=YES
ATOMCODE_VERSION=5.0.9 (52ca5e6)      ATOMCODE_OBJECT_VERIFIED=YES
DSH_VERSION=0.1.2-alpha.1 (cd5ef814)  DSH_OBJECT_VERIFIED=YES
ALL_THREE_OBJECTS_VERIFIED=YES

MODEL=deepseek-v4-flash   MODEL_PARITY=FULL   GATEWAY_PARITY=PARTIAL
BASELINE_CHANGED=NO       BETA_BASELINE_STATUS=FROZEN
```

`origin/main` has moved past `BASELINE_PRODUCT_HEAD` and that stays true here:
the specimen is the artifact hashed above, never a checkout of current `main`.

## 8. Where the contract lives

Goals, acceptance checks and the rubric are lab-side, outside every agent
workspace, and are injected only at scoring time:

```
$DOGFOOD_ROOT/eval/phase-c/
  phase-c-manifest.yaml        machine-readable binding for the runner
  goals/C{1,2,3}.goal.md       the prompts, hashed above
  goals/C{1,2,3}.raw.md        unedited issue bodies, for audit of §2's edits
  oracles/c{1,2,3}-check.sh    acceptance checks
  oracles/c2-acceptance.test.tsx, c3-acceptance_test.go
  oracles/review-rubric.md
  candidates/{task,react-hook-form,cli}   pinned clones
  FINGERPRINT, FINGERPRINT_INPUTS.txt
```

`REFERENCE_FIX_COMMIT` is recorded in the manifest for oracle calibration only.
It must never reach a prompt or a workspace, and an agent's differing but
correct implementation is not a defect — a diff that does not look like upstream
is not a reason to reject.

## 9. Environment

```
OLD_EVAL_ORPHANS_FOUND=6
OLD_EVAL_ORPHANS_CONFIRMED_UNUSED=YES
OLD_EVAL_ORPHANS_GRACEFULLY_TERMINATED=6/6
UNRELATED_USER_PROCESSES_PRESERVED=YES
PRODUCT_REPO_DAEMON_ACTION=LEAVE_RUNNING
PHASE_C_WORKSPACE_ISOLATION_READY=YES
ENVIRONMENT_READY_FOR_PHASE_C=YES
```

Six `leveler serve` daemons from a deleted session's scratchpad, `ppid=1`, four
days old, whose repository directory no longer exists and whose only sockets
were their own socketpairs — SIGTERM, all six gone, verified. A `leveler serve`
against the live product repository and an interactive `atomcode` with a living
parent were left alone; Phase C isolates its own workspaces and runtime homes,
so neither can collide with it.

An earlier report in this programme said "no residual eval processes". That
check matched only runner script names and missed all eight; the list above is
what a full `pgrep` shows.

## 10. Gate

```
3 QUALIFIED REAL TASKS SELECTED          YES
ALL THREE PINNED HEADS VERIFIED          YES
ALL THREE INITIAL STATES REPRODUCIBLE    YES
ALL THREE GOALS FROZEN                   YES  (sha256 in §2)
ALL THREE ACCEPTANCE CHECKS AUDITED      YES  (red at start, green at fix)
NO PRIOR SAME-TASK EXPOSURE              YES  (§4 discloses repo-level exposure)
REVIEW RUBRIC FROZEN                     YES
RUN ORDER FROZEN                         YES
TIMEOUTS FROZEN                          YES
RESET / INVALID / RERUN POLICY FROZEN    YES
FROZEN OBJECT IDENTITIES RECONFIRMED     YES
```

```
PHASE_C_TASK_SELECTION=PASS
PHASE_C_CONTRACT_FREEZE=PASS
PHASE_C_CONTRACT_STATUS=FROZEN
READY_FOR_PHASE_C_FORMAL_COHORT=YES

OPEN_BETA_BLOCKER=0
OPEN_BETA_REQUIRED=0
```

Next task, separately: **Phase C Formal Cohort Execution** — 9 runs, then the
blind review packet.

## 11. On how these three were chosen

The selection question was never "which tasks show CodeLeveler at its best".
Every candidate was filtered on realism, reproducibility, auditability and
three-way fairness before any thought about who might win, and the rejection
table above is the evidence: the four tasks dropped as `TOO_TRIVIAL` were
dropped for being easy, not for being unfavourable, and the one dropped as
`SOLUTION_IN_ISSUE` was dropped because it would have been easy for everyone.

Phase B ended with `scale-s800` measuring something other than what its name
claimed. That is the failure mode this contract is built against: the three
tasks here are described by what a user actually reported, the checks assert
what that user actually asked for, and nothing has been added to any oracle
that the issue itself does not demand.
