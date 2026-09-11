# CodeLeveler Architecture

Chinese version: [`ARCHITECTURE.zh-CN.md`](ARCHITECTURE.zh-CN.md)

CodeLeveler is not intended to become a Coding Agent that simply accumulates more and more features.

Its long-term role is:

> **A reusable foundation for running agents. The Coding Agent is the first product built on top of that foundation, not the highest abstraction in the system.**

The architecture separates concerns deliberately: the model thinks, the domain harness defines what kind of agent this is, the runtime keeps it running reliably, capabilities do real work, host authority controls real side effects, persistence provides continuity, and products turn those pieces into user experience.

---

## 1. The Whole System at a Glance

You can understand the architecture before knowing any implementation detail.

```text
                            Model
                    (reasoning and decisions)
                              ↕
User → Product → Domain Harness → Agent Runtime
                              │
                   ┌──────────┴──────────┐
                   ▼                     ▼
                Capabilities         Persistence
                   │                     │
                   └──────────┬──────────┘
                              ▼
                         Host Authority
                              │
                              ▼
                       Operating System
```

If you remember only one sentence:

> **The model thinks. The Harness defines how this kind of agent should work. The Runtime keeps it alive and correct. Capabilities define what it can do. Host Authority decides which real-world side effects may actually happen.**

Different products can be built on the same foundation:

```text
                          Agent Foundation
                                │
              ┌─────────────────┼─────────────────┐
              ▼                 ▼                 ▼
        Coding Harness     Review Harness     Other Harness
              │                 │                 │
              ▼                 ▼                 ▼
        Coding Product     Review Product     Future Product
```

Products share the runtime foundation without inheriting each other's domain semantics.

---

## 2. Terms and Names

The names in this architecture describe ownership. Each term exists to answer “who is responsible for this?”

| Term | Meaning | Common confusion |
| --- | --- | --- |
| **Agent** | A goal-directed running entity that uses a model and capabilities to do work | An agent is not a model; the model is its source of intelligence |
| **Model** | The reasoning engine: understanding, planning, judgment, generation | The model does not directly own filesystem, process, or task lifecycle |
| **Harness** | The domain layer that adapts a general model and runtime to a concrete domain such as Coding or Review | It is not the runtime and not a replacement “brain” |
| **Agent Runtime** | The reusable machinery that lets an agent loop, pause, resume, cancel, persist, and recover | It runs the agent; it does not define domain reasoning |
| **Agent Kernel** | The smallest generic model↔tool execution loop | It knows no Coding or Review concepts |
| **Engine** | The long-lived runtime component that owns sessions, tasks, turns, events, recovery, and continuation | **The Engine owns lifecycle, not intelligence** |
| **Capability** | A reusable system ability such as workspace access, command execution, browser control, or VCS | A capability is not the same thing as a model-visible tool |
| **Tool** | A model-facing adapter for invoking a capability | A tool should not become the capability runtime itself |
| **Tool Surface** | The set of tools visible to a model for a particular Harness and product | Owning many capabilities does not mean exposing all of them every round |
| **Host Authority** | The single boundary with final authority to perform controlled side effects | Authority is stronger than “permission config”; it is who can make the effect real |
| **Persistence** | Durable state that lets tasks, turns, ownership, events, and work continue across time | It is much more than chat history |
| **Authority** | The canonical owner of a class of facts or actions | One fact must not have competing authorities |
| **Product** | The user-facing composition: TUI, Web, Desktop, Mobile, and concrete agent products | A product presents runtime facts; it does not create runtime facts |
| **Provider / Protocol Layer** | Adapters that normalize model vendors and wire protocols | Protocol adaptation must not silently change domain semantics |

### 2.1 Why “Harness”

A Harness turns a general model and a general runtime into a specific kind of agent:

```text
General Model + General Runtime
              ↓
        Domain Semantics
              ↓
 Coding Agent / Review Agent / Other Agent
```

“What is a coding task?”, “which coding tools should the model see?”, and “what does domain completion mean?” are Coding questions, not Runtime questions.

So:

> **The Harness defines what kind of agent this is. The Runtime defines how that agent runs reliably.**

### 2.2 Why “Authority”

Authority means “who has final say.”

The model may request “modify this file,” but only Host Authority can make the filesystem mutation real.

Likewise, a UI may display “completed,” but the task state must come from the authoritative runtime fact rather than being inferred independently by the UI.

---

## 3. Seven Core Principles

The architecture can be reduced to seven statements:

```text
The Model owns intelligence.
The Harness owns domain semantics.
The Runtime owns lifecycle and mechanical correctness.
Capabilities own reusable abilities.
Host Authority owns controlled real side effects.
Every persistent fact has one authoritative owner.
Products project Runtime truth; they do not create it.
```

The central lifecycle boundary is:

> **Engine owns lifecycle, not agent intelligence.**

That prevents the Engine from slowly becoming a giant state machine that contains planning, judgment, tool choice, and product-specific semantics.

---

## 4. Model: Where Intelligence Comes From

The model owns work that requires reasoning:

```text
understanding the goal
planning
investigation
choosing tools
generating code or content
debugging
trade-offs
semantic judgment
adapting after failure
```

The system should provide the model with:

```text
clear capabilities
stable semantics
precise errors
real environment state
deterministic mechanical constraints
reliable execution results
```

It should not build a hidden second reasoning system to compensate for model weakness.

Therefore:

```text
MODEL CAPABILITY LIMIT
        ≠
RUNTIME DEFECT
```

The Runtime fixes engineering problems—failure, concurrency, cancellation, recovery, authority, consistency—not intelligence limits.

---

## 5. Harness: Defining What Kind of Agent This Is

The Harness sits between model, runtime, and product.

It answers:

> **In this domain, what can the agent see, what can it do, what rules apply, and what does domain completion mean?**

### 5.1 Coding Harness

A Coding Harness may own:

```text
coding-domain contract
repository semantics
coding tool surface
verification semantics
delegation semantics
multi-agent write collaboration semantics
coding completion semantics
```

### 5.2 Review Harness

A Review Harness may own:

```text
review target
review scope
findings
severity
evidence
review verdict
review completion semantics
```

### 5.3 Harnesses Are Siblings

```text
                     Agent Runtime
                  /       |        \
                 ▼        ▼         ▼
              Coding    Review    Research
              Harness   Harness   Harness
```

Review is not a “mode” inside Coding, and Research is not an extension pack under Coding.

A new domain reuses the common Runtime and common Capabilities without inheriting another domain's semantic baggage.

### 5.4 What a Harness Owns—and Does Not Own

**Owns:**

```text
domain vocabulary
domain constraints
domain tool selection
domain completion semantics
domain collaboration rules
domain context organization
```

**Does not own:**

```text
a second task lifecycle
a second persistence system
a path around Host Authority
hidden reasoning on behalf of the model
a second permission or ownership system
```

---

## 6. Agent Runtime: Keeping Agents Alive Reliably

The Agent Runtime is the central reusable foundation.

Conceptually:

```text
Agent Kernel
    +
Persistent Engine
    =
Agent Runtime
```

### 6.1 Agent Kernel

The Kernel owns the generic model execution loop:

```text
model interaction
tool loop
streaming
round progression
budgets
usage accounting
retry and backoff
timeouts
cancellation
stopping
```

It does not know:

```text
Coding
Review
repository workflow
product-specific prompts
domain completion definitions
UI behavior
user acceptance
```

Its value is simple: Coding, Review, and future Harnesses can all run on the same agent loop.

### 6.2 Engine

The Engine owns long-lived lifecycle mechanics:

```text
sessions
tasks
turns
events
persistence
pause and resume
recovery
cancellation
background work
parent-child relationships
ownership
runtime outcomes
```

The Engine may know:

```text
a task is running
a task was cancelled
a turn ended
a child task exited
an event was persisted
```

But it must not infer from “tests passed” or “the tree changed” that:

```text
the user's coding request is semantically complete
```

The boundary is:

```text
Engine owns lifecycle.
Harness owns domain semantics.
Model owns reasoning and judgment.
```

---

## 7. Capabilities and Tools: What the System Can Do vs. How the Model Invokes It

This distinction is fundamental.

### 7.1 Capability

A Capability is a reusable ability the system actually owns, for example:

```text
workspace access
command execution
browser control
search
code intelligence
version control
memory
media processing
skills
remote execution
external services
```

A Capability answers:

> **What can the system do?**

### 7.2 Tool

A Tool is the model-facing interface for requesting a Capability.

```text
Model
  ↓
read-file tool
  ↓
Workspace Capability
  ↓
Host filesystem
```

A Tool answers:

> **How does the model request that ability?**

Therefore:

```text
Tool ≠ Capability
```

One Capability may have multiple Tool adapters. A Capability may also be used directly by a Harness or Runtime when that use belongs to its responsibility.

### 7.3 Tools Stay Thin; Capabilities Have Clear Ownership

A Tool primarily owns:

```text
model-facing name and description
parameter schema
input decoding
capability invocation
result rendering
precise errors
```

A Capability owns:

```text
real domain behavior
reusable logic
its own necessary state
its own necessary lifecycle
consistency
```

Long-lived state, global policy, service discovery, permission decisions, and process lifecycle should not be pushed into Tools merely because a Tool needs to access them.

### 7.4 Capability Is a Responsibility, Not a Mandatory Package Boundary

“Capability” first describes ownership, not a required crate, service, process, or trait.

Physical separation should be driven by real boundaries such as:

```text
multiple real consumers
independent lifecycle
independent security boundary
independent protocol boundary
independent persistence boundary
remote deployment need
```

Do not manufacture abstractions merely to make the architecture diagram symmetric.

---

## 8. Tool Surface: How Much Should the Model See?

Owning many capabilities does not mean the model should see every tool in every round.

The Harness selects the tool surface for the current product:

```text
System Capabilities
       │
       ▼
Harness Selection
       │
       ▼
Tool Surface
       │
       ▼
Model
```

The goal is:

> **Expose the clearest, most stable, least ambiguous interface that still preserves the necessary capability.**

A good tool should:

```text
express one clear intent
have stable semantics
have predictable input and output
report failure precisely
have a clear boundary from neighboring tools
```

The system may enable or disable a Tool based on real mechanical capability—for example, whether the host has a browser or the model supports vision.

It must not silently change Tool semantics because:

```text
“this model is weak”
“this task looks hard”
```

---

## 9. Host Authority: Who Is Allowed to Change the Real World?

The model may request a side effect, but it does not own host power directly.

```text
Agent requests
      ↓
Host Authority decides and performs
      ↓
Operating System changes
```

Host Authority controls:

```text
filesystem mutation
process execution
network access
sandboxing
permissions
approval
workspace boundaries
process lifecycle
```

This separates:

```text
what the agent wants to do
          ≠
what the host allows to happen
```

No Tool, plugin, child agent, or Product may create a second path around Host Authority.

### 9.1 Why There Must Be One Execution Authority

If file mutation follows one authority, commands another, and child agents a third, there is no unified safety boundary.

For controlled real side effects:

> **Requests may come from many places. Final execution authority must remain singular.**

---

## 10. Mechanical Facts, Semantic Completion, and User Acceptance

CodeLeveler explicitly separates “what happened” from “whether the work is truly done.”

```text
Mechanical Truth
      ≠
Semantic Satisfaction
      ≠
User Acceptance
```

### 10.1 Mechanical Facts Belong to the Runtime

The Runtime can authoritatively record facts such as:

```text
whether a command ran
its exit status
whether a file changed
whether an event occurred
whether verification ran
whether an artifact exists
whether a process exited
```

These are mechanically knowable facts.

### 10.2 Semantic Completion Belongs to Model + Domain

For example:

```text
tests passed
```

does not automatically imply:

```text
the requested feature is semantically complete
```

Whether the available evidence is sufficient to satisfy the domain goal requires semantic interpretation in the context of the Harness.

### 10.3 The User Owns Final Acceptance

The final relation is:

```text
Runtime owns facts.
Model interprets those facts semantically.
User owns final acceptance.
```

This prevents a green check, successful Tool call, or changed file from becoming an automatic “done” signal.

---

## 11. Persistence: Letting Work Continue Beyond One Conversation

Long-lived agents require durable state.

The core rule is:

```text
One persistent fact
        ↓
One authoritative owner
        ↓
One canonical representation
```

Task state, turn state, ownership, evidence, usage, artifacts, and background work all need a clear single source of truth.

### 11.1 Persist Before Forward

Authoritative runtime events should flow as:

```text
Runtime Fact
    ↓
Persist
    ↓
Forward to Clients
```

A client should never observe an authoritative fact before the Runtime has durably recorded it.

### 11.2 Persistence Provides Continuity

Its purpose is much broader than saving chat history:

```text
pause and resume
crash recovery
cross-device continuation
background work
long-running tasks
durable child sessions
auditability
```

An agent may eventually work for hours or days and continue across devices. Persistence is the foundation of that shape.

---

## 12. Model Providers and Capability Negotiation

CodeLeveler can work with different models and model services without allowing protocol differences to leak into domain architecture.

The Provider / Protocol layer normalizes:

```text
request and response protocols
streaming
tool calling
reasoning transport
structured output
vision
context and output limits
error mapping
```

### 12.1 Protocol Differences Can Be Adapted; Semantics Cannot Be Faked

If two providers differ only in wire format, the adapter can normalize them.

If a model truly lacks a required mechanical capability, the system should report that capability as absent rather than invent a hidden behavioral path that pretends it exists.

### 12.2 Available Capability Is an Intersection

A capability is actually usable only when multiple conditions agree:

```text
Model Capability
      ∩
Host Capability
      ∩
Runtime Capability
      ∩
Harness Requirement
      =
Available Capability
```

This is capability negotiation.

It lets the same architecture work across different models, machines, and execution environments.

---

## 13. Product and Clients: Experience Belongs to Product, Truth Belongs to Runtime

The Product layer turns the foundation into something people use:

```text
CLI
TUI
Web
Desktop
Mobile
Remote Client
```

Products may decide:

```text
how information is presented
how interaction is organized
navigation
default UX
which capabilities compose a product
```

But a Product does not redefine Runtime truth.

The core rule is:

> **The UI is a projection of Runtime truth, not a new source of Runtime truth.**

Clients should connect to the Runtime through a stable command/event boundary:

```text
Client Command
      ↓
Runtime
      ↓
Runtime Event
      ↓
Client
```

One Runtime can therefore serve multiple clients:

```text
                     Runtime
                 /     |      \
                ▼      ▼       ▼
              TUI     Web    Mobile
```

The client and Runtime do not need to live on the same machine.

---

## 14. How One Task Actually Flows

Consider a coding task: “fix this bug and verify the result.”

### Step 1: Product Receives the Goal

The Product owns interaction and passes the goal into the Coding Harness.

```text
User
 ↓
Product
```

### Step 2: The Harness Establishes Coding Context

The Coding Harness decides:

```text
this is a coding task
which coding capabilities the model should see
what repository and domain rules apply
how domain completion is expressed
```

### Step 3: Runtime Starts the Task

The Runtime:

```text
creates task and turn lifecycle
maintains state
records events
manages budget, cancellation, and recovery
```

### Step 4: The Model Reasons and Chooses an Action

The model investigates, plans, and requests a Capability through a Tool.

### Step 5: Capabilities Perform the Work

Read, search, code intelligence, and other Capabilities perform their responsibilities.

For real side effects:

```text
Model Request
    ↓
Domain Tool
    ↓
Capability
    ↓
Host Authority
    ↓
File / Process / Network
```

### Step 6: Runtime Records Mechanical Facts

Examples include process exit status, file changes, verification results, artifacts, and lifecycle events.

### Step 7: The Model Judges Domain Completion

The model combines the goal, domain semantics, and mechanical facts to decide whether more work is needed.

### Step 8: Product Presents the Result; User Accepts or Rejects

Runtime provides authoritative facts, Product presents them, and the user owns acceptance.

The complete flow is:

```text
User
 ↓
Product
 ↓
Harness
 ↓
Agent Runtime ↔ Model
 ↓
Tool Surface
 ↓
Capabilities
 ↓
Host Authority
 ↓
Operating System
 ↓
Mechanical Facts
 ↓
Persistence
 ↓
Product Projection
 ↓
User Acceptance
```

---

## 15. Multi-Agent: More Than “Calling More Models”

Multi-agent architecture is not primarily about model count.

It is:

> **Multiple agent lifecycles collaborating under the same Runtime, ownership model, persistence model, and Host Authority.**

```text
                    Parent Agent
                /       |        \
               ▼        ▼         ▼
           Explorer    Worker    Reviewer
               │        │         │
               └────────┼─────────┘
                        ▼
                    Shared Runtime
              ┌─────────┼─────────┐
              ▼         ▼         ▼
          Persistence Ownership Capabilities
```

Multi-agent collaboration needs one shared mechanical model for:

```text
lifecycle
parent-child relationships
ownership
write scope
capability access
background execution
cancellation
settlement
persistence
result handoff
```

Roles may differ. Domain semantics may differ. But child agents must not invent a second lifecycle, permission system, or persistence system beside the Runtime.

---

## 16. Dependency Direction: Lower Means More General

Dependencies should always point toward more general layers:

```text
Product
  ↓
Harness
  ↓
Runtime / Capabilities
  ↓
Foundation
```

Another way to read it:

```text
Higher layers: more product- and domain-specific
Lower layers: more general, stable, and ignorant of upper-layer semantics
```

Therefore:

```text
Runtime must not depend on a concrete Harness.
Harness must not depend on a concrete Product UI.
Foundation must not know Coding or Review concepts.
A Capability must not depend backward on the specific Tool or Product consuming it.
```

Lower layers provide mechanisms. Upper layers provide composition and semantics.

---

## 17. Architecture Guardrails: Allowed and Forbidden

This section is the direct decision framework for future design.

### 17.1 Product Layer

**Allowed:**

```text
new UIs
new clients
interaction and navigation changes
new product compositions
local view state
```

**Forbidden:**

```text
independently deriving authoritative task state
treating Tool success as task completion
creating a second Runtime truth
bypassing Runtime to own durable task lifecycle
```

### 17.2 Harness Layer

**Allowed:**

```text
new domain rules
new Coding / Review / Research Harnesses
tool-surface changes
domain completion semantics
domain collaboration semantics
```

**Forbidden:**

```text
pushing Coding or Review semantics into generic Runtime
hidden planning or reasoning on behalf of the model
a second task lifecycle
a second permission, ownership, or persistence system
bypassing Host Authority
```

### 17.3 Agent Runtime

**Allowed:**

```text
generic lifecycle mechanisms
stronger pause / resume / cancellation
background lifecycle
parent-child session relationships
resource budgets
events and persistence
crash recovery
```

Only when the mechanism remains valid across multiple domains.

**Forbidden:**

```text
understanding product-specific completion semantics
deciding how the model should investigate or plan
changing runtime rules by model “strength”
embedding a product workflow
becoming a giant Agent Brain
```

### 17.4 Capability Layer

**Allowed:**

```text
new reusable capabilities
capability-owned lifecycle where necessary
consolidating shared behavior under a clear capability owner
local and remote implementations
```

**Forbidden:**

```text
product UI logic inside a Capability
a Capability depending backward on a specific Tool
multiple competing owners for the same capability
splitting components only for architectural symmetry
```

### 17.5 Tool Layer

**Allowed:**

```text
new model intent entry points
clear parameter schemas
invoking existing capabilities
rendering capability results to the model
precise errors
```

**Forbidden:**

```text
becoming a global service locator
owning long-lived global state
owning permission policy
owning process lifecycle
reimplementing the capability runtime
silently changing operation semantics
```

### 17.6 Host Authority

**Allowed:**

```text
controlling real side effects
permissions and approvals
workspace enforcement
sandbox and process constraints
local, remote, or cloud execution backends
```

**Forbidden:**

```text
judging whether a domain goal is semantically complete
changing facts based on Product UI state
allowing a bypass execution authority beside it
```

### 17.7 Provider / Protocol Layer

**Allowed:**

```text
protocol adaptation
request / response normalization
model capability normalization
wire compatibility
mechanical capability negotiation
```

**Forbidden:**

```text
changing domain semantics to pretend a capability exists
injecting hidden behavior because a model is weak
allowing different providers to imply different task meanings
```

### 17.8 Multi-Agent

**Allowed:**

```text
role-specialized child agents
durable child sessions
background work
capability negotiation
result handoff
remote workers
```

**Forbidden:**

```text
child agents bypassing Runtime
child agents bypassing shared ownership
child agents creating a second filesystem authority
child agents creating a second persistence truth
```

### 17.9 New Abstractions

A new crate, trait, manager, registry, adapter layer, or generic framework is justified when at least one real driver exists:

```text
two real implementations or consumers
a real dependency-inversion need
an independent security boundary
an independent protocol boundary
an independent persistence boundary
an independent lifecycle boundary
a remote deployment boundary
an observed ownership or coupling problem
```

It is not justified merely because:

```text
“we may need it later”
“it is more generic”
“it looks cleaner”
“a second implementation may exist someday”
“the diagram becomes more symmetric”
```

---

## 18. Architecture Invariants

These rules should remain true over the long term.

### 18.1 Intelligence Boundary

```text
The Model owns intelligence.
The Runtime does not emulate agent intelligence.
```

### 18.2 Domain Boundary

```text
The Harness owns domain semantics.
Generic Runtime does not define Coding, Review, or other product semantics.
```

### 18.3 Lifecycle Boundary

```text
Engine owns lifecycle, not agent intelligence.
```

### 18.4 Capability Boundary

```text
A Tool is an adapter.
A Capability is the reusable ability.
```

### 18.5 Execution Authority Boundary

```text
The Agent requests.
Host Authority decides and performs real side effects.
```

### 18.6 Persistence Boundary

```text
One persistent fact, one authoritative owner.
```

### 18.7 Product Boundary

```text
Products project truth; they do not create Runtime truth.
```

### 18.8 Dependency Boundary

```text
Dependencies point toward more general layers.
General layers do not depend backward on product-specific layers.
```

### 18.9 Multi-Harness Boundary

```text
Adding a semantically different Harness must not require redesigning the Agent Kernel.
```

### 18.10 Reliability Boundary

```text
Moving responsibility between layers must not weaken
mechanical correctness, authority, persistence, cancellation, recovery, or safety.
```

---

## 19. Evolution Direction

CodeLeveler should evolve by broadening the foundation, not by piling every feature into one Coding Agent.

### 19.1 Single Domain → Multiple Domains

```text
                     Agent Runtime
                  /       |        \
                 ▼        ▼         ▼
              Coding    Review    Research
```

Each Harness owns its own semantics and Tool Surface while sharing Runtime, Capabilities, Persistence, and Host Authority.

### 19.2 Single Agent → Multi-Agent Runtime

```text
Single Agent
    ↓
Parent / Child Agents
    ↓
Role-specialized Agents
    ↓
Multi-Agent Runtime
```

Explorer, Worker, Reviewer, Specialist, and other roles can emerge without creating parallel lifecycle systems.

### 19.3 Tool Collection → Capability Platform

Capabilities become a composable platform:

```text
                      Capability Platform
                    /        |        \
                   ▼         ▼         ▼
                Coding     Review     Other
```

Workspace, Execution, Browser, Search, Memory, VCS, Code Intelligence, Media, and external services become reusable across domains.

### 19.4 Local → Remote → Cloud

Runtime semantics should be independent of execution location:

```text
Harness
   ↓
Agent Runtime
   ↓
Host Authority
   ├── Local Host
   ├── Remote Host
   ├── Container
   ├── Isolated VM
   └── Cloud Worker
```

What changes is *where* the work executes, not what the Agent means.

### 19.5 Request-Scoped Agents → Durable Agents

Agents increasingly become durable work entities:

```text
long-running goals
pause / resume
background execution
cross-device continuation
durable child sessions
recovery
long-lived context
```

A single chat request is only the shortest lifecycle, not the architectural ceiling.

### 19.6 Capability Negotiation as a First-Class Mechanism

Different models, hosts, and remote workers expose different capabilities.

The system should negotiate those capabilities explicitly instead of relying on hidden semantic fallback.

### 19.7 Final Shape: Agent Platform

The long-term structure is:

```text
                              Products
                  ┌────────────┼────────────┐
                  ▼            ▼            ▼
               Coding        Review       Others
                  │            │            │
                  └────────────┼────────────┘
                               ▼
                          Harnesses
                               │
                               ▼
                         Agent Runtime
                               │
                    ┌──────────┴──────────┐
                    ▼                     ▼
             Capability Platform     Persistence
                    │                     │
                    └──────────┬──────────┘
                               ▼
                         Host Authority
                               │
                   ┌───────────┼───────────┐
                   ▼           ▼           ▼
                 Local       Remote       Cloud
```

The final mental model is simple:

```text
The Model provides intelligence.
The Harness defines the domain.
The Runtime provides lifecycle.
Capabilities provide abilities.
Host Authority provides controlled execution.
Persistence provides continuity.
Products provide experience.
```

When a new domain, client, model, or execution environment appears, the architecture should let it extend the correct layer without forcing the whole system to be redesigned.
