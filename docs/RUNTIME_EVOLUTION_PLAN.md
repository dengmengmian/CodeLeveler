# CodeLeveler Runtime Evolution Plan

> Status: Target roadmap  
> Baseline: `main @ 3355bfb`  
> Target architecture: [`FUTURE_RUNTIME_ARCHITECTURE.md`](FUTURE_RUNTIME_ARCHITECTURE.md)
>
> Core rule:
>
> **Stabilize and generalize the Runtime Core before expanding product surfaces.**

This roadmap maps the current repository architecture to the long-term Durable Agent Runtime target. It intentionally avoids a big-bang rewrite. Existing boundaries that are already correct should be preserved; boundaries that are too Coding-specific should be generalized gradually; new crates and services should be created only when a real dependency boundary requires them.

---

# 1. Current architectural baseline

The current system is no longer a simple `CLI -> Agent -> Tool` program.

The practical execution path is approximately:

```text
TUI / CLI / Web / Mobile
        |
        v
leveler-client-protocol / transport
        |
        v
leveler-app
Composition + interactive runtime service
        |
        v
leveler-engine
Task / Turn / Event / Recovery / Completion owner
        |
        +----------------------+
        |                      |
        v                      v
leveler-agent             leveler-verifier
Model <-> Tool loop       Coding completion gate
        |
        v
ToolHost
(currently inside leveler-agent executor)
        |
        v
leveler-tools
        |
        v
leveler-execution
        |
        v
Workspace / Process / Sandbox

Durable substrate:
leveler-storage + SQLite + event log

Model path:
leveler-model <- leveler-protocol <- leveler-provider
```

This baseline already contains several important runtime properties that must not be lost:

- engine-owned task/turn lifecycle;
- canonical event persistence;
- durable tool side-effect ordering;
- crash reconciliation;
- typed task outcomes;
- one ToolHost admission path for model-proposed tool calls;
- permission / approval / sandbox enforcement in host code;
- verification-controlled Coding completion;
- session-scoped runtime event subscriptions;
- command envelopes and idempotency support;
- local daemon / socket runtime;
- Web and Mobile using the same runtime semantics rather than owning separate Agent loops.

The next architecture phase should build on these properties rather than replace them.

---

# 2. Current -> Target gap summary

| Area | Current state | Target state | Gap severity |
| --- | --- | --- | --- |
| Engine | Durable task/turn engine, but TaskSpec and repositories are Coding/SQLite-shaped | Generic Durable Agent Execution Kernel | High |
| Task model | Session largely acts as Task identity | Explicit Task distinct from Session | High |
| Lifecycle vocabulary | Generic outcomes mixed with Coding workflow states | Runtime lifecycle separated from domain workflow | High |
| Storage | SQLite concrete repositories; EventStore already has useful seam | Narrow storage ports + SQLite/cloud adapters | High |
| Ownership | Process/session admission only | RuntimeId + Task owner + OwnerEpoch/fencing | High before cloud |
| Agent | Strong direct loop, but still contains policy/domain responsibilities | Model/context/tool loop only | Medium |
| ToolHost | Correct logical boundary, physically inside Agent executor | Same boundary; extraction only if dependencies require it | Low now |
| Tools | Good capability layer | Keep capability layer | Low |
| Execution | Good host mechanism layer | Keep host mechanism layer | Low |
| Verifier | Correct Coding completion gate | Remain Coding-specific until a second domain proves generic gate API | Low now |
| App | Local composition root + runtime service | Local composition root; reusable runtime construction seam | Medium |
| CLI | Broad dependencies and some direct composition knowledge | Thin command/interface adapter | Medium / later |
| Client protocol | Already multi-client and version-aware | Extend to Task/Runtime ownership/cloud semantics | Medium |
| Local transport | Local daemon seam works | Durable Local Runtime/Supervisor | Medium |
| Web | Local/daemon browser adapter | Local + future remote client over same semantics | Medium / later |
| Mobile/Remote | Real remote-control slice | Reuse for future owner routing, but not cloud execution engine | Medium / later |
| Background processes | In-process subprocess registry | Keep separate from durable Agent task scheduling | Architectural guard |
| Scheduler | None at Runtime level | Durable wake/create/resume scheduler | Future |
| NPC | Architecture concept only | Domain runtime above shared core | Future |

The immediate work is therefore not “add NPC” or “add cloud”. The immediate work is to close the high-severity core gaps while preserving current Coding behavior.

---

# 3. Crate-by-crate target responsibilities

## 3.1 `leveler-cli`

### Current role

`leveler-cli` owns user-facing subcommands and currently knows about many lower-level crates: app, engine, agent-related types, storage, execution, eval, local/remote transport, TUI, Web, and supporting features.

It starts:

- one-off runs;
- TUI;
- resume;
- `serve` daemon;
- Web;
- remote flows;
- eval and setup commands.

### Target role

`leveler-cli` should be an **interface adapter**, not a second composition root.

It should primarily depend on:

```text
leveler-app
leveler-client-protocol
TUI/Web/remote surface crates
CLI-specific config/output
```

It may still use eval/admin crates for explicit commands, but normal runtime execution should enter through `leveler-app` or a stable runtime client.

### Planned change

Not P0.

After core boundaries stabilize:

- move repeated runtime assembly decisions behind `leveler-app`;
- reduce direct use of storage/engine/execution internals from ordinary commands;
- keep CLI parsing and user-facing output in CLI;
- do not rewrite CLI while Task/Storage ownership is changing.

### Decision

**Keep crate. Do not split. Narrow dependencies later.**

---

## 3.2 `leveler-app`

### Current role

`leveler-app` is the local product composition root. It assembles configuration, providers, model selection, SQLite storage, execution policy, tools, memory, verification and `TaskEngine`. It also provides the in-process interactive runtime implementation and active-turn ownership for local sessions.

This is a valid responsibility today.

### Target role

`leveler-app` remains the **local CodeLeveler composition root**.

It should own:

- resolving host/project configuration;
- choosing local storage adapter;
- constructing runtime dependencies;
- creating local runtime service/client adapters;
- product defaults for Coding mode.

It should not become the universal cloud/control-plane crate.

Future cloud workers may compose the same lower-level runtime libraries from a different service crate.

### Planned change

As Engine store ports and Runtime identity are introduced:

- inject storage ports instead of assuming concrete DB paths deep in Engine;
- construct `RuntimeId` and local ownership services;
- keep Coding configuration assembly here;
- move only truly reusable runtime construction helpers downward if both local app and cloud worker need them.

### Decision

**Keep crate. Keep as local composition root. Do not turn it into a God Runtime crate.**

---

## 3.3 `leveler-engine`

### Current role

This is already the strongest candidate for the Runtime Kernel.

It owns or coordinates:

- task/session execution;
- turn boundaries;
- canonical lifecycle writes;
- event log;
- transcript persistence barriers;
- crash recovery;
- dangling tool reconciliation;
- supervisor continuation;
- verification orchestration;
- terminal outcome commitment.

It is intentionally the single writer of terminal task/session lifecycle facts.

### Current gap

The engine still exposes Coding-shaped concepts, especially through task specifications and verification/repository data, and still holds concrete SQLite-oriented repository access in key paths.

### Target role

`leveler-engine` becomes the **Durable Agent Execution Kernel**.

It should own only generic execution semantics:

```text
Task lifecycle
Turn lifecycle
Canonical events
Runtime ownership checks
Cancellation
Supervision
Interaction waiting
Recovery
Resume
Terminal outcome commitment
Hard resource limits
```

Coding-specific decisions should be supplied through domain policy/configuration rather than assumed by the kernel.

### Planned change

Priority order:

1. introduce explicit Task identity without deleting Session;
2. separate generic runtime state from Coding workflow state;
3. replace concrete storage access with narrow store ports;
4. add RuntimeId / owner epoch validation;
5. keep verifier invocation as a domain completion dependency rather than absorbing Coding verification logic;
6. preserve canonical event ordering and side-effect durability invariants throughout migration.

### Decision

**Keep crate. Promote conceptually to Runtime Kernel. Do not create `leveler-runtime` just to rename it.**

A new runtime crate is justified only if Engine becomes impossible to depend on from both local and cloud compositions without pulling Coding application concerns. That is not the current state.

---

## 3.4 `leveler-agent`

### Current role

`leveler-agent` runs the model <-> tool feedback loop and also contains several higher-level concerns such as:

- budgets/continuation;
- compaction/context behavior;
- gates and closeout logic;
- plan/progress/evidence behaviors;
- sub-agent execution;
- named agent behavior;
- authorization-related orchestration;
- ToolHost implementation under the executor.

### Target role

The long-term responsibility should be narrower:

```text
Model request
Streaming/model response interpretation
Context/compaction
Tool proposal batching
ToolHost invocation
Tool result feedback
Turn outcome
Delegated direct loops
```

Domain workflow should not accumulate indefinitely in this crate.

### Planned change

Do not perform a large extraction immediately.

Instead:

- mark which modules are direct-loop mechanics vs Coding/product policy;
- make policies injectable where they already have stable interfaces;
- move Coding-only lifecycle decisions upward only when Task/lifecycle model is ready;
- preserve sub-agent use of the same ToolHost boundary;
- do not create a generic workflow engine inside Agent.

### Decision

**Keep crate. Gradually shrink responsibilities. No big-bang rewrite.**

---

## 3.5 ToolHost (`leveler-agent/src/executor/host.rs` today)

### Current role

ToolHost is already the logical boundary where a model-proposed tool call becomes execution.

It coordinates:

```text
Durability barrier
Hooks
Permission rules
Profile policy
Approval
Execution admission
Tool execution
Attributed child activity
Recovery reconciliation
```

This is a critical invariant and one of the best parts of the current architecture.

### Target role

The role does not materially change.

All local, cloud, container-hosted, Coding, NPC and delegated model-proposed tool execution must continue through this one conceptual boundary.

### Should it become `leveler-tool-host`?

**Not now.**

Extraction is justified only when one of these becomes true:

1. another top-level runtime needs ToolHost without depending on `leveler-agent`;
2. dependency direction forces a cycle;
3. ToolHost's public contract stabilizes enough to be independently versioned;
4. cloud/NPC domain code starts importing Agent internals only to access ToolHost.

Until then, physical extraction adds crate/trait overhead without product value.

### Decision

**Freeze the logical boundary; defer physical crate extraction.**

---

## 3.6 `leveler-tools`

### Current role

Capability layer:

- Tool trait and schema;
- Tool registry;
- built-in tools;
- MCP integration;
- per-tool risk/parallelism/mutation metadata;
- ToolContext connecting capabilities to workspace/execution services.

### Target role

Keep exactly this conceptual role:

> **Tools describe available capabilities and convert validated intent into calls to host mechanisms.**

### Guardrail

`ToolContext` must not become the universal application context.

Do not add:

- Session repository;
- cloud Task owner;
- UI state;
- Control Plane client;
- generic scheduler;
- NPC identity.

If a tool needs one of those, provide a narrow service interface rather than growing an EverythingContext.

### Decision

**Keep crate and boundary. No split/merge planned.**

---

## 3.7 `leveler-execution`

### Current role

Low-level host execution mechanism:

- workspace/path safety;
- command/process execution;
- process-tree cancellation;
- approval vocabulary;
- permission/sandbox mechanics;
- checkpoints/snapshots;
- hooks;
- background subprocesses;
- OS-specific isolation.

### Target role

Keep this as the **host mechanism / safety kernel** beneath Tools and Verifier.

Cloud/container execution should reuse this layer with environment-specific adapters rather than create a separate cloud executor.

### Important distinction

The current `BackgroundTaskRegistry` manages subprocesses spawned by an Agent/tool. It is not and must never silently evolve into the durable Runtime Task Scheduler.

```text
BackgroundTaskRegistry
= child process lifecycle inside one runtime

Future Scheduler
= durable Agent task wake/create/resume infrastructure
```

### Decision

**Keep crate. Keep background-process semantics separate from future Task scheduling.**

---

## 3.8 `leveler-storage`

### Current role

Foundational SQLite persistence and repositories for sessions, turns, events, messages, model requests, artifacts and related projections/migrations.

Its dependency direction is good: storage should remain foundational and should not depend back on Agent/Engine product logic.

### Current gap

Higher runtime layers still rely on concrete `Database` and repository implementations in places, which makes future cloud storage harder.

### Target role

`leveler-storage` remains persistence infrastructure, but implementations sit behind narrow runtime storage contracts.

Possible long-term shape:

```text
Runtime storage ports
   |
   +-- SQLite implementation (leveler-storage)
   +-- Cloud/Postgres implementation (future service/adapter)
   +-- memory implementation (tests)
```

Do not make `leveler-storage` depend on PostgreSQL merely because cloud will exist. A separate adapter crate/service may be cleaner once cloud exists.

### Decision

**Keep crate. Do not merge with Engine. Abstract callers, not the SQLite internals prematurely.**

---

## 3.9 `leveler-lifecycle`

### Current role

Shared typed vocabulary used across storage, engine and agent. It contains both broadly reusable lifecycle/result concepts and Coding/product-specific structures such as plan/evidence/readiness/impact and detailed Agent states.

### Current gap

This crate is the most obvious semantic mixing point for future NPC/general runtime support.

### Target role

Keep one crate initially, but logically separate modules into two categories.

Generic Runtime vocabulary:

```text
TaskId / runtime status
TaskOutcome
TurnOutcome
RuntimeId / ownership
Task contract primitives that are truly generic
```

Coding/domain vocabulary:

```text
PlanState
EvidenceLedger (where Coding-shaped)
ChangeImpact
Readiness / TaskClass
Coding workflow phase/state
```

### Should a new `leveler-task` or `leveler-runtime-types` crate be created?

**Not now.**

First create module-level boundaries inside `leveler-lifecycle`. Extract a new crate only when:

- cloud worker or NPC needs generic lifecycle types but would otherwise pull substantial Coding dependencies/API;
- generic types have stabilized;
- extraction reduces dependency direction rather than just moving files.

### Decision

**Keep crate now; make semantic split explicit before physical split.**

---

## 3.10 `leveler-verifier`

### Current role

Coding completion gate. It runs scoped format/build/test checks, captures evidence, classifies failures and contributes to the engine's terminal decision.

This is correctly outside the Agent Loop.

### Target role

Keep it Coding-specific for now.

Do not rename it to a universal `leveler-validation` crate before NPC or another real domain demonstrates a shared completion-gate abstraction.

Future conceptual interface may become:

```text
CompletionPolicy / CompletionGate
   |
   +-- CodingVerifier
   +-- future domain gate
```

But the current verifier implementation should not be generalized speculatively.

### Decision

**Keep crate. Keep Engine -> Verifier relationship. Do not move verification into Agent.**

---

## 3.11 `leveler-client-protocol`

### Current role

Stable semantic seam between runtime and first-class clients. It already carries commands, runtime events, snapshots, session-scoped subscription semantics, command envelopes/idempotency, and protocol compatibility concepts.

### Target role

This should become the base contract for:

- TUI;
- Web;
- Mobile;
- local daemon;
- future remote/cloud routing where semantics match.

Extend rather than replace it with new Task/Runtime concepts:

```text
TaskId
RuntimeId
ExecutionLocation
TaskCreated/Assigned/Transferred
RuntimeOnline/Offline
Ownership transfer status
```

Compatibility mappings should preserve current session-oriented commands during migration.

### Decision

**Keep crate. Evolve protocol additively. Do not create cloud-protocol-v2 as a parallel semantic model.**

---

## 3.12 `leveler-local-transport`

### Current role

Local runtime service seam and Unix/TCP transport for daemon clients.

This already proves that TUI/Web do not need to own the Agent process directly.

### Target role

Remain the local transport implementation.

The future Local Runtime Supervisor should build around this seam rather than replace it.

Cloud transport may use another protocol implementation, but command/event semantics should remain based on `leveler-client-protocol`.

### Decision

**Keep crate. Do not turn it into a generic distributed systems library.**

---

## 3.13 `leveler-tui`

### Current role

Primary Coding UI over `InteractiveRuntimeClient`.

### Target role

Remain the most complete Coding client.

TUI should consume:

- canonical snapshot/event state;
- Task/Runtime location projection;
- approvals/clarifications;
- diff/verification/plan/sub-agent projections.

TUI must never regain ownership of Task lifetime merely because it is the main UX.

### Decision

**Keep as client. No runtime logic migration into TUI.**

---

## 3.14 `leveler-web`

### Current role

Browser UI/server bridging SPA clients to a `LocalRuntimeService`, supporting in-process runtime, daemon connection and multi-project routing.

The newest WebSocket implementation uses per-session event streams for session facts and global events only for cross-session facts. That is the correct direction.

### Target role

Local Web remains a client adapter over local runtime.

Future remote Web should reuse the same state model/reducers but connect through remote/control-plane transport rather than expose the local loopback server publicly.

For a possible online-programming/container product, Web may simply be the user-facing client of a container-hosted CodeLeveler runtime.

### Decision

**Keep crate. Do not make Web a Control Plane.**

---

## 3.15 Remote / Mobile / Relay crates

Relevant current pieces include remote protocol, remote agent, relay, mobile app and session-wire support.

### Current role

They implement a real secure remote-control slice:

- pairing/device identity;
- signed messages;
- runtime RPC routing;
- remote restrictions;
- mobile session/conversation interaction.

### Target role

Preserve this as the Local Runtime remote-control path.

Future Cloud Control Plane is a broader service and should not be implemented by gradually turning Relay into:

- task database;
- cloud scheduler;
- worker executor;
- second Agent runtime.

Some identity/routing concepts may be reused, but responsibilities stay explicit.

### Decision

**Keep current remote stack. Extend only where it remains remote-control infrastructure. Create Control Plane later as a separate service boundary when cloud work begins.**

---

## 3.16 Model / Provider / Protocol crates

### Current role

- `leveler-model`: provider-neutral model types;
- `leveler-protocol`: wire/protocol adaptation;
- `leveler-provider`: provider selection/configuration/runtime adapter.

### Target role

This separation is already directionally correct.

AgentGate or any other gateway is external to CodeLeveler architecture. If compatible, it is simply one configured provider endpoint/route from CodeLeveler's perspective.

### Decision

**Keep these boundaries. No AgentGate dependency.**

---

## 3.17 Supporting Coding crates

`leveler-project`, `leveler-vcs`, `leveler-lsp`, `leveler-skills`, `leveler-context`, `leveler-memory`, `leveler-media` and related support crates provide real product value.

They should not all be forced into a new `leveler-coding` crate immediately.

A future Coding domain facade becomes useful only when generic Engine/App/NPC composition needs a single Coding policy bundle and current support dependencies start leaking into generic Runtime code.

### Decision

**Do not consolidate or split now. Treat them as supporting capabilities and clean dependency direction incrementally.**

---

## 3.18 `leveler-eval`

### Current role

Existing evaluation harness used for real capability/regression checks and policy ablations.

### Target role

The eval framework is essential to architecture migration.

Do not create a parallel benchmark system.

Add migration-specific cases for:

- Task/Session compatibility;
- storage-port contract behavior;
- owner fencing;
- daemon/TUI disconnect survival;
- crash recovery across ownership boundaries;
- client handoff;
- eventual cloud/worker scenarios.

### Decision

**Keep and extend existing eval architecture.**

---

# 4. Boundaries that are already correct and should be protected

The following should be treated as architectural assets, not refactor targets:

## 4.1 Engine is the lifecycle writer

Do not allow App, Agent, UI or Verifier to independently stamp competing terminal lifecycle facts.

## 4.2 ToolHost is the only model-proposed execution path

Do not add shortcuts for cloud, NPC, remote or sub-agents.

## 4.3 Persist before side effect

The runtime must durably establish the ToolCallStarted/side-effect boundary before execution and durably finish it afterwards.

## 4.4 Recovery distinguishes risk from replay safety

`Safe` approval risk is not equivalent to “safe to replay after crash”. Preserve explicit replay semantics.

## 4.5 Storage is a substrate, not a pipeline stage

Do not create `execution -> storage -> verifier` coupling. Engine owns durable runtime truth; Execution remains lower-level host mechanism.

## 4.6 Verifier is an Engine sibling capability

Model-requested execution uses Agent -> ToolHost -> Tools -> Execution.

Host-initiated Coding verification may use Engine -> Verifier -> Execution.

These are different trust paths and should remain explicit.

## 4.7 Clients consume one runtime contract

TUI, Web and Mobile must not recreate separate Task state machines.

---

# 5. Boundaries that need to change before Cloud/NPC

## 5.1 Session cannot remain the permanent Task identity

Migration must introduce Task explicitly while keeping backward compatibility.

## 5.2 Runtime lifecycle cannot remain Coding-shaped

Generic operational state must be separated from Coding workflow phases.

## 5.3 Engine cannot remain tied to concrete SQLite repositories

Introduce narrow ports gradually.

## 5.4 Process-local ActiveTurns is not distributed ownership

It remains useful for local per-session admission but cannot represent Task Owner across runtimes.

## 5.5 BackgroundTaskRegistry is not Scheduler

Keep these concepts separate permanently.

---

# 6. What should NOT be split now

To prevent architecture churn, the following extractions are explicitly deferred.

| Proposed split | Decision now | Revisit when |
| --- | --- | --- |
| `leveler-runtime` | Do not create | Engine cannot serve local/cloud compositions cleanly |
| `leveler-task` | Do not create | Generic lifecycle types are stable and lifecycle crate becomes a dependency problem |
| `leveler-runtime-types` | Do not create | Same as above |
| `leveler-tool-host` | Do not create | Multiple top-level consumers need ToolHost without Agent internals |
| `leveler-coding` | Do not create | NPC/second domain proves a real domain facade is needed |
| universal `leveler-validation` | Do not create | Second domain proves a shared completion contract |
| workflow DSL crate | Do not create | Two or more real workflows cannot be expressed with existing policy seams |
| cloud-specific Agent crate | Never as parallel core | Cloud is a deployment of the same runtime |

The desired architecture is defined by ownership and contracts, not by maximizing crate count.

---

# 7. Phase 0 — Finish current runtime stability

Priority: **P0 — blocking**

Before new architecture work becomes default, close the current stability baseline.

Required classes of validation:

- long-running execution / soak;
- repeated TUI task runs;
- daemon reconnect;
- Web/TUI handoff;
- real Mobile device/cellular path;
- crash recovery;
- approval and clarification;
- resource growth;
- Windows/Linux/macOS host validation according to supported feature matrix.

The key reason is diagnostic isolation: distributed-runtime work must not hide pre-existing local-runtime instability.

### Gate

No Cloud Worker, Scheduler or NPC implementation becomes the main development priority until this baseline is signed off.

---

# 8. Phase 1 — Generic Task model without breaking Session

Priority: **P1**

> Status: **landed (task identity slice)**. `TaskId` is a typed identity in
> `leveler-core`; the `tasks` table (migration 0016) records the 1:1
> task↔primary-session association with a deterministic legacy backfill
> (task id = session id); the engine ensures the association at creation and
> at every execution entry, and `TaskStarted` carries the task id additively.
> `TaskSpec` is split into `RuntimeTaskSpec` (goal/kind/continuation/limits)
> and `CodingTaskSpec` (repository/mode/sandbox/verification/base_commit).
> `RuntimeId` is intentionally not introduced yet — nothing consumes it.

Introduce the minimum Task abstraction required by future runtimes.

## 8.1 New generic identities

```text
TaskId
RuntimeId
```

## 8.2 Task association

Initial migration may maintain a simple 1:1 relationship:

```text
Task
  |
  +-- primary Session
```

Do not immediately support arbitrary multi-session workflows just because the schema allows them.

## 8.3 Task fields

Minimum generic shape:

```text
TaskId
Goal / contract reference
Domain
Lifecycle status
Execution location
Owner runtime (nullable before assignment)
Owner epoch
Created / updated timestamps
Terminal outcome
```

Coding repository/base-commit/verification data stays in Coding task configuration, not generic identity.

## 8.4 Compatibility

Existing session ids and resume flows must continue working through an explicit mapping.

### Acceptance

- existing DB migrates automatically;
- old sessions remain resumable;
- TUI behavior unchanged;
- task list can be reconstructed from canonical data;
- no generic Task API requires Coding-only `AgentState` values.

---

# 9. Phase 2 — Separate generic runtime lifecycle from Coding workflow

Priority: **P1**

> Status: **first semantic split landed**. `leveler-lifecycle` now has a
> `runtime` module (SessionStatus/TaskOutcome/TurnOutcome) that must not
> reference the Coding `workflow` module (AgentState). All original paths are
> re-exported; wire strings and the `sessions.state` column are unchanged.
> The remaining Coding vocabulary (plan/progress/readiness/…) still lives at
> the crate top level pending a consumer that needs the physical split.

Refactor `leveler-lifecycle` semantically before splitting crates.

Generic status example:

```text
Created
Queued
Assigned
Running
WaitingInteraction
Suspended
Blocked
Completed
Failed
Cancelled
```

Keep `TaskOutcome` / `TurnOutcome` typed and separate from operational status.

Coding states such as localization, plan checking, repair and review become Coding workflow state/projection.

### Acceptance

NPC/future domain code can depend on generic lifecycle without needing Coding workflow semantics.

---

# 10. Phase 3 — Runtime storage ports

Priority: **P1/P2**

> Status: **landed**. The engine consumes only narrow ports — `EventStore`,
> `TaskStore`, `SessionStore`, `TurnStore`, `MessageStore`,
> `ModelRequestStore`, `TerminalStore` — bundled in the plain `EngineStores`
> composition struct; `TaskEngine`/`TurnRunner` no longer hold a concrete
> `Database`. Terminal task/turn commits stay single-transaction inside the
> SQLite adapter (`TerminalRepository` unchanged); the memory terminal store
> simulates the same atomic semantics and offers injected commit failures.
> `leveler-app` remains the composition root wiring
> `EngineStores::from_database`. SQLite is still the only production
> adapter; app/CLI/Web convenience queries (session list, checkpoints,
> command receipts) intentionally stay on concrete repositories.

The existing EventStore seam should be preserved and expanded with narrow contracts where necessary.

Possible shape:

```text
EventStore
TaskStore
SessionStore
TurnStore
MessageStore
CommandReceiptStore
OwnershipStore
ArtifactMetadataStore
```

Do not introduce:

```text
trait Database { everything... }
```

The first adapter remains SQLite.

Memory/test implementations should exercise the same semantic contracts.

PostgreSQL is not required in this phase.

### Acceptance

Engine core unit/integration tests can run against test stores without depending on SQLite-specific APIs where persistence semantics are not under test.

---

# 11. Phase 4 — Runtime ownership and fencing

Priority: **P1/P2 — required before cloud**

Introduce:

```text
TaskOwner {
  task_id,
  runtime_id,
  owner_epoch,
  lease/heartbeat metadata
}
```

Every canonical task write in distributed-capable mode must prove current ownership.

### Required fault tests

```text
A owns task at epoch 5
network partition
B acquires epoch 6
A reconnects
A attempts append/complete/approval resolution
=> rejected as stale owner
```

Do not interpret lease expiry alone as permission to blindly replay an in-flight external side effect. Existing dangling-call recovery rules remain authoritative.

---

# 12. Phase 5 — Local Runtime Supervisor / durable daemon

Priority: **P2**

> Status: **landed (single-host slice)**. `RuntimeId` is a typed identity in
> `leveler-core`, persisted per state directory (flock-guarded first mint,
> atomic write, corrupt-file = hard error) and stable across daemon restarts.
> `leveler serve` remains the one daemon; its socket bind now holds an
> exclusive lock for the daemon's lifetime, so concurrent starts elect
> exactly one runtime. The default Unix TUI path is discover-or-start
> (detached spawn + ready-json + connect-with-retry) with `--in-process` as
> the explicit embedded mode. Clients read `RuntimeInfo` (identity +
> diagnostics) through an additive local-transport request. Lifecycle
> invariants are locked by process-level E2E (restart-stable identity,
> daemon election, SIGKILL recovery) and transport-level tests (disconnect
> never cancels, explicit cancel fires once, session isolation).
> Not in scope yet: health/capacity reporting, restart supervision of the
> daemon itself, and remote registration.

Build on existing `leveler serve` and local transport.

Goal:

> accepted work survives client departure and local runtime lifecycle is explicit.

Target:

```text
TUI / Web / Mobile bridge
        |
        v
Local Runtime Supervisor
        |
        v
Project Runtime / Engine
```

Responsibilities:

- RuntimeId;
- daemon lifecycle;
- project runtime availability;
- restart/recovery;
- health;
- active task capacity;
- optional remote registration.

`--in-process` can remain as fallback/dev mode.

### Acceptance

```text
start task in TUI
close TUI
runtime continues
open Web/TUI again
same task and canonical sequence visible
```

and runtime process restart produces explicit resume/recovery behavior rather than silent loss.

---

# 13. Core Stability Gate

This gate separates **Core Architecture Work** from **Product Expansion Work**.

The following must be stable enough before Cloud/Scheduler/NPC becomes the main line:

1. Task/Session model has compatibility tests;
2. generic lifecycle is no longer tied to Coding phases;
3. Engine storage boundary is port-based where cloud requires it;
4. ownership/fencing semantics have deterministic failure tests;
5. Local Runtime survives TUI disconnect and recovers after daemon restart;
6. ToolHost/side-effect recovery invariants remain green;
7. existing Coding eval and real-project tests show no material regression.

Until this gate is passed, optimization effort should focus on the Runtime Core rather than new product features.

---

# 14. Phase 6 — Optional container-hosted validation

Priority: **P2/P3, opportunistic**

Container hosting is not a mandatory product milestone.

If a real project needs online coding, validate that existing runtime interfaces work inside a container:

```text
Browser/Web or terminal
       |
       v
CodeLeveler Runtime in container
```

Prefer existing Web/TUI/client-protocol capabilities.

Do not build a separate SDK unless a real integration proves the existing runtime interface is insufficient.

The value of this phase is architectural validation: it demonstrates that the core is not accidentally coupled to one desktop/TUI process.

---

# 15. Phase 7 — Control Plane MVP

Priority: **P3 — after Core Stability Gate**

Create a separate service boundary only when cloud/multi-runtime work begins.

Responsibilities:

- identity/device/runtime directory;
- task directory/projections;
- runtime heartbeat;
- command routing to owner;
- event projection;
- notification.

It must not own Agent reasoning or execute model-proposed tools.

Current Relay remains a remote-control relay and must not silently become this entire service.

---

# 16. Phase 8 — Remote Web/Mobile parity

Priority: **P3**

Extend current clients rather than rewrite them.

Complete important projections:

- Task location/owner state;
- conversation;
- plan;
- tool activity;
- diff;
- verification;
- evidence;
- approvals;
- clarification;
- steering;
- cancel/resume.

TUI remains the highest-productivity Coding interface. Mobile optimizes for observation and decision points.

---

# 17. Phase 9 — Cloud Worker

Priority: **P4**

Cloud Worker composes the same Runtime Core from a cloud service environment.

```text
Cloud Worker
  |
  +-- leveler-engine
  +-- leveler-agent
  +-- ToolHost
  +-- leveler-tools
  +-- leveler-execution
  +-- Coding policy/verifier
```

New infrastructure:

- isolated workspace provisioner;
- worker identity;
- scoped credential delivery;
- resource quota;
- network policy;
- cleanup;
- cloud store adapter;
- artifact/workspace object storage.

### Acceptance

```text
Mobile/Web creates cloud Coding task
local laptop is OFF
worker executes and verifies
clients receive completion + diff + evidence
```

---

# 18. Phase 10 — Local <-> Cloud ownership transfer

Priority: **P4**

Do this only after cloud execution is reliable independently.

Flow:

```text
Local owner
   |
quiesce
   |
reconcile in-flight tool boundary
   |
create transfer package
   |
release ownership
   |
advance epoch
   |
Cloud acquires
   |
resume
```

Transfer includes runtime context and workspace state, not raw database merging.

Fault injection is mandatory around every ownership boundary.

---

# 19. Phase 11 — Durable Scheduler

Priority: **P5**

Create Runtime-level durable scheduling only after durable Task/Owner infrastructure exists.

Triggers may include:

```text
Manual
At time
Recurring
Inbox/event
Task completion
Dependency
Retry deadline
```

Scheduler only creates/wakes/resumes work. It does not contain a second Agent Loop.

Do not reuse `BackgroundTaskRegistry` as Scheduler.

---

# 20. Phase 12 — NPC Runtime

Priority: **P5**

NPC builds on the completed runtime stack:

```text
NPC Domain
   |
Scheduler
   |
Task Runtime
   |
Cloud/Local Owner
   |
Shared Agent + ToolHost
```

NPC adds:

- identity;
- long-term memory;
- inbox;
- world state;
- relationship state;
- character policy;
- goal selection;
- schedule definition.

NPC does not duplicate:

- permissions;
- sandbox;
- Agent Loop;
- provider execution;
- canonical events;
- crash recovery;
- task ownership.

Only after Coding and NPC both exist should shared domain abstractions be extracted based on evidence.

---

# 21. Recommended implementation sequence

The practical order is:

```text
CURRENT
Reliable local Coding Runtime
        |
        v
P0 Stability baseline
        |
        v
P1 Task + generic lifecycle
        |
        v
P2/P3 Storage ports + Ownership/Fencing
        |
        v
P5 Durable Local Runtime Supervisor
        |
        v
================ CORE STABILITY GATE ================
        |
        +--> optional container validation if a real product needs it
        |
        v
Control Plane
        |
        v
Remote client parity
        |
        v
Cloud Worker
        |
        v
Ownership Transfer
        |
        v
Scheduler
        |
        v
NPC
```

The key change from a feature-first roadmap is that **Cloud and NPC do not drive the core architecture while the core is still moving**.

---

# 22. First concrete engineering backlog

When implementation starts, the first architecture backlog should be small and reviewable.

## Batch A — Architecture inventory / tests

- document current lifecycle writers in code comments/tests;
- add dependency/architecture tripwires where practical;
- add explicit tests that TUI/client disconnect does not imply cancellation in daemon mode;
- preserve ToolHost unique-execution tripwire;
- preserve crash replay-safety tests.

## Batch B — Task identity

- introduce `TaskId` type;
- introduce task/session mapping migration;
- add Task repository/API without removing Session APIs;
- expose TaskId in internal canonical events where appropriate;
- compatibility tests for existing resume.

## Batch C — Runtime identity

- introduce `RuntimeId`;
- assign stable local runtime identity semantics;
- include runtime identity in diagnostic/snapshot state without changing ownership yet.

## Batch D — Lifecycle semantic split

- create generic runtime status module inside `leveler-lifecycle`;
- classify existing `AgentState` consumers into runtime vs Coding workflow;
- migrate only the generic ownership/lifecycle sites first;
- keep UI compatibility projection.

## Batch E — Storage seams

- inventory direct `Database` dependencies in Engine;
- extract one narrow store port at a time;
- keep SQLite as only production implementation until contracts stabilize;
- add store contract tests.

Only after these batches are stable should owner epoch / distributed lease semantics begin.

---

# 23. Change discipline

Every architecture phase follows these rules:

1. **No big-bang crate rewrite.**
2. **Structure changes and product behavior changes are separate commits.**
3. **Existing public/session compatibility is preserved through explicit migrations.**
4. **New path first, compare/shadow where useful, then switch default, then remove old path.**
5. **Every distributed-state feature gets deterministic fault tests.**
6. **All model-proposed side effects still pass ToolHost.**
7. **No new benchmark system; use `leveler-eval` and existing test infrastructure.**
8. **A new crate must solve a real dependency/ownership problem, not make an architecture diagram prettier.**
9. **A new service must own a distinct process-level responsibility.**
10. **Documentation is updated when responsibilities change.**

---

# 24. Final priority table

| Priority | Work | Reason |
| --- | --- | --- |
| P0 | Finish local/runtime stability | Trustworthy baseline |
| P1 | Explicit Task + generic lifecycle | Domain-neutral runtime foundation |
| P1/P2 | Runtime storage ports | Deployment independence |
| P1/P2 | Runtime identity + ownership/fencing | Distributed correctness prerequisite |
| P2 | Durable Local Runtime Supervisor | Task survives UI/process handoff |
| Gate | Core Stability Gate | Prevent feature-driven core churn |
| P2/P3 | Optional container validation | Only if real product requires it |
| P3 | Control Plane | Multi-runtime coordination |
| P3 | Remote Web/Mobile parity | Remote management experience |
| P4 | Cloud Worker | Laptop-off execution |
| P4 | Ownership transfer | Seamless Local/Cloud continuation |
| P5 | Durable Scheduler | Long-horizon work infrastructure |
| P5 | NPC Runtime | Second domain on proven core |

---

# 25. Success definition

The target architecture is not complete because all proposed crates exist. It is complete when the runtime semantics hold across scenarios.

## Scenario A — Local Coding

```text
TUI starts task
TUI closes
Local Runtime continues
Web/Mobile/TUI reconnects
same task resumes/continues
verification proves completion
```

## Scenario B — Container-hosted Coding (if needed)

```text
Product starts isolated container
CodeLeveler runs with same core
Web/TUI controls it
no alternate Agent implementation exists
```

## Scenario C — Cloud Coding

```text
Mobile/Web creates cloud task
laptop is offline
one worker owns task
worker completes with evidence
all clients see same terminal facts
```

## Scenario D — Ownership fault

```text
old owner partitions
new owner acquires newer epoch
old owner reconnects
stale writes are fenced
no duplicate side effect is silently accepted
```

## Scenario E — NPC

```text
scheduler wakes NPC
NPC selects/creates Task
shared Runtime executes
approval reaches user remotely if needed
Task finishes or stops explicitly
```

At every failure point the system must do one of three things:

```text
recover safely
stop explicitly
request human reconciliation
```

It must never guess that a side effect did not happen, duplicate an unsafe action, or report success without required evidence.

---

# 26. Final decision

CodeLeveler's next phase is **not** “build all future features”.

It is:

> **Stabilize the existing Coding Runtime, generalize only the core boundaries proven necessary for durable Task ownership, and make that core dependable enough that Cloud and NPC can later reuse it without forks.**

This keeps the product useful during the migration and avoids sacrificing a working Coding Agent in pursuit of premature platform abstraction.
