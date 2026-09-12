# W3-A Engine Ownership Audit

## Status

```text
AUDIT_BASE=38e6a0304f8c38b65fe940853079e9fabadb3840
AUDIT_MODE=READ_ONLY
W3_A_AUDIT=COMPLETE
INDEPENDENT_REVIEW=AGREES
MODEL_INTELLIGENCE_BOUNDARY=PASS
GENERIC_LIFECYCLE_OWNER=FAIL
DOMAIN_SEMANTICS_ISOLATION=FAIL
W3_A_GATE=BLOCKED
FOUNDATION_FROZEN=NO
```

The gate is blocked by existing ownership violations. It is not blocked by a
test failure, and the audit found no path where the Engine infers that the
user's request is complete because tests passed or files changed.

## Authority Contract

The architecture defines three distinct owners:

```text
Model owns reasoning and judgment.
Harness owns domain semantics.
Engine owns lifecycle.
```

The Engine may persist facts supplied by a Harness, but it must not choose a
Coding workflow state, interpret Coding ledgers, organize Coding context, or
delegate a class of durable task lifecycle to the Product layer. Product code
may request and display lifecycle transitions; it does not become their
durable authority.

## Scope and Method

The audit inspected production dependencies and production call paths at the
review base. It traced:

- session, task, turn, ownership, recovery, and terminal writes;
- the `TaskEngine` and `TurnRunner` entry points;
- all production callers that bypass those entry points for lifecycle writes;
- typed event, seed, checkpoint, and recovery APIs exposed by
  `leveler-engine`;
- where outcome, verification, review, and completion decisions are made.

The audit did not modify code, schemas, CI, or tests. No broad refactor is
authorized by this document. A separate independent review inspected the same
base and agreed that the gate is blocked, including the session-creation and
long-goal persistence findings.

## Confirmed Ownership Violations

### 1. Session creation has two lifecycle writers

Normal and daemon session creation flow through `Application::insert_session`,
which directly calls `SessionRepository::create` and `set_execution`.
`TaskEngine::create_task` already implements the same lifecycle operation and
also creates the durable task row. The App path therefore leaves an intermediate
state where the session exists but its task does not; later paths compensate by
calling `ensure_for_session` themselves.

Evidence:

- `crates/leveler-app/src/session.rs:357-365`
- `crates/leveler-app/src/session.rs:409-444`
- `crates/leveler-engine/src/engine.rs:261-279`
- `crates/leveler-engine/src/engine.rs:337-362`

These are not two distinct meanings. They are two writers for the same durable
session/task creation fact.

Correct Owner: the App/Harness supplies workspace, goal, model, permissions,
and product axes. The Engine performs mechanical session and task creation.

Minimal repair: make normal and daemon creation call the existing
`TaskEngine::create_task` path and remove the App-side create/set-execution
path and task-row compensation. Keep product axes with their existing Product
owner. Only if initial axes must share the creation transaction should the
Engine accept them as opaque create values; it must not interpret them or grow
axes-specific branches. Do not add a Session manager or registry.

### 2. Long-goal writes are unfenced and split from terminal commit

Before `engine.run` acquires task ownership, the App opens or reuses a goal by
directly calling `TaskStore` and `GoalStore`. After the run, it records windows
and settles the goal with additional direct, best-effort writes. The goal store
operations carry no ownership token. A foreign-owned task can therefore receive
a goal write before the run rejects its ownership, and a process crash after the
fenced task terminal commit but before goal settlement can leave a completed
session with a permanently running goal.

Evidence:

- `crates/leveler-app/src/session.rs:656-672`
- `crates/leveler-app/src/session.rs:684-755`
- `crates/leveler-app/src/session.rs:1325-1358`
- `crates/leveler-agent/src/coding/run.rs:464-476`
- `crates/leveler-storage/src/goal_store.rs:1-14`
- `crates/leveler-storage/src/goal_store.rs:117-159`

Correct Owner: the Coding Harness decides whether an objective continues and
whether an outcome means work is still owed. The Engine owns authorized,
fenced persistence of goal identity, windows, and state.

Minimal repair: perform open/reuse only after `mark_running` returns an
ownership token, and move goal writes behind narrow Engine APIs that require
that token. Commit a Harness-supplied terminal goal disposition with the task
terminal fact when atomicity is required. Do not move objective comparison or
the `Completed`/`Blocked`/`Failed` semantic decision into the Engine, and do not
add a Goal manager.

### 3. Parallel parent lifecycle is owned by the Product layer

`leveler-app::parallel` creates the parent session and task, acquires task
ownership, writes `Running`, appends `TaskStarted`, derives terminal axes, and
calls `TerminalStore::finish_task_owned` directly. `TaskEngine::finish_task`
documents this as an exception and cites `§18.11`, but the architecture ends at
§18.10 and defines no such exception.

Evidence:

- `crates/leveler-engine/src/engine.rs:217-223`
- `crates/leveler-app/src/parallel.rs:86-140`
- `crates/leveler-app/src/parallel.rs:253-305`
- `crates/leveler-agent/src/coding/run.rs:687-713`

This violates the lifecycle and persistence boundaries even though normal and
parallel sessions are disjoint. Splitting rows by `ExecutionKind` does not
change the Owner of the durable fact.

Correct Owner: the Engine owns the parent session/task lifecycle. The Coding
Harness owns candidate selection, merge semantics, verification interpretation,
and the final outcome it reports.

Minimal repair: route the parent through `TaskEngine::create_task`,
`mark_running`, and `finish_task`. If an externally driven task needs one small
mechanical Engine entry point, add that entry point without adding a parallel
manager or a second runtime.

### 4. The Engine chooses a Coding workflow state

`TaskEngine::mark_running` writes both generic `SessionStatus::Running` and
`AgentState::Execute`. `leveler-lifecycle` explicitly classifies `AgentState`
as Coding workflow vocabulary rather than generic runtime vocabulary.

Evidence:

- `crates/leveler-engine/src/engine.rs:308-323`
- `crates/leveler-lifecycle/src/lib.rs:10-24`
- `crates/leveler-lifecycle/src/workflow.rs:1-82`

Correct Owner: the Coding Harness selects its workflow state; the Engine
persists the state and owns the generic running transition.

Minimal repair: make the caller provide the workflow state. Do not add a
generic state machine or teach the Engine to translate execution kinds into
Coding phases.

### 5. Generic turn seeding is fixed to Coding state

The public `TurnSeeds` contract contains `PlanState`, `EvidenceLedger`, and
`ProgressLedger`. The Engine decides when to load them and parses typed Coding
events to assemble the seed. The Harness correctly owns the existing
`prior_epoch_open` judgment, but a non-Coding Harness still has to pass through
a Coding-shaped Engine API.

Evidence:

- `crates/leveler-engine/src/turn.rs:92-103`
- `crates/leveler-engine/src/turn.rs:698-784`
- `crates/leveler-engine/src/turn.rs:856-879`
- `crates/leveler-lifecycle/src/lib.rs:17-24`
- `crates/leveler-agent/src/coding/run.rs:270-282`

Correct Owner: the Coding Harness owns the meaning and inheritance policy of
plan, evidence, and progress. The Engine owns ordered, strict, durable fact
lookup and turn lifecycle.

Minimal repair: move typed seed assembly and the fresh-turn inheritance rule to
`leveler-agent::coding`. Retain the Engine's indexed event lookup and transcript
mechanics. Do not introduce a seed registry, service locator, or generic domain
framework.

### 6. Goal checkpoint projection and triggers interpret Coding semantics

The Engine parses `EvidenceLedger` and `PlanState`, maps findings and
verification into `GoalCheckpoint`, chooses a goal, renders a model-visible
context block, and triggers an interrupted goal checkpoint from the generic
restart reaper. These are context-organization and domain-projection decisions,
which the architecture assigns to the Harness.

Evidence:

- `crates/leveler-engine/src/checkpoint.rs:45-95`
- `crates/leveler-engine/src/checkpoint.rs:121-183`
- `crates/leveler-engine/src/checkpoint.rs:234-367`
- `crates/leveler-engine/src/engine.rs:365-517`
- `crates/leveler-engine/src/reaper.rs:134-170`

Correct Owner: the Coding Harness chooses checkpoint semantics, projection, and
model-visible wording. The Engine and Storage own committed cursors, atomic
persistence, replay, and recovery of mechanical lifecycle facts.

Minimal repair: move projection, rendering, and domain triggers to Coding.
Keep a narrow Engine checkpoint commit/read boundary. Let generic reaping report
affected sessions, then let the composition/Coding path decide whether to write
a goal recap. Do not add a callback framework to the reaper.

### 7. Generic crash recovery embeds a workspace-specific instruction

The Engine's recovery error tells callers to inspect a workspace, and the
acknowledgement API describes the unknown side effect as manually verified in
that workspace. This is accurate for today's Coding CLI, but it is not a generic
lifecycle fact: another Harness may own a remote or non-workspace side effect.

Evidence:

- `crates/leveler-engine/src/lib.rs:101-104`
- `crates/leveler-engine/src/engine.rs:519-568`
- `crates/leveler-cli/src/cli.rs:196-200`

Correct Owner: the Engine reports that a side effect may have occurred and was
not replayed. The Product or Harness supplies the concrete recovery instruction.

Minimal repair: make the Engine wording domain-neutral and retain the workspace
instruction at the CLI/App boundary.

## Suspicious Coupling That Is Not a Current Gate Violation

`EngineEvent` contains Plan, Goal, Review, Evidence, Candidate, Workspace, and
UserShell variants. The Coding Harness currently constructs the domain facts;
the Engine mainly persists, replays, classifies, and projects them. Legacy Node,
Repair, and Window variants remain for replay. The audit found no Engine branch
that turns one of these events into a semantic completion decision.

This is compile-time coupling between a generic Engine and Coding/product event
vocabulary, but the typed schema currently supplies real replay and data-class
safety. W3 should first remove the Engine's active interpretation of Coding
payloads. A domain-event envelope should wait for a real second Harness; this
audit does not justify an event registry, type-erased JSON bus, schema rewrite,
or deletion of legacy variants.

`WorkspaceFacts` also keeps Git acquisition in Coding and gives the Engine only
bounded facts. That separation is correct in isolation. Its Engine-facing type
is coupled to the checkpoint violation above and should be narrowed as part of
that repair, without adding a Workspace manager.

## Boundaries That Are Correct

- `TurnRunner::run_turn` opens the turn, commits initiating input, pumps events
  persist-before-forward, fences writes, classifies cancellation mechanically,
  and commits the terminal fact atomically.
- Tool selection and execution remain outside the Engine. The Engine records
  start/finish/risk facts and supplies durability and ownership barriers.
- Lost-child detection and `ok: false` settlement are mechanical lifecycle
  facts. `LostChildVoice` correctly returns contribution meaning to the Harness.
- The Engine records generic model messages, errors, usage, and cost arithmetic;
  it does not select a provider or alter provider semantics.
- Transcript watermarking, strict/lossy replay, token thresholds, and fold
  mechanics are generic. Semantic summarization is supplied through
  `ContextSummarizer`.
- Verification plans, baseline interpretation, review eligibility, review
  verdicts, and task outcome interpretation remain in
  `leveler-agent::coding`. The Engine persists the outcome supplied by the
  Harness and does not infer domain completion.
- `NewSession` carries workspace, goal, model, mode, and sandbox as opaque
  durable values. `TaskEngine::create_task` writes them without interpreting
  them.

## Non-Blocking Dependency Debt

`tempfile` is used by `crates/leveler-engine/tests/minimal_harness.rs` but is a
normal `leveler-engine` dependency. It belongs in `[dev-dependencies]`. This
does not block the ownership gate by itself.

## Required Repair Order

1. Unify normal, daemon, and parallel parent session/task creation behind
   `TaskEngine`.
2. Fence long-goal writes and close the task-terminal/goal-settlement crash
   window without moving goal semantics into the Engine.
3. Stop the Engine from selecting `AgentState::Execute`.
4. Move Coding typed seeds and goal-checkpoint projection/triggers to Coding,
   retaining Engine persistence and recovery mechanisms.
5. Make generic crash-recovery wording domain-neutral.
6. Move `tempfile` to dev-dependencies and add a non-Coding minimal-Harness
   boundary test proving run/resume/reap do not require Coding plan, evidence,
   progress, workspace, or checkpoint types.

The repair must preserve event replay, ownership fencing, crash consistency,
and the one durable store. It must not add an Engine manager, registry, factory,
generic Workspace framework, second checkpoint store, or parallel lifecycle
subsystem.

## Gate Decision

```text
ENGINE_INFERS_DOMAIN_COMPLETION=NO
ENGINE_SELECTS_CODING_WORKFLOW_STATE=YES
ENGINE_INTERPRETS_CODING_SEEDS=YES
ENGINE_INTERPRETS_CODING_CHECKPOINTS=YES
PRODUCT_CREATES_RUNTIME_SESSIONS_DIRECTLY=YES
LONG_GOAL_WRITES_OWNERSHIP_FENCED=NO
TASK_TERMINAL_AND_GOAL_SETTLEMENT_ATOMIC=NO
PRODUCT_OWNS_PARALLEL_PARENT_LIFECYCLE=YES
ONE_LIFECYCLE_OWNER=NO
W3_A_GATE=BLOCKED
NEXT_ACTION=ARCHITECTURE_REPAIR_PLAN_AND_APPROVAL
```

W3-A is complete as an audit. Foundation cannot be frozen at this base. The
next task is a scoped architecture repair, not another evidence-only gate.
