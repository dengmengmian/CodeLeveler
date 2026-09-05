# Formal Three-Way Eval — Phase C Final

```
FORMAL_THREE_WAY_EVAL_PHASE_C=COMPLETE
PHASE_C_COHORT_VALID=YES
BLIND_REVIEW_VALID=YES
HUMAN_REVIEW_COMPLETE=YES

CODELEVELER_FINAL_CORRECT=3/3
ATOMCODE_FINAL_CORRECT=1/3
DSH_FINAL_CORRECT=0/3
GOOD_WINNER=CodeLeveler

CODELEVELER_MEDIAN_WALL=2384.2s
ATOMCODE_MEDIAN_WALL=246.0s
DSH_MEDIAN_WALL=244.5s
FAST_WINNER=AtomCode

CODELEVELER_ACCOUNTING_AUDITABLE=NO
COST_RANKING_SUPPORTED=NO
CHEAP_WINNER=NO_VALID_RANKING

FALSE_VERIFIED_TOTAL=0
CODELEVELER_AUTHORITY_YIELD=0/3

READY_FOR_BETA_READINESS_GATE=NO
PRODUCT_CODE_CHANGED=NO
```

Three real bugs, three third-party repositories, three harnesses, one run each,
scored mechanically and then reviewed blind by the user against a single
question: *would you send this to a maintainer?*

**CodeLeveler delivered the only changeset a maintainer would take on every
task. It also took roughly ten times as long to do it, and the runtime declined
to certify any of the three.** That is the whole result in two sentences, and
the rest of this document is the evidence for it.

`N=3 PER TOOL. SMALL_SAMPLE=YES.` Nothing here is a significance claim.

---

## 1. Cohort validity

```
TOTAL_COMPLETED_RUNS=9/9   VALID_RUNS=9   INVALID_INFRA=0   MISSING=0
TIMEOUTS=0   CRASHES=0
AGENT_COHORT_RERUN=NO      SELECTIVE_RERUN=NO
PHASE_C_CONTRACT_DRIFT=NO
CODELEVELER_BASELINE_MUTATED=NO / ATOMCODE=NO / DSH=NO
GOALS_MUTATED=NO / ORACLES_MUTATED=NO
```

Scoring was corrected twice, and both corrections are part of the record rather
than a footnote:

| | defect | fix |
| --- | --- | --- |
| pass 1 | `pnpm` refused to replace the agent's `node_modules` without a TTY, so all three C2 checks died in 0.4s having judged nothing | driver exports `CI=true` for C2 |
| pass 2 | Go returned `ok … (cached)`, so a C3 variant was scored without the race check ever executing against its tree | driver exports `GOFLAGS=-count=1` for C1 and C3 |

What keeps the cohort valid:

```
AGENT WORKTREES WERE NEVER RERUN
ORACLE SOURCE FILES WERE UNCHANGED   (all five re-hashed against the contract)
ALL NINE WORKTREES WERE RESCORED UNIFORMLY
PINNED START NEGATIVE CONTROLS FAILED AS EXPECTED
```

The negative control matters most: after both corrections, each oracle was run
against its own pinned start commit and each still failed with the symptom its
issue reports. The checks discriminate; 7/9 mechanical passes are not an
artefact of a loosened environment.

## 2. Human review integrity

```
HUMAN_REVIEW_DECISIONS_RECEIVED=9/9
HUMAN_REVIEW_FINGERPRINT=1a120ffd705e43a5941dc50ea7dd10cc3923ba2e249be8d0b77a9c70ccd4dcfd
BLIND_MAPPING_FINGERPRINT=e431378051e44517b3f2eabc4506abaf3a7b055b5a80071b0ce99d166f47acc6
TOOL_IDENTITY_VISIBLE_TO_HUMAN_REVIEWER=NO
```

Decisions were frozen before the mapping was opened. One packet defect was
caught during review and is recorded rather than smoothed over: the generator
renders `git diff HEAD`, which by construction omits the *content* of untracked
files, and two C2 variants turned on exactly such a file. Its full text was
supplied before the C2 ruling, and C3 was audited for untracked files before
being presented.

```
C2_PACKET_OMISSION_DETECTED=YES
C2_PACKET_OMISSION_CORRECTED_BEFORE_DECISION=YES
C3_UNTRACKED_REVIEW_PACKET_AUDIT=PASS
HUMAN_DECISION_TAKEN_WITH_COMPLETE_RELEVANT_INFORMATION=YES
```

The generator must be fixed before any future blind round.

## 3. The mapping, and the per-run matrix

```
C1-A→DSH        C1-B→AtomCode    C1-C→CodeLeveler
C2-A→AtomCode   C2-B→CodeLeveler C2-C→DSH
C3-A→CodeLeveler C3-B→AtomCode   C3-C→DSH

C1_MAPPING_VALID=YES  C2_MAPPING_VALID=YES  C3_MAPPING_VALID=YES
BLIND_MAPPING_VALID=YES
```

| Task | Tool | Mechanical | Human | Final | Human reason |
| --- | --- | --- | --- | --- | --- |
| C1 | DSH | FAIL | REJECT | **FAIL** | MISSING_REQUIREMENT |
| C1 | AtomCode | FAIL | REJECT | **FAIL** | MISSING_REQUIREMENT |
| C1 | CodeLeveler | PASS | ACCEPT | **PASS** | — |
| C2 | AtomCode | PASS | REJECT | **FAIL** | UNACCEPTABLE_CHANGE |
| C2 | CodeLeveler | PASS | ACCEPT | **PASS** | — |
| C2 | DSH | PASS | REJECT | **FAIL** | UNACCEPTABLE_CHANGE |
| C3 | CodeLeveler | PASS | ACCEPT | **PASS** | — |
| C3 | AtomCode | PASS | ACCEPT | **PASS** | — |
| C3 | DSH | PASS | REJECT | **FAIL** | UNRELATED_CHANGE |

## 4. GOOD

```
CODELEVELER  mechanical 3/3   human accept 3/3   final correct 3/3
ATOMCODE     mechanical 2/3   human accept 1/3   final correct 1/3
DSH          mechanical 2/3   human accept 0/3   final correct 0/3
```

```
CODELEVELER_GOOD=STRONG
ATOMCODE_GOOD=MIXED
DSH_GOOD=WEAK
GOOD_WINNER=CodeLeveler
```

### Human review is where the ranking actually separated

Mechanically the field looked close: 7 of 9 passed. Review cut that to 4. **Five
of the nine runs failed, and only two of those five failed on behaviour** — the
other three produced working code a maintainer would still send back.

| Failure mode | Runs | What went wrong |
| --- | --- | --- |
| Did not fix it | DSH C1, AtomCode C1 | no production change at all |
| Incomplete deliverable | AtomCode C2, DSH C2 | working tree passes, but the helper the fix calls was never added to the index |
| Unrelated churn | DSH C3 | correct concurrency fix bundled with a whole-file rewrite of unrelated test assertions |

C1 is worth naming precisely. DSH did not treat it as a fix task at all — it
answered as support, explained why `timestamp` behaves that way, suggested a
`status:` workaround, and told the user to file a bug upstream. AtomCode
produced no production change either. Both left the reported bug in place.

### Changeset completeness

```
CHANGESET_COMPLETENESS_GAP_BY_TOOL=  AtomCode 1 (C2), DSH 1 (C2), CodeLeveler 0
```

Both gaps are the same file on the same task: `src/logic/getNullAncestorValue.ts`,
created and never staged. Two different harnesses hit it, so this is not a
property of one tool — it is what that task's shape invites, since the natural
fix is a new helper file. CodeLeveler avoided it by putting the function inside
the file it was already editing.

**This is the sharpest methodological finding of Phase C.** A mechanical oracle
that runs against a working tree cannot see the difference between a complete
patch and a working directory. Two runs passed every check and would have handed
a maintainer a call site with no callee.

### Workspace hygiene — and a reversal from Phase B

```
WORKSPACE_HYGIENE_ISSUES_BY_TOOL=  AtomCode 3/3, CodeLeveler 0/3, DSH 0/3
```

Every AtomCode run wrote its own `datalog/` run logs into the repository. **No
CodeLeveler run left anything.** Phase B's `scale-s800` diagnosis found the
opposite — CodeLeveler creating `.e2e-events.txt` and `.e2e-telemetryd` inside
the tree — so on real tasks that behaviour did not recur:

```
WORKSPACE_HYGIENE_REPRODUCED_ACROSS_PHASES=NO   (for CodeLeveler)
```

The D3 hygiene finding stands as a Phase B observation. It was not reproduced
here, and the same weakness now sits with a competitor instead.

### Scope discipline

```
FINAL_CHANGESET_SELF_DISCIPLINE_ISSUE=  DSH (C3)
```

DSH's C3 concurrency fix is byte-identical to AtomCode's and resolves the race.
It was rejected for bundling a conversion of the whole test file from
`t.Fatalf`/`t.Errorf` to `require`/`assert`, touching tests with no relationship
to the bug. In a real PR that is a separate change.

## 5. Truthfulness and authorization are not the same thing

```
CODELEVELER_VERIFIED=0
CODELEVELER_COMPLETED_UNVERIFIED=3
CODELEVELER_BLOCKED=0
CODELEVELER_FAILED=0

FALSE_VERIFIED_TOTAL=0
TRUTHFULNESS_HELD=YES

CODELEVELER_AUTHORITY_YIELD=0/3
CORRECT_BUT_COMPLETED_UNVERIFIED=3
AUTHORIZATION_YIELD_INSUFFICIENT=YES
```

Three tasks. Three correct, maintainer-acceptable changesets. **Zero
certifications.** Every run ended "Completed in N rounds, but not independently
verified."

Nothing false was ever asserted — that is the property the F7 authority floor
was built to hold, and it held. But a gate that never says yes to work that was
right every single time is not protecting the user from anything on this
evidence. `FALSE_VERIFIED=0` does not mean Completion Truth is healthy; it means
half of it is.

### Exit code

```
EXIT_CODE_FALSE_NEGATIVE_COUNT=3/3
```

All three CodeLeveler runs exited `rc=1` while delivering work the reviewer
accepted. Phase B recorded the same mismatch on the benchmark lane; it
reproduces on real tasks. `rc=1` is defensible as "not verified", but any
automation doing `if rc == 0` reads three successes as three failures.

### Completion ledger state loss — now with a measured cost

```
COMPLETION_LEDGER_STATE_LOSS=CONFIRMED   frequency 1/3
CORRECT_WORK_REJECTED_BY_RUNTIME=YES
```

The full ledger trajectory at each refusal separates the benign reading from the
real one:

```
C1  0 1 2 2 3 3 3 4 5 6 6 6 6 → 0 0 0 0 0 → 0 (verify=3)     ← resets
C2  0 1 2 2 2 2 2 2 2 3 4 4 4 5 5 5 6 7 8 8 8 8 8 8 8 8 8    ← accumulates
C3  0 1 1 2 2 2 2 2 3 4 4 4 5 5 5 5 5 5 5 5 5 5              ← accumulates
```

C2 and C3 climb and hold. C1 climbs to 6, drops to 0, and stays there. A
per-attempt delta window would have zeroed all three, so that explanation is
out. On the strength of the emptied ledger the gate wrote:

> Agent describes modifications to `sources_timestamp.go`, `task_test.go`, and
> testdata fixture, **but runtime recorded no such file changes**.

Those modifications were real. And the C1 run is the one that ended **mechanical
PASS + human ACCEPT** — the runtime called a truthful agent a liar about work
that was, in fact, correct and deliverable. The damage is to authorization yield,
not to truthfulness; no false Verified was produced. Both statements are true and
neither substitutes for the other.

## 6. FAST

| Task | Tool | Wall |
| --- | --- | ---: |
| C1 | CodeLeveler | 1872.2s |
| C1 | AtomCode | 246.0s |
| C1 | DSH | 237.5s |
| C2 | CodeLeveler | 3485.0s |
| C2 | AtomCode | 227.5s |
| C2 | DSH | 244.5s |
| C3 | CodeLeveler | 2384.2s |
| C3 | AtomCode | 374.3s |
| C3 | DSH | 1700.9s |

```
CodeLeveler  total 7741.4s  mean 2580.5s  median 2384.2s  min 1872.2  max 3485.0
AtomCode     total  847.8s  mean  282.6s  median  246.0s  min  227.5  max  374.3
DSH          total 2182.9s  mean  727.6s  median  244.5s  min  237.5  max 1700.9

CODELEVELER_MEDIAN_WALL_RATIO_VS_ATOMCODE=9.7x
CODELEVELER_MEDIAN_WALL_RATIO_VS_DSH=9.8x

CODELEVELER_FAST_STATUS=WEAK
ATOMCODE_FAST_STATUS=STRONG
DSH_FAST_STATUS=MIXED
FAST_WINNER=AtomCode
```

AtomCode wins on consistency as much as speed: 227–374s across all three tasks,
where DSH ranges 237–1701s.

### Where the time goes

`MODEL_PARITY=FULL`, `GATEWAY_PARITY=PARTIAL`, so end-to-end latency cannot be
attributed wholesale to any runtime. What *can* be attributed is call count:

| Task | CodeLeveler requests | AtomCode requests | ratio |
| --- | ---: | ---: | ---: |
| C1 | 113 | 12 | 9.4× |
| C2 | 189 | 45 | 4.2× |
| C3 | 162 | 47 | 3.4× |

Per-call context is comparable — CodeLeveler averages ~57K input per call,
AtomCode's prompts run 42–63K. So:

```
PRIMARY_TOKEN_AMPLIFIER=MODEL_REQUEST_COUNT
```

Not context size. CodeLeveler is not sending bigger requests; it is sending
three to nine times as many.

### The reviewer sub-agent

```
REVIEWER_STARTS=2                    (C1, C2; C3 spawned none)
REVIEWER_BUDGET_EXHAUSTED=2
REVIEWER_ACCEPTED_FINDINGS=0
REVIEWER_TOKENS≈993K input           (C1 ↑486,369 / C2 ↑507,035)
REVIEWER_WASTE=CONFIRMED
REVIEWER_ROI=NEGATIVE
```

Both spawns ended `INCOMPLETE_PARTIAL (stopped: its token or cost budget ran
out)`. In C1 the child occupied **1169s between the parent's last model request
and process exit — 62% of that run's wall clock** — and produced nothing the
parent used.

```
REVIEWER_REPORT_GATE_MISMATCH=CONFIRMED
```

The C1 reviewer called `report_finding` twice; both were refused by a generic
plan gate telling it to `update_plan` first. It set a minimal plan, and its
budget ran out before it could re-report. The same gate refused its
`shell_command` calls. This is policy composition, not prompt quality: a
read-and-report child is being held to a planning contract written for an
executor.

```
REVIEWER_RELIABILITY_HAZARD=YES
```

C2 finished at 3485s against a 3600s timeout — **115 seconds of margin, 3.2%** —
on a task it had already solved, while a reviewer burned 507K tokens to deliver
nothing. A child that is not aware of the parent's remaining deadline can push a
completed task into a timeout.

### What could not be measured

```
TIME_TO_CORRECT_RESULT=NOT_MECHANICALLY_AVAILABLE
```

`command_receipts` is empty and per-tool-call timings are not persisted, so the
last productive edit cannot be located in wall-clock time. The C1 reviewer tail
is derivable only because the child's model requests are absent from the
parent's DB, which is itself the accounting defect. `OBSERVABILITY_GAP`.

## 7. CHEAP

Cost is not ranked here, and the reason is CodeLeveler's own books.

| Layer | input | cached input | output | child attribution | cost |
| --- | --- | --- | --- | --- | --- |
| Provider parse | COMPLETE | COMPLETE | COMPLETE | n/a | n/a |
| Runtime aggregate | COMPLETE | PARTIAL | COMPLETE | PARTIAL | MISSING |
| Persistence | COMPLETE | **MISSING** | COMPLETE | **MISSING** | MISSING |
| Session attribution | COMPLETE | MISSING | COMPLETE | **MISSING** | MISSING |
| Pricing | MISSING | MISSING | MISSING | MISSING | MISSING |
| Display/report | PARTIAL | MISSING | PARTIAL | MISSING | MISSING |

```
CACHED_TOKEN_PARSE=COMPLETE
CACHED_TOKEN_PERSISTENCE=MISSING
CACHED_TOKEN_PRICING=MISSING
CACHED_TOKEN_SESSION_ATTRIBUTION=MISSING

CHILD_BUDGET_AGGREGATION=YES        (spend does reach the parent's budget)
CHILD_DURABLE_REQUEST_ATTRIBUTION=INCOMPLETE
BUDGET_ENFORCEMENT ≠ AUDITABLE_ACCOUNTING

CODELEVELER_ACCOUNTING_AUDITABLE=NO
CODELEVELER_COST_ACCOUNTING=NON_AUDITABLE
```

`openai_chat/stream.rs` reads DeepSeek's `prompt_cache_hit_tokens` and
`anthropic_messages` reads `cache_read_input_tokens`; neither is written
anywhere. `model_requests` has no cached column and `events` holds zero rows
matching `%cached_input%`. In C1 the `sessions` table has one row and
`model_requests` holds only the parent's 113 calls, so the recorded 6,392,866
input tokens understate that run by at least the child's 486,369.

Recorded totals, presented as measurements and not as a ranking:

| Task | Tool | requests | input | output | cached |
| --- | --- | ---: | ---: | ---: | --- |
| C1 | CodeLeveler | 113 | 6,392,866 | 67,547 | **N/A** |
| C1 | AtomCode | 12 | 499,170 | 28,563 | 450,432 (90.2%) |
| C2 | CodeLeveler | 189 | 10,581,526 | 304,619 | **N/A** |
| C2 | AtomCode | 45 | 2,185,809 | 21,591 | 2,126,976 (97.3%) |
| C3 | CodeLeveler | 162 | 10,209,284 | 234,802 | **N/A** |
| C3 | AtomCode | 47 | 2,550,699 | 28,704 | 2,461,184 (96.5%) |
| all | DSH | N/A | N/A | N/A | N/A |

```
COST_RANKING=NOT_SUPPORTED
CODELEVELER_CHEAP_STATUS=NON_AUDITABLE
ATOMCODE_CHEAP_STATUS=MIXED
DSH_CHEAP_STATUS=INSUFFICIENT_DATA
CHEAP_WINNER=NO_VALID_RANKING
```

AtomCode's own log shows 90–97% cache hits, and its C2 run used 2.19M input
tokens — the same order of magnitude as CodeLeveler's, at a fraction of the
uncached cost. Whether CodeLeveler runs at 5% cache hits or 95% decides whether
the gap is severe or modest, and **that number does not exist in anything the
runtime keeps.** Calling CodeLeveler "most expensive" from 6–10M raw input would
be a guess.

What *is* confirmed without pricing:

```
CONFIRMED_AVOIDABLE_TOKEN_WASTE=YES   ≈993K input tokens across two reviewer
                                       spawns that produced zero findings
MODEL_REQUEST_AMPLIFICATION=3.4x–9.4x vs AtomCode on identical tasks
```

## 8. Three-way matrix

| Dimension | CodeLeveler | AtomCode | DSH | Confidence |
| --- | --- | --- | --- | --- |
| **GOOD** | **STRONG** — 3/3 | MIXED — 1/3 | WEAK — 0/3 | Medium (n=3, but human review is unanimous per tool) |
| **FAST** | WEAK — 2384s median | **STRONG** — 246s | MIXED — 245s median, 1701s worst | High for the ratio; Medium for attribution (gateway) |
| **CHEAP** | NON_AUDITABLE | MIXED | INSUFFICIENT_DATA | Low — no valid ranking |

> **CodeLeveler's largest competitive gap is not quality. It is that the same
> correct answer costs three to nine times as many model calls and roughly ten
> times the wall clock, and that its own books cannot say what that costs.**

```
PRIMARY_GOOD_GAP=none against competitors; the gap is internal —
                 3/3 correct, 0/3 authorized
PRIMARY_FAST_GAP=9.7x median wall, driven by 3.4–9.4x model request count,
                 plus a reviewer that consumed 62% of one run
PRIMARY_CHEAP_GAP=cost cannot be measured at all
```

## 9. Phase B → Phase C cross-check

| Phase B signal | Phase C | Classification |
| --- | --- | --- |
| Workspace hygiene (CodeLeveler left artifacts in-repo) | 0/3 CodeLeveler runs left anything; all 3 AtomCode runs did | **NOT_REPRODUCED** (for CodeLeveler); BENCHMARK_SPECIFIC |
| Final changeset self-review | no tool inspected its own change set before finishing; two runs shipped an unstaged production file | **REPRODUCED**, REAL_TASK_RELEVANT |
| Scope constraint compliance | no Phase C task constrains the change surface; DSH C3 was rejected for unrelated churn anyway | **PARTIAL** — the underlying discipline issue is real without an oracle enforcing it |
| Authority yield | 0/6 correct-and-Verified in Phase B, 0/3 here | **REPRODUCED**, REAL_TASK_RELEVANT |
| False Verified | 0 in Phase B, 0 here | **REPRODUCED** (held) |
| Exit code semantics | rc=1 on 8/8 in Phase B, 3/3 here with all three accepted | **REPRODUCED**, REAL_TASK_RELEVANT |
| Latency | 3–8× in Phase B, 9.7× median here | **REPRODUCED and amplified** |
| Token metrics | Phase B could not read AtomCode's counters; fixed. CodeLeveler's own cache/child gap now the blocker | **PARTIAL** — the blocker moved from the competitor to us |

The hygiene reversal deserves emphasis. Phase B's scale-s800 diagnosis
concluded that CodeLeveler's weakness was workspace hygiene with a measurable
competitive cost. On real tasks that did not recur, and the same weakness showed
up in a competitor instead. The Phase B conclusion was correct about that
benchmark and does not generalise.

## 10. Product discovery findings

Found in Phase C but not measured by task correctness.

```
FGC-01  ACCOUNTING_AUTHORITY          = CONFIRMED
FGC-02  REVIEWER_WASTE                = CONFIRMED
FGC-03  COMPLETION_LEDGER_STATE_LOSS  = CONFIRMED  (1/3)
FGC-04  ORPHAN_DAEMON_LIFECYCLE       = SYMPTOM CONFIRMED, root cause unproven
```

**FGC-01** — cached tokens parsed and never persisted; child request usage never
attributed durably. Blocks any claim about cost, before or after a change.

**FGC-02** — two spawns, two budget exhaustions, zero accepted findings, ≈993K
input tokens, 62% of one run's wall clock, and a 115-second timeout margin on
another. Root cause is a generic plan gate refusing a reviewer's `report_finding`.

**FGC-03** — the ledger emptied mid-run in 1 of 3 runs and the completion gate
contradicted a truthful agent on that basis. The affected run was correct and
accepted, so `CORRECT_WORK_REJECTED_BY_RUNTIME=YES`.

**FGC-04** — seven `leveler serve` daemons with `ppid=1`, up to six days old,
whose repository directories had been deleted, still holding sockets. Nothing
told them to retire.

```
ORPHAN_DAEMON_LIFECYCLE_SYMPTOM=CONFIRMED
ROOT_CAUSE_NOT_FULLY_PROVEN=YES
```

A fixed TTL is the wrong fix and would break BR-A's non-idle drain guarantee.
The predicate has to be: repo gone, no active client, no active session or task,
no owned background work — then retire, after draining.

**Lab infrastructure, not a product defect:** the AtomGit refresh credential in
the lab went stale because the live install rotates it, which killed the first
cohort attempt in one second per AtomCode run. Fixed before the cohort of record.
`PHASE_C_AGENT_COHORT_CONTAMINATED=NO`.

## 11. Ranked findings

| Rank | Finding | GOOD | FAST | CHEAP | Evidence | Confidence |
| ---: | --- | --- | --- | --- | --- | --- |
| 1 | Accounting authority — cost unmeasurable | NONE | LOW | **HIGH** | cached never persisted; child spend absent from `model_requests` | HIGH |
| 2 | Reviewer waste and deadline hazard | LOW | **HIGH** | **HIGH** | 2/2 exhausted, 0 findings, ≈993K tokens, 62% of a run, 115s margin | HIGH |
| 3 | Model request amplification | NONE | **HIGH** | **HIGH** | 113/189/162 vs 12/45/47 at equal per-call context | HIGH |
| 4 | Authorization yield 0/3 | **MEDIUM** | NONE | NONE | 3 correct, 3 CompletedUnverified | HIGH |
| 5 | Completion ledger state loss | **MEDIUM** | LOW | LOW | ledger 6→0→0 in C1 only; gate refused on a false premise | HIGH |
| 6 | Exit code false negative | **MEDIUM** | NONE | NONE | rc=1 on 3/3 accepted runs | HIGH |
| 7 | Mechanical oracle blind to unstaged files | NONE | NONE | NONE | 2 runs passed every check with an unstaged production file | HIGH |
| 8 | Orphan daemon lifecycle | LOW | NONE | LOW | 7 daemons, ppid=1, repos deleted | MEDIUM |
| 9 | Review packet omits untracked content | NONE | NONE | NONE | C2 packet omitted the helper's body | HIGH |
| 10 | `TIME_TO_CORRECT_RESULT` unmeasurable | NONE | LOW | NONE | `command_receipts` empty | HIGH |

```
FAST_GOOD_CHEAP_TOP_1=Accounting Authority Closure
FAST_GOOD_CHEAP_TOP_2=Reviewer Efficiency / Policy Composition Closure
FAST_GOOD_CHEAP_TOP_3=Model Request Amplification Reduction
```

Findings 4–6 form a fourth cluster — *the runtime cannot certify its own correct
work* — which is a real Completion Truth problem but cost nothing in task
success here. It ranks below the three above on competitive leverage, not on
seriousness.

## 12. Engineering priority

```
FIRST_ENGINEERING_WORK_PACKAGE=ACCOUNTING_AUTHORITY_CLOSURE
```

It goes first because every other item's payoff is a number this codebase cannot
currently produce. Cutting the reviewer saves *some* tokens; reducing model calls
saves *some* cost — and with cached usage unpersisted and child spend
unattributed, neither claim can be verified after the fact. Measuring first is
not the cautious choice here, it is the only order in which the rest can be
scored.

Target invariant:

```
sum(durable attributable request usage) ≈ runtime cumulative usage
```

with any permitted divergence written into the schema rather than left implicit.

| Priority | Work package | Why now | GOOD | FAST | CHEAP | Depends on |
| --- | --- | --- | --- | --- | --- | --- |
| **P1** | Accounting Authority Closure | nothing else can be scored without it | — | low | high | — |
| **P2** | Reviewer Efficiency / Policy Composition | largest single avoidable waste, and a timeout hazard | low | high | high | P1 to quantify |
| **P3** | Model Request Amplification | the actual 3.4–9.4× driver | — | high | high | P1 to quantify |
| **P4** | Completion Ledger State Loss | correctness of the authorization path | medium | — | — | reproducer first |
| **P5** | Authorization yield + exit code semantics | 3/3 correct, 0 certified, rc=1 | medium | — | — | P4 |
| **P6** | Daemon retirement predicate | operational, must preserve BR-A drain | low | — | low | — |
| **P7** | Eval: oracle staged-changeset check; packet untracked content | our own measurement gaps | — | — | — | — |

P7 is eval infrastructure, not product, and is cheap: an oracle that scored a
working tree let two incomplete patches through, and our own review packets
omitted the file that decided two rejections.

### Hypotheses, not designs

Recorded so the next round starts from evidence. **None is implemented here.**

*Reviewer* — align `report_finding` with the reviewer role rather than the
generic executor plan gate; make the child's budget aware of the parent's
remaining deadline; settle partial findings when the budget runs out instead of
discarding them; exit early when no value is accruing. Guardrails: human accept
rate must not fall, task success must not fall, false Verified must not rise.
Teaching the reviewer to satisfy the plan gate by prompt is not the fix.

*Completion ledger* — a deterministic reproducer before any change: material
edit → successful verification → continuation/window boundary → rehydration →
inspect the seeded ledger → completion gate, asserting separately on the internal
ledger, the rendered projection, and the completion predicate. One occurrence in
three runs does not justify touching the completion architecture blind.

*Daemon* — retire on `repo missing AND no active client AND no active session or
task AND no owned background work`, draining first, preserving BR-A.

## 13. Re-eval design

Design only; nothing is scheduled.

The new treatment freeze must record `PRODUCT_HEAD`, `PRODUCT_TREE`,
`ARTIFACT_SHA256`, `BUILD_IDENTITY`, `MODEL_CONFIG`, `EVAL_CONFIG`. The current
baseline (`7486c3377f19…` / `121983c9…`) is retained, never overwritten.

Analytical comparison is four-arm — CodeLeveler Old, CodeLeveler New, AtomCode,
DSH — with Old supplied by this cohort's frozen results rather than re-run. The
active cohort stays three specimens.

```
TASK_EXPOSURE_RISK=YES
```

C1/C2/C3 have been executed once by all three tools. Workspaces and runtime
homes are recreated per run so nothing carries over mechanically, which makes
them a usable **regression set** — but a competitive conclusion drawn only from
tasks every tool has already seen would be weak. The final closure needs a
**fresh generalization set** of new real tasks alongside them.

Suggested gates, for freezing later — not contract changes now:

```
GOOD   task success, human accept, false Verified, unsafe mutation,
       changeset quality        → GOOD_NOT_REGRESSED=YES
FAST   time to correct result, median E2E wall, post-edit tail, reviewer tail,
       model request count      → FAST_GAP_MATERIALLY_REDUCED=YES
CHEAP  requires ACCOUNTING_AUDITABLE=YES first; then cost per successful task,
       input / cached / output / child / reviewer / waste tokens
                                → CHEAP_GAP_MATERIALLY_REDUCED=YES
```

Speed and cost may not be bought with quality. CodeLeveler currently wins the
dimension that is hardest to win; the work ahead must not spend that.

## 14. Beta gate

```
FAST_GOOD_CHEAP_GAP_ANALYSIS=COMPLETE
FAST_GOOD_CHEAP_ENGINEERING_CLOSURE=NOT_STARTED
FAST_GOOD_CHEAP_CLOSURE=NOT_PASSED
READY_FOR_BETA_READINESS_GATE=NO

OPEN_BETA_BLOCKER=0
OPEN_BETA_REQUIRED=0
BETA_BASELINE_STATUS=FROZEN
```

The Beta gate stays closed by standing decision, not because Phase C found a
release blocker. It did not: no false Verified, no unsafe mutation, no data
loss, and the best delivered quality of the three. What it found is that the
quality costs three to nine times the model calls, and that the runtime can
neither certify the work nor say what it cost.

## 15. Evidence

Lab-local and unversioned.

```
$DOGFOOD_ROOT/eval/phase-c/formal-20260905-121433/
  0{1..9}-{C1,C2,C3}--{codeleveler,atomcode,dsh}/
      record.json  stdout.log  stderr.log  mechanical.json  mechanical.log
      final/{git.diff,git.status,git.diff.stat,untracked.txt}  ws/
  mechanical-summary.json      human-decisions.json
  blind_mapping.json           human-review-fingerprint.txt
  cost-report.json             COST_DECOMPOSITION.md
  COHORT_VALIDITY.md           SCORING_NOTE.txt
  contract-fingerprint-{start,end}.txt

PHASE_C_TASK_MANIFEST_FINGERPRINT=2dfedd5b9c51da2555ef83afcc1d874d5cb6fd3ac85092b641f02688d287ab32
HUMAN_REVIEW_FINGERPRINT=1a120ffd705e43a5941dc50ea7dd10cc3923ba2e249be8d0b77a9c70ccd4dcfd
BLIND_MAPPING_FINGERPRINT=e431378051e44517b3f2eabc4506abaf3a7b055b5a80071b0ce99d166f47acc6
```
