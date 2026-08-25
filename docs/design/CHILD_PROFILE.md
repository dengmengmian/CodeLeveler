# Child Profile

**Opened:** 2026-08-24 · **Status:** implemented · **Does not:** UI, marketplace,
remote workers, ACP, SubAgentProvider

Moves Multi-Agent from a generic child whose behaviour lived in prompt text to
a named capability contract the runtime, the trace, and eval can all read.

Analysis that this builds on: [`CHILD_PROFILE_ANALYSIS.md`](CHILD_PROFILE_ANALYSIS.md).

## Motivation

`spawn_agent(task)` already created focused children — Explorer, Worker, a
harness-launched Reviewer — but the only durable fact was a role string.
After MA-VALUE-A (+16 % vs single agent) the log could say *that* a child
helped, not *what kind of child* produced the value.

A generic child is insufficient because:

- the parent cannot know what output to expect without reading the task prose
- eval cannot group Explorer findings vs Reviewer bugs vs Worker changes
- a future UI card / timeline / inspector has nothing typed to render

This phase does **not** change how children run. It names the contract they
already had.

## Architecture

```
Task
  ↓
ChildProfile          ← id, role, capabilities, tool/workspace/output/runtime/budget
  ↓
Child Agent           ← same spawn primitive, same caps (6 / 4)
  ↓
Structured Result     ← FindingRecord / Verification / ChangedFiles (existing)
  ↓
Contribution Trace    ← now also profile_id, profile_role, capabilities
```

`ChildProfile` is the single resolve point. It lives in
`crates/leveler-agent/src/child_profile.rs`. The old flags
(`read_only`, `requires_scope`, `may_report_blocking`, `max_rounds`,
`serial_tools`) are methods derived from the policies, so they cannot drift.

Spawn:

```
spawn_agent(task)                 → Default profile (unchanged)
spawn_agent(task, role=explorer)  → Explorer (historical alias)
spawn_agent(task, profile=explorer)
spawn_agent(task, profile=worker, files=[…])
```

`profile=reviewer` is an honest denial. Independent review stays
harness-launched (`run_reviewer_child`) so a model cannot mark its own
homework. The Reviewer *profile* still exists and is what the harness uses.

Conflicting `profile` + `role` is refused. Unknown ids are refused. An
unscoped Worker is refused. A read-only profile asking for `files` is refused.

## Built-in profiles

| id | role | capabilities | tools (physical) | workspace | output |
| --- | --- | --- | --- | --- | --- |
| `default` | Default | `implementation` | `without_mcp_tools` | late-bound claim | findings + changed files |
| `explorer` | Explorer | `repository_analysis` | `read_only_subset` | read-only | `FindingRecord[]` |
| `reviewer` | Reviewer | `code_review`, `verification` | `read_only_subset` | read-only | findings + verification |
| `worker` | Worker | `implementation`, `testing`, `verification` | `without_mcp_tools`, serial | exclusive `files` | changed files + verification |

`builtin.<id>` parses as `<id>`.

Reviewer `tool_policy` is named `read_test`. Physically it is the same
observe-class subset as Explorer: `run_command` is a mutation surface, and the
Reviewer prompt already forbids re-running suites. Verification is host-owned
on `EvidenceLedger`.

No new result types. Output flags name existing channels
(`FindingRecord`, ledger verifications, the child's `modified_files`).

## Trace

`SubAgentStarted` and `ChildResultProjection` gain, with `serde(default)`:

- `profile_id`
- `profile_role`
- `capabilities`

Old events replay. `None` / empty means "not recorded".
`UiAgentObservation` is **not** widened (UX-phase work).

`leveler trace` shows Profile / Capabilities on start and finish.

## Eval

Observer-only, in `eval/lib/value.py`:

| profile | metrics |
| --- | --- |
| Explorer | findings generated / accepted / verified |
| Reviewer | bugs found / confirmed |
| Worker | changes accepted / verification passed |

Spawn rate stays diagnostic. Logs without `profile_id` fall back to `role`.

## Future extension (not implemented)

- external agents / provider abstraction
- remote workers
- capability negotiation
- a marketplace of profiles
- UI cards, timeline, inspector — those should read this contract, not invent
  a parallel one
