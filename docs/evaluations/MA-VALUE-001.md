# MA-VALUE-001 — Multi-Agent Value Evaluation

**Status:** framework ready, not executed · **Subject:** `v0.2.0-beta.1` · **Opened:** 2026-08-22

Observer suite. Does not change spawn lifecycle, child runtime, tool schema,
agent prompt, or delegation behaviour.

Question: **does Multi-Agent complete real coding tasks more effectively than
Single Agent?** MA-WA1 already closed `DELEGATION_RUNTIME = PASS` and
`DELEGATION_SAFETY = PASS`. Adoption was `NOT GUARANTEED`. This experiment
measures *value*, not spawn frequency.

## Hypothesis

- **H1:** Multi-Agent reduces coding-task cost or improves quality.
- **H0:** Multi-Agent provides no measurable improvement.

A fail does **not** mean Multi-Agent has no value. See [Failure
interpretation](#failure-interpretation).

## Experimental design

| | Control | Treatment |
| --- | --- | --- |
| CLI | `--mode single` | `--mode multi` |
| Config (isolated `LEVELER_HOME` only) | shipped `agents.delegation = false` | product default (delegation on) |
| Model / provider | same | same |
| Repository / task / tools / budget | same | same |
| Runtime | unchanged | unchanged |

The only product input that differs is the already-shipped
`agents.delegation` key, written the same way the timing arm writes
`agents.offer_timing`. `~/.leveler` is not modified.

Fixed across arms: CodeLeveler revision, case pointers, work mode, permission
policy, verification settings, model endpoint, repetition count, machine.

### Tasks

Existing Real Usage Batch #1 cases. No synthetic tasks.

| ID | Repository | Pressure |
| --- | --- | --- |
| R005 | `rust-lang/cargo` | Large Rust exploration |
| R006 | `casdoor/casdoor` | Full-stack cross-layer |
| R007 | `hoppscotch/hoppscotch` | TypeScript monorepo impact |
| R008 | `BurntSushi/ripgrep` | Correctness + independent review |
| R009 | `go-task/task` | Concurrency + independent review |
| R010 | TailAdmin (frontend long task) | Long task + browser |

Pointers live in `eval/suites/multi_agent/multi_agent_value/cases/`. Task
statements and hidden verifiers stay in the dogfood-control repo. This tree
does not vendor them.

### Scale (phase 1)

6 tasks × 2 modes × 3 runs = **36 runs**. n < 6 valid runs per arm →
`insufficient_n`, not a published verdict.

This phase implements the framework. It does **not** execute the 36 real-model
runs (cost). Score existing EventLogs when they exist.

## Metrics

**Not a success metric: spawn rate.** MA-WA1 showed spawn frequency ≠ value.

| Family | Fields | Source |
| --- | --- | --- |
| Task success | `success` / `failure` (`task_success`) | Independent verifier (existing `expect`). Never the agent's summary. |
| Efficiency | `turns`, `input_tokens`, `output_tokens`, `total_tokens`, `wall_time`, `tool_calls` | EventLog rounds; `model_requests` for tokens; `events.created_at` for wall time. Missing table/column → `null`, not `0`. |
| Multi-agent utility | `child_spawned`, `child_role`, `child_completed`, `child_result_consumed`, `child_contribution` | Durable `sub_agent_started` / `sub_agent_finished`. Consumption = useful worker mutation **or** parent tool / `resolve_finding` after the child finished. |
| Quality | `tests_passed`, `regressions`, `review_findings`, `missed_issues` | Verification checks / review stage when present. `regressions` and `missed_issues` are human/paired fields — EventLog does not invent them. |

`child_contribution` vocabulary (observer heuristic, human annotation wins):

`exploration_reduction` · `bug_found` · `plan_improvement` ·
`verification_improvement` · `context_reduction`

An empty list means *unclassified*, not *no contribution*. Explorers do not
mutate; they still count as consumed when the parent acts on their result.

## Result schema

Observer `eval_result.json` is additive on the existing run record
(`schema_version: "1"`). Old adoption records still validate.

```json
{
  "experiment": "MA-VALUE-001",
  "mode": "single|multi",
  "task_success": true,
  "metrics": { "turns": 0, "tokens": 0, "duration": 0 },
  "multi_agent": {
    "spawn_count": 0,
    "child_roles": [],
    "child_result_used": false
  }
}
```

`metrics.tokens` / `duration` are `null` when usage was not recorded. The
zeros above are an example shape, not a default.

## Decision criteria

**PASS** when all three hold on the paired comparison (n ≥ 6 per arm):

1. Task success does not drop versus single-agent.
2. At least one of: lower turns, lower tokens, lower wall time, better
   verification, useful child findings.
3. Child output is used by the parent.

**FAIL** is `no_measured_improvement_…`, not `multi_agent_has_no_value`.

## Failure interpretation

If the comparison fails, inspect — do not retire Multi-Agent:

- **Task type.** R005–R007 stress exploration; R008–R009 stress review;
  R010 stresses a long frontend. A miss on one type is not a miss on all.
- **Delegation timing.** A late spawn that the parent ignores is overhead.
- **Child usefulness.** A finished child whose result is unused cannot pass
  criterion 3 even if cheaper.
- **Runtime overhead.** Extra tokens/wall from children with no parent
  consumption is a cost, not a capability proof.

## How to run

```sh
# Framework: wires the isolated home, writes HOW_TO_RUN. No model calls.
leveler eval run --suite multi_agent --experiment MA-VALUE-001 --mode single
leveler eval run --suite multi_agent --experiment MA-VALUE-001 --mode multi
```

Reports: `eval/reports/multi_agent/MA-VALUE-001/<mode>/{batch.json,report.md,eval_result.json}`.

A full 36-run execution needs the dogfood-control checkouts, a published
binary, and budget. It is out of scope for this phase.

## Post-Beta measurement items (not done here)

These would need product changes or a human corpus. They are **not**
implemented in this suite:

| Item | Why it is blocked |
| --- | --- |
| Per-child durable token totals | `SubAgentProgress` is transient; `model_requests` has no `agent_id`. Already listed in `BETA_RELEASE_READINESS.md`. |
| Runtime `child_contribution` labels | EventLog has no such vocabulary. The observer heuristic is not a product claim. |
| Automated `missed_issues` | Needs an independent review corpus. |
| Automated `regressions` | Needs a paired baseline tree, not a single EventLog. |

Do not start Structured SubAgent / Explorer-Worker-Reviewer product work on
the strength of this framework existing. Wait for MA-VALUE-001 results.
