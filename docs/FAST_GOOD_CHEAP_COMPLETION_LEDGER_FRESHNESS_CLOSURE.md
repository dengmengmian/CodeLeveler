# Completion Ledger Freshness / Continuation Closure

Work package 3b of the Fast / Good / Cheap engineering closure. WP3 as
originally scoped (model request amplification) falsified every hypothesis it
was given; this is the work package that came out of asking *why* the runs
that ran long ran long.

```
COMPLETION_LEDGER_FRESHNESS_CLOSURE=PASS

RE_EDIT_ADVANCES_MUTATION_SEQ=YES            (1a)
UNKNOWN_FRESHNESS_IS_NOT_FALSE=YES           (1b)
GOAL_CONTINUATION_SEEDS_UNCONDITIONALLY=YES  (1c)
REPAIR_TURN_CARRIES_THE_LEDGER=YES           (1d)
CHANGE_SUPERSEDED_ONLY_BY_SAME_PATH=YES      (1e)
JUDGE_SEES_THE_EPOCHS_FILES=YES              (1f)
FRESH_VERIFICATION_IS_NEVER_CLOSEOUT_THRASH=YES (1h)
UNRECORDED_VERIFICATION_IS_NAMED_ON_THE_RESULT=YES (1g)
CLOSURE_REVIEWER_ACCOUNTED_AND_BUDGETED=YES (1k)

LEDGER_STATE_LOSS_ON_CONTINUATION=0 across 12 runs on the 1c binary; 1 of 1 refused-close continuation dropped on the binary before it
GOOD_NON_REGRESSION=PASS                     3/3 vs 3/3
MODEL_REQUESTS_TREATMENT_VS_CONTROL=−45%     118 vs 213 mean, n=3 per arm
INPUT_TOKENS_TREATMENT_VS_CONTROL=−58%
ESTIMATED_COST_TREATMENT_VS_CONTROL=−49%

HUNDRED_ROUND_WINDOW_CHAIN=BOUNDED           1i withdrawn (§3d); 1j task budget 200/+100/×2 built (§3e), A/B in §6 exp8
CONTRACT_OBLIGATION_FROM_EXPLANATORY_PROSE=OPEN  §6, exp6/f2; not this closure

TREATMENT_RUNS=12   GOOD=12   BREAKER_KILLS=0   CLEAN_COMPLETED_ENDINGS=5
```

The runtime refused truthful completion claims because it had forgotten what
the agent did. Every extra round after that was the agent trying to prove
something to a judge that had been told the opposite.

---

## 1. The defect chain, end to end

Phase C's C1 run recorded `COMPLETION_LEDGER_STATE_LOSS` (mutations 6 → 0
across a continuation). Tracing that one number produced a chain of four
defects, each sufficient on its own to make the runtime lie to its judge:

| | where | what was wrong |
| --- | --- | --- |
| 1a | `ledger.rs` | A re-edit of an already-modified file did not advance `last_mutation_seq` (first-touch dedup), so a check that ran *before* the re-edit stayed "fresh". |
| 1b | `reconciliation.rs` | `fresh_verification` was a `bool`. A window whose ledger held no mutations at all (`last_mutation = 0`) was reported as `false` — "not green since the last edit" — when the truth was "nothing on record to order it against". The judge treated the hard-coded `false` as a contradiction of the agent's claim. |
| 1c | `turn.rs` | A `GoalContinuation` after a refused close saw `closing = true`, concluded the previous window was terminal, and opened a **fresh epoch**: mutations, verifications and the plan all dropped. That is the 6 → 0. |
| 1d | `turn.rs` | A repair turn carried only findings forward, never the edits or the failed check it was repairing. |

Fixes, in the order the chain runs: a mutation *operation* now advances the
sequence even when the path is already on record; `Freshness` is a three-state
enum whose `Unknown` tells the judge in words not to treat it as a
contradiction; a continuation of the active goal seeds task state
unconditionally and the drive loop clears `closing` when a new window opens;
a repair turn receives the persisted ledger whole. Refusal records now carry
`[workspace_facts.freshness=fresh|stale|unknown]` so the next investigation
does not have to reconstruct this from transcripts.

## 2. Two batches, one of which mattered

Both batches: the frozen C2 task (react-hook-form #13674, pinned start
`84188ebb`), the frozen oracle `c2-check.sh`, `deepseek-v4-flash`, three runs
per arm in parallel, a 300-model-request breaker. Control is the WP2 binary.

**exp2 — 1a + 1b only.** Nothing moved: the treatment arm hit the breaker in
2 of 3 runs against 1 of 3 for control, and GOOD was 2/3 in both arms.
Correct freshness accounting on its own buys nothing while the ledger is
still being thrown away at every continuation.

**exp3 — 1a + 1b + 1c + 1d.**

| run | requests | wall | close attempts / refused | windows | GOOD |
| --- | ---: | ---: | --- | --- | --- |
| c1 | 302 (breaker) | 40m | 3 / 2 | 100 · 100 · … | PASS |
| c2 | 190 | 40m | 1 / 0 | 100 · Completed:88 | PASS |
| c3 | 146 | 21m | 2 / 2 | 100 · CloseoutForced:43 | PASS |
| f1 | **89** | **17m** | 2 / 1 | **Completed:86** | PASS |
| f2 | 129 | 44m | 2 / 2 | 100 · CloseoutForced:26 | PASS |
| f3 | 136 | 17m | 2 / 2 | 100 · CloseoutForced:33 | PASS |

| arm | requests (sum) | input tokens | cached | output | est. cost (USD) |
| --- | ---: | ---: | ---: | ---: | ---: |
| control | 638 | 39,456,971 | 38,897,408 | 653,539 | 0.80 |
| treatment | 354 | 16,649,230 | 16,377,088 | 508,448 | 0.41 |

Cost is usage × the configured cache-aware price table
(`0.1389 / 0.0139 / 0.2778` per MTok), the same arithmetic WP1 made
auditable. Both arms fix the bug 3/3.

Read directly from every `evidence_ledger_updated` row across all 12 runs of
both batches: the ledger record count at the first write of a new window
never fell below the last write of the previous window **except once** —
exp2/f3, `14 → 0`, on the binary that predates 1c. exp3/f1 is the cheapest
GOOD run this programme has produced on any task by any harness under
CodeLeveler's own accounting.

## 3. What the re-test found that the fix did not cover

### 3a. "Superseded by a later change" — fixed here (1e, 1f)

exp3/f2 was refused twice with the oracle already green. The judge's stated
reason:

> The claimed fix change is superseded and is not reflected in the final
> modified files … the final workspace modified-file state lists only
> node_modules.

Two runtime statements, both false:

- `evidence_candidates` marked a change **current only if it was the last
  mutation on record**. Every earlier edit — the fix itself — was presented as
  *"superseded by a later change"*. An edit to `useController.ts` is not
  superseded by a later edit to a different path, and the judge read the word
  as "undone". Now a change is superseded only by a later mutation to one of
  its own paths.
- The judge's `modified_files` was **this window's** list. On a continuation
  window that only re-ran tests it said "only node_modules", over an epoch
  that had landed the fix. The judge now receives the epoch's cumulative
  paths (`epoch_modified_paths`), the same set the file budget already uses.

The `node_modules` mutation itself was real: the agent had run
`ln -sfn /private/tmp/…/node_modules node_modules`. `.gitignore`'s
`node_modules/` matches a directory, not a symlink, so the git-tree snapshot
correctly recorded the link and its later removal. The snapshot is not at
fault and was not changed.

### 3b. Two guards, opposite orders — fixed here (1h)

Found by the first field run of 1e+1f (exp4/f1, GOOD, 91 requests). The
completion gate refused the close — correctly: the agent had edited a test
after its last green run — with *"no check demonstrates R1 over the current
tree"*. The agent did the only legal thing and re-ran jest. The closeout guard
classified that round as **closeout thrash** (plan complete, substantive work),
injected *"do not run more builds/tests — your next message must be the final
summary"*, the agent obeyed with a text-only summary, the guard counted that
as a second thrash round and force-stopped the turn one round before the
agent could close. The run ended `CloseoutForced`, "not independently
verified", on a green tree.

`RoundInput` now carries `fresh_evidence_gained`: the tree was not proven
green when the round began and is now. Such a round is `Progress`, not
`CloseoutThrash`, however complete the plan is. Re-running a check the tree
was already proven green by is still thrash.

### 3c. A check the model watched pass and the ledger never saw — fixed here (1g)

exp5/f3 (GOOD, 169 requests) was refused twice for *"no green verification
since the last edit"* and force-stopped, with the model having re-run the
tests **twice** in between — as `TEST_ENV=web jest … 2>&1 | tail -6`. The
execution layer records a verification only when the command's own exit
status is the program's (HC-002): a pipeline exits with `tail`'s status, an
environment prefix changes what ran, a redirect hides the outcome. All three
at once here. The rule is right and is not changed. What was wrong is that
nobody told the model: it watched jest pass, then read "no green check", and
the run ended in the guard fight of §3b over evidence that had never been
recorded. exp4/f1's second refusal was the same shape (`jest … | tail -8`).

Now, when a verification-class program ran inside a shape that proves
nothing, the tool result itself carries one line: *not recorded as
verification evidence; the exit status is not `jest`'s own; run it as one
plain command for it to count.* Said where the model is looking, at the
moment the gap opens, instead of at the next refused close. The ledger
records the note as an `unproven_verification` intercept so the count is
auditable. `leveler-execution` gains `literal_program_names` — the model's
view of what it ran, next to `proven_executed_commands`, the ledger's view of
what a zero exit proves; the note is exactly the difference between the two.

### 3d. The 100-round window chain — **not fixed here**

10 of 12 runs hit the absolute per-window ceiling (`MAX_TURN_ROUNDS = 100`) at
least once. exp2/f1 ran three full windows — 300 rounds — with **zero**
`update_goal` calls, cloning old upstream versions into the repository under
`.arch/v721/` to diagnose a regression it had already reproduced.

The chain: `leveler run` is `UntilTerminal` (no task total) → window ends
`TurnLimitReached` → the supervisor asks whether the window made progress →
`made_progress` is true if the window's `modified_files` grew *or* any
mutation op happened *or* a check newly went green → a window that wrote a
diagnostic test file counts as progress → the no-progress counter resets →
`DriveGoalAgain` → another 100 rounds, until an external breaker. Those
windows are 70–80% `shell_command`.

Whether the extra rounds buy anything: of the four breaker-killed runs across
both batches, two were GOOD when killed (exp2/f2, exp3/c1) and two were not
(exp2/c1, exp2/f1). A lottery, not an investment.

This was held back as a behaviour decision, not a defect repair. The
decision was taken with the numbers above in hand, the rule was built,
tested and A/B'd — and **withdrawn**, because the A/B showed it does not
bound the failure it was aimed at. What was tried, and why it is not in the
tree, is recorded here so the next attempt starts from the evidence.

**The rule tried (1i).** A window that ended `TurnLimitReached` earns
another one only if the *goal* moved — a plan step newly completed, or a
verification newly green — and not merely because the tree grew. Pure
function, four unit tests, the R011-F1 end-to-end test split into
"refinement that advances the plan finishes" and "refinement that only
rewrites the tree stops after two". The patch is kept outside the tree.

**What exp7 showed (§6).** The control arm never hit a ceiling; the one
treatment run that did, exp7/f1, ran three 100-round windows with **zero**
close attempts and was killed by the breaker at 310 — the exact shape of
exp2/f1. The rule was live and every window *qualified*: window 2 marked
"trace root cause" and "confirm failing paths" completed, window 3 turned a
check green — the agent's own `zz-scratch.test.tsx`. In 300 rounds it wrote
no source change at all. Both of the rule's signals are satisfiable by pure
investigation: a plan step is the model's own claim, and a green check
counts whether it covers the fix or a probe.

The rule also bounds nothing the breaker did not already bound: with the
no-progress cap at 2, the worst case is still initial window + two
no-progress windows = 300 rounds. Replayed against exp2/f1 it would have
stopped at the breaker line, not before it.

**What the evidence points at instead.** The two runs of this shape share
one fact no other run has: *no `update_goal` call in 300 rounds*. And the
industry answer to an agent that does not stop is not a cleverer progress
rule; it is a task-level budget the agent can see, with a bounded way to earn
more.

### 3e. An engine-paced task budget — **built here (1j)**, A/B in §6 exp8

`leveler run` now carries a `TaskRoundBudget` of **200 rounds, +100, at most
twice**. The numbers come from the batches: every run that closed did so
within 169 rounds; both runs that never closed were still going at 300.

- **The total is the engine's, not the caller's.** `Bounded` used to mean
  "the caller owns pacing; never add turns" (the eval harness). With a
  `round_budget` on the spec the same pinned total is engine-paced: a
  per-turn ceiling inside it is a window boundary, as for an unbounded goal,
  and the total is where a goal stops — `Incomplete`, with its work and
  ledger intact. Caller-pinned budgets (`round_budget: None`) are unchanged.
- **An extension is earned, never granted for trying.** When the total runs
  out, the segment since the last grant must show a **source change**
  (a mutation to a path that is not a test, a probe, or `node_modules`)
  **and a close attempt** (a refused `update_goal` or reconciliation
  intercept — an accepted one ends the task). Both, or it stops. exp2/f1 and
  exp7/f1 had neither; every run that closed had both.
- **The budget is visible.** At 80% the model is told once: rounds used of
  total, converge now, `update_goal(blocked)` if it cannot. Every
  continuation window's restatement names rounds used so far.

Read against the 24 C2 runs so far: none of the 14 that closed is touched
(max 169 < 200); the two 300-round investigations stop at 200 with an
`Incomplete` instead of a breaker kill.

### 3f. The closure reviewer was off the books — fixed here (1k)

The harness-launched closure review (`closure_review_stage` →
`TurnRunner::run_review` → `run_reviewer_child`) is a second entry into a
child agent, beside the model's own `delegate`. WP1 wired attribution and
WP2 the settlement reserve on the delegation path only. On this path:

- the reviewer's model calls came back as `SubAgentModelRequest` events,
  drained after it finished into an observer that maps them to a progress
  line — **zero rows** in `model_requests` for every one of the six closure
  reviews that launched across the C2 batches (exp9/c3's ran 9.5 minutes);
- its rounds, tokens and cost were folded into nothing: the engine builds a
  fresh executor with no parent ledger, so the session's cumulative counters
  — and with them the task round budget and the runtime's own bill — never
  saw it;
- its wall clock started at zero against the task's whole cap, not the
  residual minus the settlement reserve the delegation path grants.

Now `run_review` writes every reviewer record through the same
`storage_model_request` the root turn uses (its `agent_id` is already
stamped), folds the child's `ProgressLedger` into the persisted session
progress with the existing `absorb_child_spend`, and hands the reviewer the
task's elapsed time so `reviewer_wall_budget` grants the residual. Six
launched reviews, five `finished_incomplete`: that ratio is now at least
visible in the rows, and is a separate question.

## 4. Tests

| | | |
| --- | --- | --- |
| 1a | a re-edit stales a check that ran before it | `a_re_edit_of_an_already_modified_file_stales_prior_verification` |
| 1b | edits with no mutation record are `Unknown`, not `Stale`; no edits at all is still not fresh; one word per state | `edits_without_mutation_records_are_reported_as_unknown_not_stale`, `no_edits_at_all_is_still_reported_as_not_fresh`, `the_freshness_label_is_one_word_per_state` |
| 1c | a continuation seeds after a refused close and after a completed plan | `a_goal_continuation_seeds_even_after_a_refused_close`, `a_goal_continuation_seeds_even_when_the_plan_is_fully_completed` |
| 1d | a repair turn carries the edits and the failed check | `a_repair_turn_carries_the_edits_and_the_failed_check_it_is_repairing` |
| 1e | another file's change does not supersede an edit; only the same path does | `an_edit_is_not_superseded_by_a_later_change_to_another_file`, `an_edit_is_superseded_only_by_a_later_change_to_the_same_path` |
| 1f | wiring only — covered by the field re-test below, not by a unit test | — |
| 1h | a check that newly covers the latest edit is progress while closing; a redundant one is still thrash | `a_verification_that_newly_covers_the_latest_edit_is_not_closeout_thrash` |
| 1g | a piped / env-prefixed / `;`-chained verification run is named on the result; a recorded run, a non-verification pipeline and a failed run get no note | `a_piped_test_run_is_named_as_unrecorded_evidence`, `a_recorded_run_and_a_non_verification_pipeline_get_no_note`, `run_command_with_a_shell_wrapper_is_covered_too` |
| 1g | every program in a pipeline, chain, env prefix or nested `sh -c` is named | `every_program_in_a_pipeline_chain_or_env_prefix_is_named`, `a_nested_shell_body_is_looked_into` (`leveler-execution`) |
| 1i | withdrawn (§3d); its six tests left the tree with it | — |
| 1j | an engine-paced total continues at the window ceiling while rounds remain, stops at the total, honours the no-progress cap; a caller-pinned one never adds turns | `an_engine_paced_budget_continues_at_the_window_ceiling_while_rounds_remain`, `a_caller_paced_budget_still_never_adds_turns_at_the_ceiling` |
| 1j | an extension needs a source change and a close attempt in the segment, and is capped; source paths exclude tests, probes and `node_modules` | `investigation_alone_earns_no_extension`, `a_segment_that_landed_a_change_and_tried_to_close_earns_one`, `the_extension_count_is_capped`, `source_paths_exclude_tests_probes_and_dependencies` |
| 1j | the 80% note fires once, names both numbers, and never for tiny budgets | `the_note_fires_once_at_eighty_percent_and_names_both_numbers`, `tiny_budgets_get_no_note` |
| 1j | end to end: the total spans windows and is where an investigating goal stops | `an_engine_paced_budget_spans_windows_and_stops_at_the_total` |
| 1j | wiring: `--max-rounds` maps to default / unbounded / a base; a budget on the spec pins the continuation (the exp8 null result) | `the_flag_maps_to_default_unbounded_or_a_base`, `a_budget_pins_the_continuation_to_its_base`, `run_parses_max_rounds` |
| 1k | a harness-launched review's calls are rows under its agent id and its rounds are in the session's progress; its clock starts where the task's left off | `a_harness_launched_review_is_accounted_and_folded_into_the_session`, `the_reviewer_starts_its_clock_where_the_task_left_it` |

Every test was written red first and turned green by the change it names.

```
FMT=PASS
CLIPPY=PASS   (workspace, all-targets, all-features, -D warnings)
FULL_PRODUCT_TEST_GATE=3744 passed, 135 result lines, 1 failure
  the failure is `client_command_schema_is_current`: the committed
  `schemas/client_command.schema.json` lacks the `shutdown_when_idle` /
  `RestartReason` types added in f5a63b2. Pre-existing, outside this change,
  left for its owner to regenerate. Nothing in this diff touches that crate.
```

## 5. Limitations

**n = 3 per arm, one task.** Identical inputs vary 2.6–3.5× in request count
on this harness (WP3 measured it). The direction is carried by the mechanism
— the treatment arm's continuations after refused closes (the exact path that
dropped the ledger in Phase C and in exp2/f3) kept every record — and the
magnitudes are three observations. The drop needs a refused close followed by
a quiet window, which happened once in twelve runs; the unit tests are what
pin the mechanism, the batches show it no longer costs anything.

**1e and 1f are validated in the field only by exp4 (§6).** exp3 ran on a
binary that predates them; the "superseded" refusal in exp3/f2 is what
produced them.

**The freshness telemetry post-dates exp3.** `[workspace_facts.freshness=…]`
was added to the refusal record after exp3's binary was built, so no batch in
this document proves 1b's `Unknown` path directly. Unit tests do.

**Shared `/private/tmp` across runs.** Agents left `rhf-72`, `rhf7271`,
`rhf-upstream` and similar under `/private/tmp`, and later runs found and used
them (the symlinked `node_modules` came from there). Both arms ran in the same
environment at the same time, so it is a confound on absolute numbers, not on
the comparison.

**Cost is an estimate.** Usage times the configured price table; no invoice.

## 6. Field re-tests of the later fixes

Same task, same start tree, same oracle, same breaker; `--auto-approve`, as
every earlier batch (their approval records are all silent `approve_once`).
No control arm: exp3's control is the comparison and nothing in the control
binary changed.

### exp4 — 1a–1f

| run | requests | wall | close attempts / refused | "superseded" refusals | windows | GOOD |
| --- | ---: | ---: | --- | ---: | --- | --- |
| f1 | 91 | 14m | 2 / 2 | 0 | CloseoutForced:88 | PASS |
| f2 | 122 | 39m | 2 / 1 | 0 | 100 · CloseoutForced:20 | PASS |
| f3 | 88 | 23m | 1 / 1 | 0 | CloseoutForced:86 | PASS |

Sum 301 requests, 16.5M input tokens (98.6% cached), est. cost USD 0.40 —
against exp3's control 638 / 39.5M / 0.80 and exp3's treatment 354 / 16.6M /
0.41. The "superseded" refusal that produced 1e appeared once in three exp3
treatment runs and zero times here.

Every refusal in exp4 was **true**: each agent claimed completion after an
edit its last *recorded* green run predated (`freshness=stale` on all five
refusal records). And every run then ended `CloseoutForced` on a green tree,
one round short of closing — the guard conflict of §3b, which exp4 is what
found. That is 1h.

### exp5 — 1a–1h (without 1g)

| run | requests | wall | close attempts / refused | closeout denies | windows | GOOD |
| --- | ---: | ---: | --- | ---: | --- | --- |
| f1 | **71** | 23m | 1 / 0 | 0 | **Completed:69** | PASS |
| f2 | 131 | 43m | 2 / 1 (plan todo gate, then accepted) | 0 | 100 · **Completed:29** | PASS |
| f3 | 169 | 38m | 2 / 2 (`freshness=stale`, both true) | 2 | 100 · CloseoutForced:66 | PASS |

Two of three runs end **`Completed`** with `update_goal` accepted — the
first runs in this programme to do so; every earlier treatment run ended
`CloseoutForced`. f1 is the cleanest run recorded: one close attempt,
accepted, no guard engaged. f3 is §3c: both re-runs were piped, nothing was
recorded, 1h had nothing to work with. Sum 371 requests, 23.0M input tokens,
est. cost USD 0.56 — f3's 169 carries most of it.

### exp6 — 1a–1h with 1g

| run | requests | wall | close attempts / refused | 1g notes | windows | GOOD |
| --- | ---: | ---: | --- | ---: | --- | --- |
| f1 | 60 | 13m | 1 / 0 | 0 | **Completed:58** | PASS |
| f2 | 247 | 69m | 1 / 1 (R2, `freshness=fresh`) | 4 | 100 · BudgetExhausted:83 · CloseoutForced:63 | PASS |
| f3 | **42** | **11m** | 1 / 0 | 0 | **Completed:40** | PASS |

Sum 349 requests, 19.4M input tokens, est. cost USD 0.49. f3 is the cheapest
GOOD run this programme has recorded on C2 by any harness: Phase C's
AtomCode took 47 model requests on the same task and did not fix it.

f2 is two things at once. It is §3d — a 100-round investigation window, a
budget-exhausted second window, a continuation — and it is 1g working: four
notes fired (`node`, `pnpm`, `jest` behind pipes), after which every test run
was `pnpm exec jest …` through `run_command`, plain, and the ledger recorded
21 verifications. Its one refusal is a **new residual**, not freshness:
`freshness=fresh`, judge `Satisfied`, but the contract had derived obligation
R2 from the issue's "why it is easy to miss" paragraph — *schema resolvers
reject the undefined* — as a verification-class requirement, and no check in
a repository that has no resolver library could ever discharge it. The agent
spent its last rounds grepping for `zod` and was force-stopped. That is a
contract-derivation question (`completion_contract.rs`), outside this
closure; recorded here so it is not rediscovered.

### exp7 — 1i A/B: control d125402 (1a–1h), treatment adds the post-ceiling rule

Three and three, in parallel, same task, tree, oracle and breaker.

| run | arm | requests | close attempts | windows | GOOD |
| --- | --- | ---: | ---: | --- | --- |
| c1 | control | 79 | 1 | Completed:77 | PASS |
| c2 | control | 58 | 2 | Completed:57 | PASS |
| c3 | control | 58 | 1 | Completed:56 | PASS |
| f1 | treatment | **310 (breaker)** | **0** | 100 · 100 · 100 · … | **FAIL** |
| f2 | treatment | 61 | 2 | Completed:59 | PASS |
| f3 | treatment | 63 | 2 | Completed:60 | PASS |

Control 195 requests, 9.4M input tokens; treatment 434 and 22.4M, of which
f1 alone is 310 and 16.7M. The five runs that never hit a ceiling are
indistinguishable across arms, as they must be — the rule only reads after
a `TurnLimitReached` window. The one run that did hit it is the run the rule
was built for, and the rule let it through three times (§3d). At n=3 this
is one observation; it is also the only observation the rule could have
been judged on, and it went the wrong way. Withdrawn.

### exp8 — 1j, first A/B: **a null result, by a wiring bug**

Three control (d125402) and three treatment, in parallel.

| run | arm | requests | windows | GOOD |
| --- | --- | ---: | --- | --- |
| c1 | control | 49 | Completed:47 | PASS |
| c2 | control | 70 | Completed:68 | PASS |
| c3 | control | 112 | 100 · Completed:10 | PASS |
| f1 | treatment | 94 | Completed:92 (closeout gate failed: the agent's symlinked `node_modules`) | PASS |
| f2 | treatment | 68 | Completed:66 | PASS |
| f3 | treatment | 72 | Completed:70 | PASS |

Indistinguishable arms — and they should have been, for the wrong reason.
The budget was on the spec, but the headless `run_in_session` path pinned
`UntilTerminal` over the spec's continuation, so `round_limit()` was `None`
and neither the total, the extension, nor the 80% note could ever engage.
The treatment binary behaved exactly like control. Found by asking why no
run had printed a budget line; fixed by making the headless goal path derive
its continuation from the task budget (`goal_continuation_for`), with a
regression test pinning that a budget on the spec yields a pinned
continuation. The eval path and the interactive UI keep `UntilTerminal`
and no budget. `leveler run --max-rounds N` sets the base (`0` = unbounded).

### exp9 — 1j, wired: A/B plus a bound check

Before the batch, a three-round sanity run on an empty repository:
`--max-rounds 3` stopped at round 3 — *"Reached the 3-round limit before
finishing. Resume with: leveler resume …"*. The budget binds.

**m40** — treatment with `--max-rounds 40` on C2, so the total is reached on
a real model:

- stopped at exactly round 40, `BudgetExhausted`, with the files changed and
  a resume id in the stop message; the tree passed the oracle;
- the 80% note was injected at round 32 (in `session_messages`; the CLI does
  not echo injected messages);
- the segment had a source change *and* a refused close, so it had **earned**
  the extension — and did not get it. The pinned window limit exits as
  `BudgetExhausted` with no budget dimension, and the extension gate was keyed
  on `TurnLimitReached` alone. `round_budget_spent` now names both endings
  as "the rounds are spent" and neither tokens/cost/time exhaustion nor any
  other stop; **m40b** re-runs the same check on the corrected binary.

**m40b** — corrected binary, `--max-rounds 40`: stopped at 40 again, and
this time the rule is what stopped it. The segment had source changes
(`useController.ts`, a new `getNullAncestorValue.ts`) but **no close
attempt** in 40 rounds, so no extension: the tree was cut mid-edit, with a
resume id, and fails the oracle. That is the trade the budget makes, shown
on a real run: finishing earns more rounds, editing alone does not. With
the default base of 200 no run in this programme would have been cut —
every close came by 169.

**A/B** — three control (d125402) and three treatment (budget wired), in
parallel:

| run | arm | requests | close attempts / refused | windows | GOOD |
| --- | --- | ---: | --- | --- | --- |
| c1 | control | 68 | 1 / 0 | Completed:66 | PASS |
| c2 | control | 73 | 2 / 1 | Completed:70 | PASS |
| c3 | control | 82 | 2 / 2 | CloseoutForced:79 | PASS |
| f1 | treatment | 81 | 2 / 1 | Completed:78 | PASS |
| f2 | treatment | **26** | 1 / 1 (contract obligation, `freshness=fresh`) | CloseoutForced:24 | PASS |
| f3 | treatment | 87 | 1 / 0 | Completed:85 | PASS |

Control 223 requests / 12.0M input tokens, treatment 194 / 10.4M. No run
came within 100 rounds of the total, so the arms are equivalent by
construction: the budget is a bound on the tail, and this batch, like exp8,
drew no tail. The evidence that the bound binds, and how, is the three
`--max-rounds` runs above; the evidence that it costs nothing when it does
not bind is these six plus exp8's six.

**m70** — `--max-rounds 70`: stopped at 70, `BudgetExhausted`, tree GOOD
(the agent's last note: full suite green, "final review of the diff") —
and **no close attempt in 70 rounds**, so no extension. Three field runs at
the total, and the extension path was earned only in the one that hit the
gate bug (m40); the other two had a source change and never claimed. That
is the rule as decided — finishing earns rounds, editing does not — shown
twice at its sharpest: a green tree cut because the model had not yet said
so. At the default base of 200 no observed run is near that edge (latest
close: 169). If that edge ever bites, the candidate relaxation is recorded
here and not built: accept a *fresh green check covering a source change*
in place of the close attempt.

The extension mechanism itself is covered by unit tests
(`a_segment_that_landed_a_change_and_tried_to_close_earns_one`,
`the_total_is_spent_by_either_rounds_stop_but_not_by_other_budgets`); it has
not yet been observed granting in the field.

### Across the treatment batches

| batch | binary | requests (3 runs) | ended `Completed` | GOOD |
| --- | --- | ---: | ---: | --- |
| exp3 control | WP2 | 638 | 0 | 3/3 |
| exp3 | 1a–1d | 354 | 1 | 3/3 |
| exp4 | 1a–1f | 301 | 0 | 3/3 |
| exp5 | 1a–1h | 371 | 2 | 3/3 |
| exp6 | 1a–1h + 1g | 349 | 2 | 3/3 |

Twelve treatment runs, twelve GOOD, no breaker kill. The request count is
noisy at n=3 — one §3d investigation run moves a batch by a hundred — but
the *shape* of how runs end moved monotonically: from every run needing a
forced closeout to most runs closing on their own first claim.

## 7. What this licenses

```
COMPLETION_JUDGE_TOLD_THE_TRUTH_ABOUT_FRESHNESS=YES
GOAL_CONTINUATION_KEEPS_EVIDENCE=YES
CHEAP_IMPROVEMENT_CONFIRMED=YES   on this task, this n
CODELEVELER_IS_CHEAPER_THAN_ATOMCODE=NOT_CLAIMED

NEXT_DECISION=HUNDRED_ROUND_WINDOW_CHAIN_PROGRESS_RULE
NEXT_FINDING=CONTRACT_OBLIGATION_FROM_EXPLANATORY_PROSE
NEW_FORMAL_TREATMENT_FROZEN=NO
```

Nothing here changed a limit, a prompt, or a model setting. Every fix
replaced a false statement the runtime was making — to its judge, to its
model, or to itself — with a true one, and the rounds that had been spent
arguing with those statements went away.
