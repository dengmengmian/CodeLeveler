# Multi-Agent S4 — Final Value & Root-Cause Closure

## Status

Multi-agent itself works reliably in production. But the uncensored S4 data still
does not show that automatic delegation adds value repeatably, so delegation stays
available under a conservative policy where the model decides.

```text
MULTI_AGENT_CAPABILITY_CLOSURE=PASS
AUTO_DELEGATION_VALUE_CLOSURE=BLOCKED
MULTI_AGENT_PRODUCT_CLOSURE=BLOCKED_ON_VALUE
PRIMARY_ROOT_CAUSE=CHILD_EXECUTION_QUALITY
SECONDARY_ROOT_CAUSE=COORDINATION_OVERHEAD
```

This is the same frozen S4 as before, with no ceilings in the way, run 5 + 5
times in pairs. Both arms passed the task 0 of 5 times. Delegated runs got more
units right (24/40 vs 20/40), cost less (−43 %) and were faster in 4 of 5 pairs.
The 5th pair (+366 s) is the one where B did not delegate. No run succeeded, so
no delegation counts as useful under the registered definition, and the time
saved is within the arms' own spread.

What stops a task from passing is the model's implementation of two packages.
`lruttl` passed 1 time in 10 runs and `mdtable` 4 times in 10. They fail no
matter who implements them: the single agent, a child that has the full
contract, or the parent. Child handoff and parent integration were each checked
mechanically and rejected as the cause. So neither is eligible for repair, and
no product code was changed for value.

The one product change in this task is a truth fix made before the
experiment: a child stopped by its wall-clock cap used to settle as "token or
cost budget ran out".

## 1. Historical evidence (unchanged)

| Step | Document | Result |
|---|---|---|
| MA4 | `MULTI_AGENT_EVAL_AND_ACCEPTANCE.md` | FAIL: units too small, one delegation, slower, 6× cost |
| MA4-B | `MULTI_AGENT_WORKLOAD_THRESHOLD_VALIDATION.md` | FAIL at every size; S4 "delegated less correct" measured against single-agent runs cut at 100 rounds |
| MA4-C | `MULTI_AGENT_PARENT_REASONING_BUDGET_VALIDATION.md` §1–15 | E. INCONCLUSIVE_DUE_TO_ROUND_LIMIT |
| Round budget | `LONG_TASK_ROUND_BUDGET_CLOSURE.md` | hidden 100-round ceiling removed (`35e9055`); `wait_task` on a child waits (`376bac2`) |
| MA4-C §16 | same | B. PARENT_REASONING_BUDGET_NOT_ROOT_CAUSE (max: 1/3, 21/24; high: 0/3, 17/24) |
| **This** | this document | S4 A/B without censoring: value not proven; root cause = execution quality |

## 2. Gates before the experiment

```text
BASE_HEAD=5efe373 (worktree clean, HEAD == origin/main)
5efe373 CI 34921662163: attempt 1 FAILURE (windows-latest fmt·clippy·test), attempt 2 success
FIRST_ATTEMPT_STABLE(5efe373)=NO
RUSTSEC-2026-0285=FIXED   rustls=0.23.45   cargo deny: advisories ok, bans ok, licenses ok, sources ok
HIDDEN_ROUND_CEILING_FIX=present (35e9055 ancestor; loop_test::a_bounded_window_above_one_hundred_rounds_is_the_hard_edge ok)
WAIT_TASK_BOUNDED_FIX=present (376bac2 ancestor; multi_agent_test::wait_task_on_a_child_waits_for_it_to_end ok)
S4_TASK_CHANGED=NO (mat-s4-eight-engines.yaml and catalog last touched in c06249f, registration)
```

### 2.1 Child timeout truth residual — fixed (`adfb61b`)

The MA4-C audit said 7 of 28 children hit the 20-minute cap but settled as
"token or cost budget ran out". A durable S4 row confirmed it mechanically:
four children started at 01:38:43, and three of them settled at 01:58:43–46
(exactly 1200 s later) with `stop=budget` and that summary.

Root cause: the kernel already records the dimension
(`AgentOutcome.budget_exhaustion: BudgetExhaustion { dimension }`). But the
child settlement only looked at `StopReason` (`stop_reason_wording`), and
`SubAgentFinished` only carried `stop=budget`.

| | |
|---|---|
| Failing test first | `multi_agent_test::a_child_stopped_by_its_duration_cap_says_the_duration_ran_out`: parent residual 61 s → the child gets 1 s, and its one round takes 1.5 s. It went red on the exact S4 text, then red on `limit == None` |
| Fix | wording names the limit that fired; `SubAgentFinished.limit: Option<ChildLimit>` (`duration`, `model_tokens`, `cost`, `commands`, `modified_files`, `round_window`, `round_ceiling`), durable on the engine event and in `leveler run --output jsonl`, `serde(default)` so older rows replay (`event::contract_tests::a_child_budget_limit_is_durable_and_optional`) |
| Not changed | child execution policy, the 20-min cap, `ChildStop`, the client protocol (web/mobile) |
| Regression | fmt, clippy `-D warnings`, workspace 3912 passed / 0 failed, web protocol/typecheck/197 tests/build, mobile analyze + 78 tests |
| CI | `34924571707` on `adfb61b`, attempt 1 success (Linux, macOS, Windows, web, mobile, deny/audit) |
| In the real S4 runs | pilot B: 2 children `stop=budget limit=duration`, "(stopped: its wall-clock duration limit ran out)"; formal B slot 1: 1 |

## 3. Experiment lock

```text
A = Delegation OFF (isolated LEVELER_HOME, agents.delegation = false)
B = Delegation ON  (product default)
BINARY=leveler 0.2.0-beta.2 (adfb61bed52a), one file for both arms (sha256 990bdda9…)
MODEL=deepseek/deepseek-v4-flash  PROVIDER=deepseek  GATEWAY=taotoken (~/.leveler/config.toml, unchanged)
PARENT_REASONING=max  CHILD_REASONING=max  (config default; per-request trace: A parent Max×542;
                       B parent Max×168, child Max×388; no other level)
TASK=mat-s4-eight-engines (registered c06249f)  ROUND WINDOW=400  TIMEOUT=5400 s  ORACLE=hidden suites via expect
HARNESS=evals/scripts/multi_agent_ab.py (unchanged since ada3b73)
MACHINE=one host; each slot ran A and B at the same time (--parallel 2)
SCHEDULE: pilot slot; formal slots 0–2 together (6 runs, the MA4-C §16 load), then slots 3–4 (4 runs)
```

The only variable is `agents.delegation`. The prompt, fixture, acceptance, work
units, timeout, round budget, model and reasoning effort are all unchanged.

## 4. Pilot (not in the sample)

| Arm | Oracle | Units | Wall s | Parent req | Children | Cost |
|---|---|---|---|---|---|---|
| A off | fail | 7/8 | 2751 | 203 | 0 | $3.41 |
| B on | fail | 5/8 | 1621 | 23 | 4 (1/1/2/4 packages; the 2- and 4-package children hit the 20-min cap) | $0.52 |

The pilot confirmed the setup: same binary, the delegation switch only in A,
Max effort everywhere, window 400, no round-limit stop, no poll loop, and
complete metrics. Its uneven 1/1/2/4 bundling did not repeat in the formal
batch (§7.4).

## 5. Formal A/B — every run

| Slot | Arm | Oracle | Units | Verification | Wall s | Round-limit | First spawn s | Parent req | Parent in / out tok | Children (completed / partial / duration stop) | Child req | Child in / out tok | Child-owned units passed | Parent rework (children) | Integration s | Total tok | Cached tok | Cost | wait_task / get_task | FV / OWN / LOST / DUP / ORPH / WAT |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | A off | fail | 4/8 | passed | 1843 | no | — | 188 | 15.43M / 235k | 0 | 0 | 0.00M / 0k | — | — | — | 15.67M | 15.24M | $2.21 | 0 / 0 | 0 / 0 / 0 / 0 / 0 / 0 |
| 0 | B on | fail | 7/8 | passed | 1794 | no | 574 | 31 | 1.81M / 109k | 6 (6 / 0 / 0) | 110 | 3.19M / 329k | 6/6 | 0 | 20 | 5.44M | 4.71M | $0.82 | 1 / 0 | 0 / 0 / 0 / 0 / 0 / 0 |
| 1 | A off | fail | 6/8 | passed | 2218 | no | — | 117 | 11.39M / 325k | 0 | 0 | 0.00M / 0k | — | — | — | 11.72M | 11.22M | $1.67 | 0 / 0 | 0 / 0 / 0 / 0 / 0 / 0 |
| 1 | B on | fail | 6/8 | passed | 1886 | no | 84 | 55 | 4.01M / 63k | 6 (5 / 1 / 1) | 98 | 2.65M / 526k | 6/6 | 1 | 368 | 7.25M | 6.32M | $1.09 | 13 / 0 | 0 / 0 / 0 / 0 / 0 / 0 |
| 2 | A off | fail | 3/8 | passed | 1478 | no | — | 64 | 4.31M / 234k | 0 | 0 | 0.00M / 0k | — | — | — | 4.54M | 4.19M | $0.66 | 0 / 0 | 0 / 0 / 0 / 0 / 0 / 0 |
| 2 | B on | fail | 3/8 | passed | 1231 | no | 413 | 20 | 0.88M / 166k | 6 (6 / 0 / 0) | 88 | 2.38M / 278k | 3/6 | 0 | 10 | 3.70M | 3.03M | $0.58 | 0 / 0 | 0 / 0 / 0 / 0 / 0 / 0 |
| 3 | A off | fail | 4/8 | passed | 1930 | no | — | 109 | 8.64M / 272k | 0 | 0 | 0.00M / 0k | — | — | — | 8.91M | 8.48M | $1.28 | 0 / 0 | 0 / 0 / 0 / 0 / 0 / 0 |
| 3 | B on | fail | 5/8 | passed | 1421 | no | 227 | 23 | 0.87M / 35k | 4 (4 / 0 / 0) | 91 | 3.88M / 407k | 5/8 | 0 | 28 | 5.19M | 4.48M | $0.78 | 0 / 0 | 0 / 0 / 0 / 0 / 0 / 0 |
| 4 | A off | fail | 3/8 | passed | 1648 | no | — | 64 | 4.82M / 250k | 0 | 0 | 0.00M / 0k | — | — | — | 5.07M | 4.69M | $0.74 | 0 / 0 | 0 / 0 / 0 / 0 / 0 / 0 |
| 4 | B on | fail | 3/8 | passed | 2015 | no | — | 39 | 2.88M / 314k | 0 | 0 | 0.00M / 0k | — | — | — | 3.20M | 2.76M | $0.49 | 0 / 0 | 0 / 0 / 0 / 0 / 0 / 0 |

- Every run ended `completed`, exit 0, `stop_limit` none.
- Usage reconciles: total = parent lane + child lane for requests, input,
  output, cached and cost in all 10 runs, with 0 unpriced requests.
- Parent rework in slot 1: the parent finished `glob` after its child hit the
  duration cap, and the package passed.
- The 13 `wait_task` calls in slot 1 B each waited a bounded 120 s, roughly
  2 minutes apart.

```text
CENSORED_RUNS=0/10   ROUND_LIMIT_CENSORING=NO
GET_TASK_POLLING_RESIDUAL=NOT_TRIGGERED (0 get_task calls in any run)
```

## 6. Value judgment

| | A off | B on | Paired B − A (slots 0…4) |
|---|---|---|---|
| Task success | 0/5 | 0/5 | 0, 0, 0, 0, 0 |
| Units passed | 20/40 | 24/40 | +3, 0, 0, +1, 0 |
| Verification truth (FALSE_VERIFIED) | 0 | 0 | |
| INCORRECT_AND_VERIFIED (quality metric) | 5/5 | 5/5 | |
| Wall median (min–max) | 1843 s (1478–2218) | 1794 s (1231–2015) | −48, −332, −247, −510, **+366** s (median −247) |
| Delegated | — | 4/5 | slot 4 did not delegate |
| Useful delegation (registered: child work kept in a run that passed) | — | 0/4 | |
| Child-owned units passed (PARTIALLY_USEFUL) | — | 20/26 in 4/4 delegated runs | |
| Cost total (median) | $6.56 ($1.28) | $3.75 ($0.78) | −1.39, −0.58, −0.09, −0.49, −0.25 |
| Tokens median | 8.91M | 5.19M | |
| First spawn median | — | 320 s (84, 227, 413, 574) | |

```text
TASK_SUCCESS_B >= TASK_SUCCESS_A: YES (0/5 = 0/5; the gate cannot discriminate)
UNITS_PASSED_B not materially lower: YES (+4)
USEFUL_DELEGATION_REPEATED=NO (registered definition: no run passed the oracle)
LATENCY_VALUE=NO   delegated slots −3 %…−26 %, but the non-delegated B slot is +366 s over its A:
                   a same-sized gap with no delegation at all, so the gain is not distinguishable from run variance
QUALITY_VALUE=NO   task success unchanged at 0; +4 units with 3 of 5 slots tied
COST_ACCEPTABLE=YES (B cheaper in 5/5 slots, −43 %)
VALUE_STABLE=NO
SAFETY_ALL_ZERO=YES
S4_MULTI_AGENT_VALUE=FAIL → ROOT-CAUSE PHASE
```

## 7. Root cause

Question: children really do run in parallel, and child-owned units pass more
often, so why is there no product gain?

### 7.1 Who implemented each unit, and whether its hidden suite passed

| Unit | A (single agent) | B child-owned | B parent-owned (delegated run) | B slot 4 (no delegation) |
|---|---|---|---|---|
| expr | 4/5 | 3/4 | — | 1/1 |
| jsonpath | 2/5 | 4/4 | — | 0/1 |
| textdiff | 2/5 | 3/3 | 0/1 | 0/1 |
| toposort | 2/5 | 2/3 | 0/1 | 1/1 |
| ratewindow | 5/5 | 4/4 | — | 1/1 |
| glob | 1/5 | 4/4 | — | 0/1 |
| mdtable | 3/5 | 0/2 | 1/2 | 0/1 |
| lruttl | 1/5 | 0/2 | 0/2 | 0/1 |
| **total** | **20/40** | **20/26** | **1/6** | **3/8** |

A focused child that has the contract does better per unit than the single
agent: 77 % against 50 % overall, and `glob` 4/4 against 1/5. Task success
needs all 8 units, though:

- `lruttl` passed once in 10 runs (A 1/5, B 0/5).
- `mdtable` passed 4 times in 10.
- Both fail about equally no matter which seat implements them.
- `lruttl` failed in 9 of the 10 runs. The 10th (A, slot 1) failed `glob`
  and `jsonpath` instead.

### 7.2 R1 Child context handoff — REJECTED

All 22 children in the 4 delegated runs were checked.

```text
PARENT_KNEW_FILES=22/22 (the parent read every assigned contract before spawning)
PARENT_KNEW_RELEVANT_TESTS=16/22 (slot 2's parent did not read visible tests before spawning — it could not hand on what it did not have)
PARENT_KNEW_CONSTRAINTS / ACCEPTANCE = the task statement + doc-comment contracts (no hidden-test knowledge exists on the parent side)
PARENT_KNEW_DEPENDENCIES = none required (packages independent by construction)

CHILD_RECEIVED  goal 22/22  files 22/22  constraints 22/22  acceptance 22/22  verification 22/22
                contract-specific evidence 22/22  scope wording 17/22  dependencies NOT_REQUIRED 22/22
CHILD_READ_OWN_CONTRACT_FILE=22/22
BRIEF_CHARS median 4258 (1671–19258)
```

Missing context does not line up with failure:

- The 5 briefs without scope wording (slot 0) passed 5 of 5. Their write scope
  was still enforced at runtime, with 0 ownership violations.
- The 6 children with a failing unit have every handoff dimension present, and
  each read its own contract file:
  - slot 2: `lruttl`, `expr`, `mdtable`
  - slot 3: `lruttl`, `toposort`, `mdtable`
- The median brief of a failing child (4637 chars) is no shorter than that of
  a passing one (4258).
- The contract a child needs is the file it reads itself, so anything the
  parent knew cannot be lost on the way.

```text
CONTEXT_LOSS_CASES=0
CHILD_CONTEXT_HANDOFF_PROBLEM=REJECTED
```

### 7.3 R3 Parent integration — REJECTED

```text
CORRECT_CHILD_RESULTS=20 (child-owned units that pass)   RESULTS_ACTUALLY_USED=20 (all in the final tree)
RESULTS_IGNORED=0   CHILD_CHANGES_OVERWRITTEN=0 harmful (1 rewrite: slot 1 glob after its child's duration stop — passed)
DUPLICATE_PARENT_WORK=0   INTEGRATION_FAILURES=0   PARENT_INTEGRATION median 24 s (10, 20, 28, 368)
INTEGRATION_PROBLEM_CONFIRMED=NO
```

The parent's closing check runs `go test ./...`, which only includes the
visible tests. It accepted wrong packages in 5 of 5 B runs, and the single
agent did the same in 5 of 5 A runs. That is shared behaviour, not a failure to
integrate what children produced.

### 7.4 R4 Task decomposition — REJECTED

```text
DELEGATED_WORK_UNITS=26 (slots 0–2: 6 children × 1 package, parent kept 2; slot 3: 4 children × 2 packages)
CORRECTLY_DECOMPOSED=26 disjoint, independent assignments   SCOPE_CONFLICTS=0 (OWNERSHIP_VIOLATION=0)
MISSED_DEPENDENCIES=0   PARTIALLY_CORRECT=0   INCORRECTLY_DECOMPOSED=0
DECOMPOSITION_PROBLEM_CONFIRMED=NO
```

In 3 runs the parent kept 2 of the 8 packages. That follows from the runtime's
total-children cap of 6, not from a wrong split. The uneven 1/1/2/4 bundling
seen in the pilot did not recur, and 1 of 22 formal children hit the duration
cap.

### 7.5 R5 Coordination overhead — secondary (latency)

| Slot | First spawn s | Queue for a slot (children × s) | Child critical path s (longest child) | Integration s | B wall | A wall |
|---|---|---|---|---|---|---|
| 0 | 574 (5 parent req, 84k out tok) | 2 × 264–277 | 1200 (`glob`, queued 277 s) | 20 | 1794 | 1843 |
| 1 | 84 | 2 × 198–233 | 1433 (`glob`, duration cap) | 368 | 1886 | 2218 |
| 2 | 413 (3 req, 59k out tok) | 2 × 80–301 | 807 (`glob`) | 10 | 1231 | 1478 |
| 3 | 227 | 0 | 1165 (`jsonpath`+`glob`) | 28 | 1421 | 1930 |

```text
PARENT_PRE_SPAWN_TIME median 320 s   SPAWN_OVERHEAD ≈ 0 (an unqueued child starts working within ~2 s of the spawn)
WAIT_SETTLEMENT_OVERHEAD ≈ 0 s (bounded wait_task)   QUEUE (concurrency 4, 6 children) 2 children × 80–301 s
CHILD_CRITICAL_PATH median 1183 s, set by the slowest single package
PARALLEL_TIME_SAVED (harness: serial child work − critical path) 1104–2007 s
REAL WALL SAVED on delegated slots 48–510 s
COORDINATION_PROBLEM_CONFIRMED=YES for latency only
```

The harness's "parallel time saved" compares against serial *child* work.
Each child takes longer per package than the single agent does, so that figure
overstates the saving. What turns into wall time is pre-spawn exploration, the
queue behind the concurrency cap, and the longest single package. That leaves a
median gain of about 16 % on delegated slots (−3 %, −15 %, −17 %, −26 %). The
one B run that did not delegate came in +366 s slower than its A, which is a
gap of the same size.

### 7.6 Decision

```text
PRIMARY_ROOT_CAUSE=CHILD_EXECUTION_QUALITY
  — mechanically: children with complete context (R1 rejected), integrated correctly (R3 rejected), on correct
    disjoint assignments (R4 rejected), still implement lruttl/mdtable wrongly; the same units fail in the
    single agent and in the parent. It is the model's implementation quality on the hardest contracts,
    in every seat, not a delegation-specific defect.
SECONDARY_ROOT_CAUSE=COORDINATION_OVERHEAD (latency only: pre-spawn 84–574 s + concurrency queue + longest package)
CURRENT_CHILD_EXECUTION_QUALITY_INSUFFICIENT=YES
```

## 8. Product repair

```text
REPAIR_REQUIRED=NO (primary root cause is not handoff or integration)
Handoff repair: EXECUTED=NO   Integration repair: EXECUTED=NO   Post-repair S4: EXECUTED=NO
Only product change in this task: adfb61b child budget stop reason (truth, §2.1)
```

In line with the task's rules, no model change, no reasoning change, no
round-budget change, no prompt tuning, no reviewer and no new task version.

## 9. Safety

```text
FALSE_VERIFIED_TOTAL=0   OWNERSHIP_VIOLATION=0   LOST_ACCEPTED_CHILD=0   DUPLICATE_SETTLEMENT=0
OPEN_ORPHAN=0   CHILD_WRITE_AFTER_TERMINAL(=AFTER_CANCEL)=0   RECOVERY_DUPLICATION=0
INCORRECT_AND_VERIFIED (quality metric): A 5/5, B 5/5
UNATTRIBUTED_CHILD_MUTATIONS=24 (truncated tool arguments in the durable log; 0 suspect; not violations, per 1866c3c)
CORRECTNESS_BLOCKER=NO
```

## 10. Final decision

```text
MULTI_AGENT_CAPABILITY=READY
MULTI_AGENT_RUNTIME=PASS
MULTI_AGENT_SAFETY=PASS
MULTI_AGENT_PRODUCT_SURFACE=PASS
MULTI_AGENT_CAPABILITY_CLOSURE=PASS (MA0–MA3, MA5 evidence unchanged; typed budget limit added)

AUTO_DELEGATION_VALUE=NOT_PROVEN
AUTO_DELEGATION_POLICY=CONSERVATIVE
AUTO_DELEGATION_VALUE_CLOSURE=BLOCKED

S4_MULTI_AGENT_VALUE=FAIL
MA4_EVAL_ACCEPTANCE=FAIL (unchanged)
MULTI_AGENT_PRODUCT_VALUE=NOT_PROVEN
MULTI_AGENT_PRODUCT_CLOSURE=BLOCKED_ON_VALUE
S4_FINAL_VALUE_EXPERIMENT=YES (no S5, no further reps)
```

`BLOCKED_ON_VALUE` does not mean the runtime is unusable. Children are durable,
resumable, cancellable and ownership-bound, and their settlements are truthful.
What is not proven is that letting the model delegate on its own makes a task
faster or more correct in a way that holds up across runs.

## 11. Product policy

```text
TINY=single-agent (the model does not delegate; keep it)
SMALL=single-agent
MEDIUM=delegation available, model decides conservatively
LARGE=delegation available, model decides conservatively; per-unit quality signal (+4/40), cost −43 % on S4, not a claimed benefit
PARALLEL_NATIVE=same as LARGE; wall is bounded by the longest unit and pre-spawn exploration
MARKETING_CLAIM=none — do not market multi-agent as faster or better; do not force delegation
```

## 12. Residuals (mechanical)

- S4 task success is 0 of 10 in both arms: at this model and effort the
  task-success gate cannot tell the arms apart. The workload stays frozen; any
  new task would be a new eval version (not created).
- The parent's final check is the visible `go test ./...`, so
  INCORRECT_AND_VERIFIED is 5/5 in both arms.
- The durable log truncates child tool arguments: 24 child writes could not be
  attributed to a child. Package ownership comes from the briefs and the hidden
  results.
- Queueing behind the concurrency cap of 4 (6 children) delayed the slowest
  child by up to 277 s in 2 runs.
- `SubAgentFinished.limit` is not yet projected into the client protocol, so
  web/mobile show `budget` together with the corrected summary text.
- Run data: session scratchpad `s4/{pilot,formal}`; analysis scripts
  `s4/analyze.py`, `s4/children.py`.

## 13. Next

```text
AGENT_EXTENSIBILITY_STARTED=NO   AGENT_EXTENSIBILITY_CODE_CHANGED=NO
NEXT_PRODUCT_PHASE=AGENT_EXTENSIBILITY_CLOSURE (independent of auto-delegation value)
```

Stop. Do not keep tuning the model, reasoning, round budget or prompt. The next
independent product phase keeps the design intent that is already decided:
one agent = one directory, `.leveler/agents/<name>/{agent.yaml, instructions.md}`
with built-in, user (`~/.leveler/agents/`) and project (`.leveler/agents/`)
sources, and project > user > built-in precedence. It must not claim that
auto-delegation value has been proven.
