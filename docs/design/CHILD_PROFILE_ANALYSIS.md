# Child Profile — Phase 1 analysis

**Opened:** 2026-08-24 · **Subject:** `v0.2.0-beta.1` + Structured Child Result /
Contribution Trace · **Status:** analysis; implementation follows this document

The brief assumes children are generic. They are not. What is missing is not a
second spawn runtime — it is a **named capability contract** sitting on top of
the role flags that already decide tools, workspace, and round bounds.

## The short version

```
TODAY                         TARGET
spawn_agent(task, role?)      spawn_agent(task, profile?)
        ↓                              ↓
AgentRole flags               ChildProfile contract
(read_only, scope, …)         (why / what / tools / output / limits)
        ↓                              ↓
generic-looking child         typed child, same primitive
```

Do **not** invent a parallel profile type. Expand the existing
`ChildProfile` in `leveler-agent` until it can answer the four questions the
brief names, and keep every security fence that already works.

## What already exists

### Role is real, and it already gates capability

`crates/leveler-agent/src/sub_agent.rs` — `AgentRole` + a `Copy` `ChildProfile`:

| Role | How it is entered | Tools | Workspace | Bound | Blocking findings |
| --- | --- | --- | --- | --- | --- |
| `Default` | `spawn_agent(task)` with no role | full minus MCP | late-bound `claim_write_scope` | residual parent budget | no |
| `Explorer` | `role=explorer` or named persona | `read_only_subset()` (observe ∩ Safe) | structurally read-only | residual | no |
| `Worker` | `role=worker` + `files` | full minus MCP, serial tools | exclusive pre-claimed scope | residual | no |
| `Reviewer` | harness only (`run_reviewer_child`) | same registry as Explorer | structurally read-only | 20 rounds | yes |

`ChildProfile::resolve` / `admit` is already the single resolve point the
2016-era audit (`docs/MULTI_AGENT_EXISTING_ARCHITECTURE.md` §14) asked for.
Admission is honest: an unscoped worker is refused, a read-only role asking for
`files` is refused, overlapping same-batch worker scopes are refused.

`AgentRole::parse` does **not** accept `"reviewer"`. That is load-bearing: a
model cannot mark its own homework. The built-in named persona `code-reviewer`
is `role: explorer`, not the harness Reviewer.

### Spawn already takes a role, not a profile

`spawn_agent` schema (`injected_tools.rs`): `task` required; `role` ∈
`default | explorer | worker`; `files`; `agent`; `run_in_background`.

Omitting `role` is Default. That path is what MA-VALUE-A ran. It must keep
working.

### Tool policy already integrates the permission system

Capability is expressed **per semantic class**, never per role × tool name:

- Explorer / Reviewer → `ToolRegistry::read_only_subset()` =
  `OBSERVE_CLASS_TOOLS ∩ RiskLevel::Safe`. A forbidden edit is "unknown tool",
  not a prompt suggestion.
- Worker / Default → `without_mcp_tools()` + the ownership registry. MCP is
  refused because an unsandboxed proxy cannot be bounded to a claimed scope.
- Host approval, sandbox, and `write_targets_outside_allowlist` still run.

The Reviewer prompt explicitly forbids re-running builds and test suites. The
brief's "read + test" is therefore **not** `run_command`. Test *execution* is
host verification on `EvidenceLedger`. Reviewer *judgment* of tests is
`FindingKind::Test` / `Correctness` plus the output contract's Verification
flag.

### Output is already structured — do not fork it

| Artefact | Where it lives | Status |
| --- | --- | --- |
| `FindingRecord` / `FindingState` | `EvidenceLedger.findings` | shipped; 490 `report_finding` calls on the MA-VALUE-A treatment arm |
| `ChildStatus` four-way truth | `ChildResult` | shipped; parent still reads `for_parent()` prose |
| Contribution counts | `ChildResultProjection` on `SubAgentFinished` | shipped; join is `source_child` |
| Changed files | child's `modified_files`, folded into the parent | shipped, not on the projection (no-payloads rule) |
| Verification | `EvidenceLedger.verifications` | shipped, host-owned |

**No second Finding type. No `StructuredChildResult` fork.** A profile's
`output_contract` names which of these existing channels a parent should expect.

### Trace already attributes a child, not a profile

`SubAgentStarted { id, nickname, role, task }` and
`ChildResultProjection { child_id, role, findings_* }`.

The log can answer "which child" and "did the parent act". It cannot answer
"what capability contract produced this value" without parsing the prompt or
guessing from `role`. `role` is close — it is 1:1 with today's four built-ins —
but it is a string with no capabilities, no output expectation, and no id
stable enough for an eval to group on once custom profiles exist.

### Eval already scores value, not spawn rate

MA-VALUE-001 (`eval/lib/value.py`) records `child_roles` and contribution
heuristics. It does not yet bucket *by profile*: explorer findings accepted,
reviewer bugs confirmed, worker changes accepted / verification passed.

Existing EventLogs without `profile_id` must still score: fall back to `role`.

### What is not in scope (and must not be built here)

UI, Agent Marketplace, Agent builder, remote workers, ACP,
`SubAgentProvider`, external providers, cloud execution, capability
negotiation, a second scheduler, a change to max child = 6 / max concurrent = 4,
or a rewrite of the spawn primitive.

Named personas (`.leveler/agents/*.md`) stay as they are: they compose
instructions and may *select* a profile via `role:`, they are not profiles.

## The missing abstraction

The current `ChildProfile` is a bag of flags derived from `AgentRole`. It
cannot be serialized, cannot be shown on a card, cannot be grouped in eval,
and cannot say what output a parent should expect.

The missing type is still called `ChildProfile`. It gains:

```
id, name, role, description, purpose
capabilities            — composable, closed set
tool_policy             — maps onto read_only_subset / without_mcp_tools
output_contract         — flags over FindingRecord / Verification / ChangedFiles
workspace_policy        — read-only | late-bound | controlled mutation
runtime_policy          — max_rounds, serial_tools, may_report_blocking
budget_policy           — inherit residual + wall-clock cap
```

Role stays the small closed enum (`Explorer | Reviewer | Worker | Default`).
Capability is orthogonal and composable. Three product profiles plus the
unnamed Default compatibility profile.

## Migration strategy

1. **Expand, do not replace.** Move `AgentRole` + `ChildProfile` into
   `crates/leveler-agent/src/child_profile.rs`. Keep `ChildProfile::resolve` /
   `admit` as the only resolve point. Existing flags become methods
   (`read_only()`, `requires_scope()`, …) derived from the new policies so
   call sites cannot drift.

2. **`spawn_agent(task, profile?)` is additive.** `profile` is an optional
   alias of `role` that resolves a full contract. Omit both → Default, identical
   to today. `role` stays. Conflicting `profile` + `role` is an honest denial.
   Unknown `profile` is an honest denial. `profile=reviewer` is an honest
   denial on the model path (harness-only). `AgentRole::parse` still does not
   accept `"reviewer"`.

3. **Do not change prompts, caps, or the parent-facing prose path.** The
   +16 % Explorer workflow reads `for_parent()` text. Profile purpose/description
   are for events, eval, and a future UI — they are not injected into the child
   prompt in this phase.

4. **Trace is additive, serde-defaulted.** `SubAgentStarted` and
   `ChildResultProjection` gain `profile_id`, `profile_role`, `capabilities`.
   Old events replay. `None` / empty means "not recorded", not "no profile".
   `UiAgentObservation` is **not** widened (same reason as contribution trace:
   that is UX-phase work).

5. **Eval is observer-only.** `profile_effectiveness` is computed from EventLog.
   Spawn rate stays diagnostic. Existing MA-VALUE-001 records remain valid
   (`additionalProperties: true`, role fallback).

6. **Reviewer "read+test" does not add `run_command`.** That would punch through
   the structural read-only fence and contradict the reviewer prompt. The
   `read_test` tool policy is the contract name; the physical registry stays
   `read_only_subset()`. Verification is the host ledger.

## Built-in mapping (the contract this implementation will pin)

| id | role | capabilities | tools (physical) | workspace | output |
| --- | --- | --- | --- | --- | --- |
| `default` | Default | `implementation` | `without_mcp_tools` | late-bound claim | findings + changed_files |
| `explorer` | Explorer | `repository_analysis` | `read_only_subset` | read-only | `FindingRecord[]` |
| `reviewer` | Reviewer | `code_review`, `verification` | `read_only_subset` | read-only | findings + verification |
| `worker` | Worker | `implementation`, `testing`, `verification` | `without_mcp_tools`, serial | controlled mutation, `files` required | changed_files + verification |

Aliases: `builtin.<id>` parses as `<id>`.

## Success after this phase

The runtime can answer, without reading prompt text:

- Why was this child created? → `profile_id` / `purpose`
- What can it do? → `capabilities` + `tool_policy`
- What should we expect back? → `output_contract`
- How much value did it create? → existing contribution projection, now grouped
  by profile
