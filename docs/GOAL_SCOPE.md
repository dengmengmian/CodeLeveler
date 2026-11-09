# Goal Scope Contract

How a goal's requirements are recorded, changed and enforced. A requirement is
a **model declaration anchored to a verbatim span of the user's message**: the
runtime validates identity, provenance, transition legality and order, and never
re-derives meaning from prose. Chinese, English, Japanese or a mixed message
work the same way because no word list is involved.

## Ownership

| Decision | Owner |
| --- | --- |
| What the user asked for (deliverables, constraints, prohibitions) | The model, reading the user's message |
| That the quoted span really occurs in the newest user input | Runtime (`quote_is_in_input`) |
| Which goal the requirement belongs to | Runtime (the turn's bound goal) |
| Requirement identity (`R1`, `R2`, …), state and order | `leveler_lifecycle::apply_scope_declaration` |
| The durable result | `leveler-storage` (ordered `goal_requirement_events`, migration 0055) |
| Whether an accounted requirement is mechanically satisfied | `leveler-agent::executor::scope_gate` |
| What the client shows | Runtime facts projected by TUI / Web / CLI |

A declaration is model output. It can create no user authority the quote does
not carry, and it proves nothing about the work. The quote proves the source
exists, not that the model's interpretation is correct; the original user
message stays in the transcript so a later reader can audit both.

## Requirement model

Each live requirement carries:

| Field | Meaning |
| --- | --- |
| `id` | Stable runtime identity (`R1`, `R2`, …). Never reused after a replace. |
| `kind` | What kind of evidence can decide it (see below). |
| `text` | The requirement as the model stated it. |
| `target` | The path, literal location or exact command, when the kind needs one. |
| `source_quote` | Verbatim span of the user message the declaration was anchored to. |
| `check` | Optional acceptance check the model associated with the requirement. |
| `state` | `active`, `waived` or `superseded`. |
| `superseded_by` | The replacement id when a `replace` retired this one. |

Kinds: `artifact`, `artifact_absent`, `command`, `named`, `content`, `tests`,
`forbidden`, `baseline`, `preparation`, `constraint`. Deliverables are the kinds
the gate holds the model to; `forbidden`, `baseline`, `preparation` and
`constraint` are context the model must honour but cannot be "completed".
`artifact`, `artifact_absent`, `content` and `command` require `target`.

## Declaration

Two structured entry points, both model-called:

- `commit_task(requirements=[{kind, text, target, source_quote, check}])` seeds
  the initial set when a Chat turn registers a goal. The seeds are applied in
  array order and mint `R1..Rn`.
- `declare_scope({operations: [...]})` changes an already bound goal's set. The
  same operation set serves `/goal` turns, resumed goals and supplements to a
  committed task.

```json
{
  "operations": [
    {"op": "add", "kind": "artifact", "text": "stage-2.txt exists",
     "target": "stage-2.txt", "source_quote": "创建 stage-2.txt"},
    {"op": "waive", "requirement_id": "R3",
     "source_quote": "这一步先不做了", "reason": "user deferred it"}
  ]
}
```

Operations: `add`, `amend`, `replace`, `waive`, `restore`. There is no delete:
`waive` stops owing the requirement while keeping its history, `replace`
supersedes the old id and mints a new one carrying the replacement, and
`restore` revives a waived requirement. A declaration applies in array order, so
a compound change is one event sequence, not several independent writes.

`get_goal_scope` reads the current set (ids, kinds, states, quotes, checks) and,
on request, the ordered change history with the turn that authorized each event.

## Validation

1. **Schema and bounds.** Unknown kinds/operations are refused; text, quote,
   target and reason have explicit length limits; one declaration carries at
   most `SCOPE_OPERATIONS_MAX` operations and the goal at most
   `SCOPE_REQUIREMENTS_MAX` live requirements.
2. **Provenance.** Every operation's `source_quote` must occur in the newest
   user-authored input of the transcript (exact, or with whitespace runs
   collapsed). An older message cannot authorize a change the newest one may
   already have reversed.
3. **Identity.** The target is the goal bound to this turn. An operation on an
   unknown or non-active requirement id is refused with its current state.
4. **Transition legality and order.** `amend` and `waive` require an active
   requirement; `restore` requires a waived one; `replace` supersedes an active
   requirement and mints the replacement. Events carry a monotonic per-goal
   `seq`, and a refusal leaves the recorded set unchanged.

## Durable history

`goal_requirement_events` stores, per goal, the `seq` order, the declaring
`turn_id`, the operation, the requirement identity, the kind/text/target, the
verbatim `source_quote`, the check, the reason and the timestamp. The current
set is a fold of that history — there is no second mutable copy that a resume,
a context fold or a crash can shrink. `record_scope` folds the history, applies
and validates the declaration, then appends its new events in one transaction.

## Completion gate

`update_goal(status="complete")` accounts for every active deliverable **by its
stable id** (`requirements=[{id, status, evidence}]`):

- `not_done` refuses completion; a missing id refuses completion;
- `unverifiable` is an explicit boundary, not an exemption from a fact the
  runtime can observe;
- `artifact` / `artifact_absent` are checked against the current workspace and
  recorded changes;
- `content` is checked as a line of the target file;
- `command` must match a successful recorded run after the last change — the
  evidence may cite `call_id:<id>` or the command line;
- `tests` must cite a passing recorded run after the last change, so no runner
  name or output format is recognised by vocabulary;
- `named` is accepted with an explicit "no mechanical check" warning.

The gate reads the durable set on every completion claim, so a compacted
transcript or a restarted process cannot narrow the scope. A model that never
declares a set has nothing for the gate to enforce; Goal mode therefore states
the declaration obligation in its contract, and `get_goal_scope` always shows
the exact set the gate will read.

## Verification report

Completion has two layers, and they are different kinds of fact:

| Layer | Source | What it establishes |
| --- | --- | --- |
| `update_goal(verification)` | Model declaration | Which executions the model says it ran |
| `VerificationRun` | Runtime execution record (`ToolCallStarted` + `ToolCallFinished`, background completion) | The command, its call id, exit status, workspace version and model step |

The gate resolves each citation against the record. A citation may name a
`call_id`, the exact command line, or the command plus a trailing note; a run
that failed, that predates the last change, or that a later run of the same
command contradicted is refused for that exact reason. Whether a command "is a
test runner" is never decided from its name or its output.

An accepted completion persists a `VerificationReport` on the task terminal
(`TaskFinished.verification`, and the same report on `TaskReport`):

```json
{
  "status": "unverified",
  "records": [
    {"requirement_id": "R1", "kind": "tests", "state": "verified",
     "detail": "verified against the execution record: `sh run-tests.sh` exited 0 at workspace version 4",
     "call_id": "call-12", "command": "sh run-tests.sh", "exit_code": 0,
     "workspace_version": 4, "model_step": 7}
  ]
}
```

`records` has one entry per owed requirement, in requirement order. A record
carries the evidence fields only when a recorded execution backed it; otherwise
`call_id`/`command`/`exit_code` are absent and `detail` says what the runtime
could not check. `workspace_version` is the mutation counter the verdict was
taken against, so a stale verdict is visible without replaying the transcript.

The four user-visible outcomes are projections of two facts, not a separate
computed verdict:

| Outcome | Representation |
| --- | --- |
| Verified | `TaskOutcome::Completed` + `VerificationStatus::Verified` |
| CompletedUnverified | `TaskOutcome::Completed` + `VerificationStatus::Unverified` |
| Incomplete | a non-delivery `StopReason` (budget, interruption) |
| Blocked | `TaskOutcome::Blocked` |

An unverified completion is accepted and delivered. It is not held for user
confirmation: the warning and the report tell the user what the runtime could
and could not prove, and the user decides what to do about it.

## Not in this contract

- No keyword, negation-word, section-heading, test-runner or status-word table.
- No extraction of requirements from prose — not at commit, not on resume, not
  after a fold.
- No second model call to classify the user's message; the running turn reads
  it and declares.
- No requirement needs human approval to be recorded; the user's quote is the
  source, and the runtime's job is identity and durability.
