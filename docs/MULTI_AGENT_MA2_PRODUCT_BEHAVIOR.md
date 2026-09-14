# Multi-Agent MA2 — Product Behavior Closure

Status: **PASS** (gate recorded after CI, §6). Scope from `docs/MULTI_AGENT_MA0_REALITY_AND_GAP_AUDIT.md`
§9 (REAL_MA2_SCOPE). Base: MA1 PASS at `1ed1c62`.

## 1. What MA2 did not change, on purpose

| Question | Answer at this HEAD |
|---|---|
| Who decides whether to delegate | The model. The hint states mechanics only (`the_steer_hint_states_mechanics_not_strategy`); no runtime heuristic spawns. |
| New roles | None. Default / Explorer / Worker / Reviewer cover every observed task; Explorer and Reviewer are structurally read-only. |
| Provider trait | Not required (MA0 §5.4). Children route through `ProviderRegistry`; a named agent pins a model. |
| Mandatory delegation | None. S1 below is the natural control: 0 spawns. |

## 2. Changes

| Item | Change | Commit | Test |
|---|---|---|---|
| G9a silent role mapping | An unknown role word or `reviewer` is refused with the reason — alone or beside `profile`; a non-string `role`/`profile` is refused; a bad role in a named agent's definition names the agent; omitted/empty still means Default | `55884f9`, `412ee90` (review) | `an_unknown_or_harness_only_role_word_is_refused`, `an_unknown_role_beside_a_profile_is_refused`, `a_non_string_role_is_refused_not_read_as_omitted`, `a_bad_role_in_an_agent_definition_names_the_definition` |
| G9b pinned model admission | Unresolvable pinned model, or unpriced under a cost cap, refused at spawn; resumed child settles failed | `ac43ef6` (MA1 review) | `an_unpriced_pinned_model_is_refused_under_a_cost_cap`, `an_unresolvable_pinned_model_is_refused_at_spawn` |
| Task tools on a child id (MA1 dogfood) | `wait_task` / `get_task` / `kill_task` naming a child get an informational answer (`is_error=false`): what the id is, whether it is running or finished-awaiting-settlement, and for `kill_task` that a child cannot be cancelled from the model side | `55884f9`, `412ee90` | `task_tools_naming_a_running_child_say_what_the_id_is`, `task_tools_naming_a_finished_unsettled_child_do_not_call_it_running` (child held on a signal, no sleep race for the running case) |

## 3. Real-model behavior batch

Release binary `55884f9`, `deepseek/deepseek-v4-flash`, isolated
`LEVELER_HOME`, fresh copy of `evals/fixtures/repos/navsvc` per scenario, one
run each. S2–S8 name `spawn_agent` in the task: they check what the runtime
and the parent do once a child exists, not whether the model elects to
delegate. Counts read from each session's event log and `model_requests`.

Columns: **Dup** = children with more than one terminal; **Open** = started
children with no terminal; **Refused** = `spawn_agent` calls that returned an
error (admission refusals only — task-tool answers are not counted); **Wall**
= first to last event of the session.

| Scenario | Spawns | Child terminals (outcome / stop) | Dup | Open | Refused | Wall | Result |
|---|---|---|---|---|---|---|---|
| S1 simple question, no instruction | 0 | — | 0 | 0 | 0 | 4 s | answered directly; no delegation |
| S2 background explorer | 1 | with_findings / completed | 0 | 0 | 0 | 29 s | `SINKS.md` written from the settled report |
| S3 foreground dependency | 1 | with_findings / completed | 0 | 0 | 0 | 29 s | parent used the result: comment added in `internal/ingest/decoder.go`; checks passed |
| S4 parallel explorers (one message) | 2 | with_findings / completed ×2 | 0 | 0 | 0 | 25 s | `PACKAGES.md` with both summaries |
| S5 parallel disjoint workers | 2 | with_findings / completed ×2 | 0 | 0 | 0 | 23 s | both `NOTES.md` files, nothing else changed; the parent called `wait_task` on both child ids and got the sub-agent answer (binary `55884f9`, before the review wording change) |
| S6 child stopped by its round cap | 1 | incomplete_partial / budget | 0 | 0 | 0 | 19 s | parent reported the child did not finish and named what was not covered; did not treat it as investigated |
| S7 harness-launched reviewer | 1 (reviewer) | with_findings / completed | 0 | 0 | 0 | 22 s | change landed, reviewer bounded (2 requests), checks passed |
| S8 overlapping workers, one message | 1 | with_findings / completed | 0 | 0 | 1 | 21 s | second spawn refused: "overlaps a worker already admitted in this batch"; only A wrote |

Totals across the batch: **DUPLICATE_SETTLEMENT=0, OPEN_EDGES=0,
OWNERSHIP_VIOLATION=0**, child costs recorded per child row in every run.

Exit codes of 1 are `CompletedUnverified` (no checks in the fixture copy),
not failures; S3 and S7 ran checks and exited 0.

Covered elsewhere, not re-run here:

| Behavior | Evidence |
|---|---|
| restart while a child runs | MA1 §9.2 — explorer and worker, real `SIGKILL`, same id resumed and settled once |
| cancel one child | MA1 `a_host_can_cancel_one_child_without_cancelling_its_parent` (deterministic; a real client control lands in MA3) |
| parent completes only after its children | existing goal intercept, seen live in the MA1 worker run |

## 4. Residuals

- The batch is one run per scenario: it proves each behavior exists with a
  real model, not its rate. Rates are MA4.
- Parent confusion between background tasks and children is answered, not
  prevented: the model may still spend a round on it (seen in S5).

## 4a. Independent review

A fresh-context review of `1ed1c62..4292d00` found no high-severity issue and
no authority bypass (the interception runs no tool; role changes only
narrow). One medium (this document claimed PASS before CI and carried no
workspace-test evidence) and six low findings (role word beside `profile`
skipped; non-string role read as omitted; misleading refusal for a bad role
in an agent definition; "still running" for a finished-unsettled child and
no cancel wording for `kill_task`; `is_error=true` counted as progress; a
sleep-raced test). All fixed test-first in the commit after `4292d00`. It
could not verify the real-model batch, whose sessions live outside the
repository.

Quality after the fixes: `cargo fmt` pass, `cargo clippy -D warnings` pass,
`cargo test --workspace --all-features --locked`: 3863 passed, 0 failed,
20 ignored.

## 5. Gates

```text
DELEGATION_STRATEGY_OWNER=MODEL
CHILD_PROFILES=NO_NEW_ROLES_REQUIRED
CAPABILITY_NEGOTIATION=PASS (G9a, G9b)
PROVIDER_ABSTRACTION=PROVEN_NOT_REQUIRED
BACKGROUND_SETTLEMENT=PASS
FOREGROUND_DEPENDENCY=PASS
PARALLEL_CHILDREN=PASS
PARTIAL_CHILD_TRUTH=PASS
REVIEWER_BOUNDED=PASS
NO_MANDATORY_DELEGATION=PASS
DUPLICATE_SETTLEMENT=0
OWNERSHIP_VIOLATION=0

MA2_PRODUCT_BEHAVIOR=PASS
```

## 6. CI

| Commit | Run | Attempt | Result |
|---|---|---|---|
| `4292d00` (batch + doc) | `34804369759` | 1 | success |
| `8cc63b6` (review fixes `412ee90` + doc) | `34805121346` | 1 | success: Linux, macOS, Windows, web, mobile, deny/audit |
