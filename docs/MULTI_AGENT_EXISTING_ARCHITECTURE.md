# Multi-agent: what actually exists at `bd06c901`

Audited in code before any change. The foundation is substantially more product-shaped than a
"primitives only" description suggests — the closure work is narrower than feared.

## Answers to the fourteen audit questions

| # | Question | Fact |
| --- | --- | --- |
| 1 | Primitives present | `spawn_agent` (roles default/explorer/worker, `files` worker scope, named personas), `run_one_sub_agent_on`, `run_reviewer_child`, `AgentRole` (+Reviewer, model-unreachable), `ChildResult{status,findings,stop_reason,partial}`, depth/concurrency/total caps, residual budgets, partial-said capture, `review_stage` observability |
| 2 | Already productised | Reviewer (harness-driven, bounded 20, closure-gated, observable); worker file ownership (`write_targets_outside_allowlist` rejects out-of-scope edits pre-run); named agents (`.leveler/agents/*.md`, project→user→builtin); role prompts |
| 3 | Mechanism-only | child findings are a free-text String; no parent consumption record beyond the tool-result text; no cross-worker scope overlap check at spawn admission |
| 4 | Explorer today | `read_only_subset()` registry (READ_ONLY_TOOLS ∩ Safe risk — capability-class based, not name×role ifs), explorer prompt, no mutation possible |
| 5 | Worker today | full registry + `write_allowlist` hard enforcement at the drive site; `max_parallel_tools=1`; prompt pins it to its files |
| 6 | Reviewer ↔ primitive | second entrance to the same primitive (`run_reviewer_child` → `run_one_sub_agent_on`), read-only registry, 20-round bound |
| 7 | Parent learns results | `ChildResult::for_parent` string into the tool result + `SubAgentFinished{ok,summary}` durable |
| 8 | Child failure propagation | `INCOMPLETE_*`/`FAILED` statuses honest; parent text says "do it yourself or delegate again" |
| 9 | Shared context? | No — fresh conversation per child, by design (schema says so) |
| 10 | Capability decision | role → registry subset + allowlist + policy; named persona may set role |
| 11 | Concurrency | semaphore 4 concurrent, 6 total per run, `MAX_SUB_AGENT_DEPTH=1` (child cannot spawn) |
| 12 | Write-collision risk | per-child allowlist enforced; **same-batch overlapping worker scopes are NOT checked** — the one real gap |
| 13 | Finding abstraction | none (String) |
| 14 | UI disclosure | TUI renders SubAgent groups from Started/Activity/Finished; summary text carries the child status line |

## The gaps that define this closure

1. **Structured findings + durable lifecycle** — nothing typed, nothing resumable.
2. **Parent consumption** — no durable acknowledge/accept/reject.
3. **Blocking-finding closure truth** — only "did a required review complete" gates Verified; an
   individual correctness finding cannot block.
4. **Same-batch worker scope overlap** — unchecked.
5. **Profile formalisation** — the capability logic exists but is scattered; needs one resolve
   point and contract tests, not new machinery.
