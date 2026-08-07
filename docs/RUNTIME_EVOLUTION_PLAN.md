# CodeLeveler Runtime Evolution Plan

> Status: Target roadmap
> Baseline: current main after runtime convergence work
>
> Core principle:
>
> **Stabilize and generalize the Runtime Core before expanding product surfaces.**

This roadmap intentionally delays Cloud, Scheduler, and NPC until the execution foundation is stable, because all future capabilities depend on the same durable runtime guarantees.

---

# 1. Current direction

CodeLeveler should not evolve by adding independent products that duplicate execution logic.

The desired evolution is:

```text
Reliable Coding Agent
        |
        v
Durable Runtime Core
        |
        +----------------+
        |                |
        v                v
Embedded Runtime     Cloud Runtime
        |                |
        +----------------+
                 |
                 v
            Scheduler
                 |
                 v
                NPC
```

The order matters.

The core runtime must become stable before adding more deployment modes.

---

# 2. Phase 0 — Finish current runtime stability

Priority: P0

Before architectural expansion, finish current runtime confidence.

Goals:

- long-running execution stability;
- crash recovery confidence;
- TUI/Web/Mobile consistency;
- cross-platform validation;
- real project validation;
- resource growth measurement.

Acceptance examples:

```text
Close TUI
   ↓
Task continues

Restart runtime
   ↓
Task resumes

Network interruption
   ↓
Reconnect without duplicate side effects
```

No Cloud or NPC work should become the priority before this phase is complete.

---

# 3. Phase 1 — Generic Runtime Core model

Priority: P1

Goal:

Move from a Coding-shaped runtime model to a domain-neutral execution model without breaking existing Coding behavior.

Main changes:

## 3.1 Separate Task from Session

Current products naturally use Session as the unit of work.

Target:

```text
Task
 |
 +-- Session
 +-- Turns
 +-- Events
 +-- Artifacts
 +-- Evidence
```

Migration must be incremental.

Do not invalidate existing sessions.

---

## 3.2 Introduce Runtime Identity

Add the concept of:

```text
RuntimeId
```

Examples:

- local Mac runtime;
- container runtime;
- cloud worker runtime.

---

## 3.3 Introduce Task Ownership

Every Task must have:

```text
TaskId
RuntimeOwner
OwnerEpoch
ExecutionLocation
```

Invariant:

> One task has one authoritative writer at any moment.

---

## 3.4 Separate runtime lifecycle from domain workflow

Generic lifecycle:

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

Coding workflow remains Coding-specific:

```text
Understand
Localize
Plan
Implement
Verify
Repair
Review
```

NPC will have different domain states.

---

# 4. Phase 2 — Storage boundary

Priority: P1/P2

Goal:

Make Runtime independent from one database implementation.

Current:

```text
Engine
 |
SQLite
```

Target:

```text
Engine
 |
Runtime Store Ports
 |
 +-- SQLite
 +-- PostgreSQL
 +-- Embedded host adapter
```

Recommended ports:

- EventStore;
- TaskStore;
- SessionStore;
- TurnStore;
- CommandReceiptStore;
- OwnershipStore;
- ArtifactMetadataStore.

Do not create one giant Database interface.

---

# 5. Phase 3 — Ownership and fencing

Priority: P1/P2

Before distributed execution, solve ownership correctness.

Introduce:

```text
TaskOwner
 RuntimeId
 OwnerEpoch
 Lease metadata
```

Every authoritative write must validate ownership.

Test cases:

```text
Runtime A owns task
Runtime A disconnects
Runtime B acquires new epoch
Runtime A reconnects
Old write rejected
```

This prevents:

- split brain;
- duplicate execution;
- conflicting approvals;
- stale completion.

---

# 6. Phase 4 — Local Runtime Supervisor

Priority: P2

Goal:

Remove TUI lifetime from runtime lifetime.

Target:

```text
TUI
 |
 connect
 |
Runtime Service
```

Responsibilities:

- runtime identity;
- startup;
- restart;
- health;
- heartbeat;
- local host registration;
- capacity.

Keep in-process mode for development and testing.

Do not let it define production semantics.

---

# 7. Phase 5 — Embedded / Container Runtime

Priority: P2/P3

Goal:

Make CodeLeveler usable by other products.

Example:

```text
Online IDE
    |
    v
Container
    |
CodeLeveler Runtime
```

Required capabilities:

- headless runtime mode;
- Runtime SDK/API;
- task creation;
- event subscription;
- approvals;
- diff retrieval;
- verification retrieval;
- cancellation;
- lifecycle management.

The container product should reuse:

- leveler-engine;
- leveler-agent;
- ToolHost;
- execution layer;
- verification model.

It should not fork a special online-coding agent.

---

# 8. Phase 6 — Control Plane

Priority: P3

Only after the runtime core is stable.

Purpose:

Coordinate multiple runtime owners and clients.

Responsibilities:

- identity;
- devices;
- runtime directory;
- task routing;
- event projection;
- notifications.

Not responsible for:

- model execution;
- tool execution;
- workspace mutation.

---

# 9. Phase 7 — Remote Web and Mobile completion

Priority: P3

Build on existing protocol instead of creating new systems.

Complete views:

- task list;
- conversation;
- plan;
- tool activity;
- diff;
- verification;
- evidence;
- approvals;
- clarification;
- steering.

Mobile focus:

remote control and decision making.

TUI focus:

maximum coding productivity.

---

# 10. Phase 8 — Cloud Worker

Priority: P4

After Embedded Runtime is stable.

Cloud Worker is another Runtime deployment.

It reuses:

```text
leveler-engine
leveler-agent
ToolHost
leveler-tools
leveler-execution
```

Infrastructure additions:

- isolated workspace;
- worker identity;
- scoped credentials;
- resource limits;
- cleanup;
- cloud storage.

---

# 11. Phase 9 — Local to Cloud transfer

Priority: P4

Implement only after Cloud Worker is reliable.

Transfer is ownership migration, not database synchronization.

Flow:

```text
Local
 |
Quiesce
 |
Checkpoint
 |
Release ownership
 |
Cloud acquire
 |
Resume
```

Failure injection is required:

- before checkpoint;
- during transfer;
- after release;
- before acquire;
- after acquire.

Exactly one owner must remain true.

---

# 12. Phase 10 — Durable Scheduler

Priority: P5

Scheduler enables:

- delayed execution;
- recurring tasks;
- dependency triggers;
- retries;
- wake events.

Scheduler creates or wakes tasks.

It does not execute tools or replace the Agent Loop.

---

# 13. Phase 11 — NPC Runtime

Priority: P5

NPC comes after durable runtime primitives exist.

NPC adds domain state:

```text
Identity
Memory
Inbox
WorldState
Relationship
CharacterPolicy
ScheduleDefinition
```

NPC does not own:

- execution;
- permissions;
- sandbox;
- recovery;
- canonical events;
- providers.

NPC uses:

```text
Scheduler
   |
Task Runtime
   |
Agent Runtime
   |
Worker
```

---

# 14. Explicitly delayed work

Do not prioritize:

- generic workflow DSL;
- fixed multi-agent role graphs;
- database synchronization;
- multiple independent agent loops;
- cloud tool execution bypassing ToolHost;
- UI-specific state machines;
- premature crate fragmentation.

---

# 15. Final priority table

| Priority | Work | Reason |
|---|---|---|
| P0 | Runtime stability | Establish trustworthy baseline |
| P1 | Generic Task/Runtime model | Foundation for all domains |
| P1/P2 | Storage ports | Enable deployment flexibility |
| P1/P2 | Ownership/fencing | Required before distributed execution |
| P2 | Runtime Supervisor | Remove TUI lifetime dependency |
| P2/P3 | Embedded Container Runtime | Product integration capability |
| P3 | Control Plane | Multi-runtime coordination |
| P3 | Remote Web/Mobile parity | Multi-client experience |
| P4 | Cloud Worker | Laptop-off execution |
| P4 | Ownership transfer | Seamless local/cloud migration |
| P5 | Scheduler | Long-running autonomous work |
| P5 | NPC Runtime | Autonomous domain expansion |

---

# 16. Success definition

The architecture is successful when these scenarios work:

## Local Coding

```text
TUI starts task
TUI closes
Runtime continues
Mobile reconnects
Task completes with evidence
```

## Embedded Coding

```text
Host product creates container
Container runs CodeLeveler
Host UI controls task through API
```

## Cloud Coding

```text
Mobile creates cloud task
Laptop is offline
Worker executes
Result appears everywhere
```

## NPC

```text
User schedules goal
NPC wakes
Creates task
Runtime executes
User approves when needed
Result arrives later
```

The order is intentional:

> Build the Runtime first. Build products on top of the Runtime second.
