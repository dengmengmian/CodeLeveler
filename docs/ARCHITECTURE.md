# CodeLeveler Architecture

This document defines CodeLeveler's long-term architecture model, design principles, ownership boundaries, architecture invariants, allowed and forbidden changes, and evolution direction.

Chinese version: [`ARCHITECTURE.zh-CN.md`](ARCHITECTURE.zh-CN.md). The English and Chinese versions should remain semantically equivalent.

This document answers four questions only:

1. What kind of system is CodeLeveler?
2. Why are responsibilities and boundaries divided this way?
3. Under this architecture, what is allowed and what is forbidden?
4. In what direction should the architecture evolve?

This document does not record implementation status, migration history, known defects, verification results, source locations, or version-specific implementation detail.

---

## 1. Architecture Vision

CodeLeveler should not be understood as a Coding Agent that continuously absorbs more capability.

Its long-term role is:

> **A reusable Agent Runtime / Harness Foundation. The Coding Agent is the first product built on top of it, not the highest abstraction in the system.**

Different agent products should be able to share the same Runtime, Capability, Authority, and Persistence foundation without inheriting Coding semantics.

The minimal model is:

```text
Model
  ↓
Harness
  ↓
Agent Runtime
  ↓
Capabilities
  ↓
Host Authority
  ↓
Operating System
```

With products included:

```text
                         Products
                            │
              ┌─────────────┼─────────────┐
              ▼             ▼             ▼
            Coding        Review        Future
              │             │             │
              └─────────────┼─────────────┘
                            ▼
                         Harnesses
                            │
                            ▼
                     Agent Runtime
                            │
                  ┌─────────┴─────────┐
                  ▼                   ▼
             Capabilities        Persistence
                  │                   │
                  └─────────┬─────────┘
                            ▼
                     Host Authority
                            │
                            ▼
                     Operating System
```

The purpose of this structure is not to maximize abstraction. It is to give every class of complexity the correct owner.

---

## 2. Core Design Model

The architecture can be compressed into seven principles.

```text
Model owns intelligence.
Harness owns domain semantics.
Runtime owns lifecycle and mechanical correctness.
Capability owns reusable domain capability.
Host Authority owns controlled real side effects.
Every persistent fact has one authoritative owner.
Product projects runtime truth; it does not create it.
```

One of the most important boundaries is:

> **Engine owns lifecycle, not agent intelligence.**

The Engine runs the system. It does not think for the agent.

---

## 3. Model: Intelligence Belongs to the Model

The model owns work that requires reasoning:

```text
goal understanding
planning
investigation
tool choice
code and content generation
debugging
trade-offs
semantic judgment
recovery from its own mistakes
```

Runtime and Harness should provide:

```text
clear capability
stable semantics
precise errors
real environment state
deterministic mechanical constraints
reliable execution
```

They should not build a second hidden reasoning system to compensate for model limitations.

Therefore:

```text
model capability limit
    ≠
runtime defect
```

Model intelligence is an input to the system, not a variable the Runtime must normalize.

---

## 4. Harness: Domain Semantics Belong to the Harness

The Harness is the domain layer between the Model and the reusable Runtime.

It defines:

> In this product domain, what the agent may do, what it can see, how it interacts with Runtime capability, and what domain completion means.

A Coding Harness may own:

```text
Coding domain contract
repository semantics
Coding Tool Surface
verification semantics
delegation semantics
write-collaboration semantics
Coding Completion Semantics
```

A future Review Harness may own:

```text
Review Target
Review Scope
Finding
Severity
Evidence
Review Verdict
Review Completion Semantics
```

Harnesses are siblings:

```text
                 Agent Runtime
               /       |        \
              ▼        ▼         ▼
           Coding    Review    Research
           Harness   Harness   Harness
```

One Harness should not inherit another Harness's domain semantics.

---

## 5. Agent Runtime

Agent Runtime is CodeLeveler's reusable execution foundation.

It contains two logical parts:

```text
Agent Kernel
    +
Persistent Runtime
    =
Reusable Agent Runtime
```

### 5.1 Agent Kernel

The Agent Kernel owns the generic model interaction loop:

```text
model interaction
tool loop
streaming
round lifecycle
budget
usage
retry
backoff
deadline
cancel
stop
```

It should not know:

```text
Coding
Review
Repository Workflow
product-specific prompts
domain-specific completion
UI
user acceptance
```

The purpose of the Kernel is that any Harness can run on the same agent loop.

### 5.2 Persistent Runtime / Engine

The Engine owns the mechanical lifecycle required for durable execution:

```text
Session
Task
Turn
Event
Persistence
Resume
Recovery
Cancellation
Background Lifecycle
Ownership
Runtime Outcome
```

The core boundary is:

```text
Engine owns lifecycle.
Harness owns domain semantics.
Model owns reasoning.
```

The Engine may know whether a Task is running, paused, cancelled, resumed, or ended. It should not define whether a Coding task is semantically complete.

---

## 6. Capability Architecture

Capabilities represent the reusable abilities the system actually possesses.

Typical capabilities include:

```text
Workspace
Command Execution
Browser
Search
Code Intelligence
Version Control
Memory
Media
Skills
Remote Execution
External Services
```

The core rule is:

> **Capability expresses what the system can do. Tool expresses how the model invokes it.**

### 6.1 Capability Is Not Tool

For example:

```text
Model
  ↓
read_file
  ↓
Workspace Capability
  ↓
Host filesystem authority
```

`read_file` is a model-facing interface.

Workspace is the reusable capability.

Therefore:

```text
Tool ≠ Capability
```

And the architecture does not require:

```text
One Capability = One Tool
```

A Capability may be exposed through multiple Tools and may also be consumed directly by a Harness, Runtime, or Product at the appropriate boundary.

### 6.2 Tool Thin, Capability Thick

Tools should remain thin.

A Tool owns:

```text
model-facing name and description
schema
input decoding
capability invocation
result rendering
precise errors
```

A Capability owns:

```text
real domain behavior
reusable logic
capability-local state
capability lifecycle
consistency
```

Long-lived state, service discovery, shared runtime machinery, and authority decisions should not be pushed into Tools merely because a Tool needs access to them.

### 6.3 Capability Is a Responsibility, Not a Mandatory Physical Module

A Capability is first an ownership boundary. It does not imply one crate, one service, one process, or one trait.

Physical separation should be driven by real boundaries such as:

```text
multiple real consumers
independent lifecycle
independent security boundary
independent protocol boundary
independent persistence boundary
remote deployment boundary
```

Do not split components merely to make an architecture diagram symmetric.

---

## 7. Tool Surface

The model should not see every internal capability in the system.

It should see only the Tool Surface selected and exposed by the current Harness for the current product.

```text
Capabilities
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

The goal of a Tool Surface is:

> Preserve necessary capability while presenting the model with the clearest, most stable, lowest-ambiguity operation surface possible.

A good model-facing Tool should have:

```text
a clear intent
stable semantics
predictable inputs and outputs
precise errors
clear boundaries from neighboring tools
```

The system may enable or disable a Tool based on real mechanical capability. It must not secretly change tool semantics because a model is considered weak or a task looks difficult.

---

## 8. Host Authority

An Agent may request side effects, but it does not own host power.

The core rule is:

```text
Agent proposes.
Host Authority decides and performs.
```

The relationship is:

```text
Model / Harness
      ↓
Side-effect request
      ↓
Host Authority
      ↓
Operating System
```

Host Authority owns controlled real side effects such as:

```text
filesystem mutation
process execution
network authority
sandbox
permission
approval
workspace boundary
process lifecycle
```

This keeps what the Agent wants separate from what the host permits.

No model-facing interface may become a second path around Host Authority.

---

## 9. Authority Model

CodeLeveler distinguishes three levels of truth:

```text
Mechanical Truth
      ≠
Semantic Satisfaction
      ≠
User Acceptance
```

### 9.1 Runtime Authority

Runtime may authoritatively record mechanical facts such as:

```text
whether a command ran
exit status
whether a file changed
whether an event occurred
whether verification ran
whether an artifact exists
whether a process ended
```

### 9.2 Model Semantic Authority

The model interprets whether those facts are enough to satisfy the domain goal.

For example:

```text
tests passed
```

does not automatically imply:

```text
the requested feature is semantically complete
```

### 9.3 User Acceptance

The user owns final acceptance.

Therefore:

```text
Runtime owns facts.
Model owns semantic interpretation.
User owns acceptance.
```

---

## 10. Persistence Architecture

Long-lived agents require reliable persistence.

The core rule is:

```text
One persistent fact
      ↓
One authoritative owner
      ↓
One canonical representation
```

Task state, Turn state, Ownership, Evidence, Usage, Artifacts, and Background Work all follow this rule.

### 10.1 Persist Before Forward

Authoritative Runtime events should follow:

```text
Runtime Fact
    ↓
Persist
    ↓
Forward / Project
    ↓
Client
```

A client should not observe an authoritative Runtime fact before the Runtime has reliably recorded it.

### 10.2 Persistence Provides Continuity

Persistence is not merely chat history. Its long-term purpose is to support:

```text
resume
recovery
cross-device continuity
background work
long-running tasks
child session durability
auditability
```

---

## 11. Product Architecture

The Product layer owns experience, composition, and delivery.

Examples include:

```text
CLI
TUI
Web
Desktop
Mobile
Remote Client
```

A Product may decide:

```text
how information is presented
how interaction is organized
which capabilities form a product
default experience
navigation
visualization
```

But a Product does not own Runtime Truth.

The core rule is:

> **UI is a projection of Runtime truth.**

A Product may own local view state, but authoritative task, execution, permission, tool, and persistent state must come from the proper owner.

---

## 12. Client / Runtime Boundary

Clients and Runtime should communicate through a stable protocol rather than shared internal state.

```text
Client Command
      ↓
Runtime
      ↓
Runtime Event
      ↓
Client
```

The same Runtime may serve multiple clients:

```text
             Runtime
          /     |      \
         ▼      ▼       ▼
       TUI     Web    Mobile
```

A Client and Runtime may also live on different machines:

```text
Client
  ↓
Transport
  ↓
Remote Runtime
```

This gives local, remote, and cloud execution the same client/runtime architecture.

---

## 13. Multi-Agent Architecture

Multi-Agent is not simply "one Agent calling several models."

It is multiple Agent lifecycles collaborating under one Runtime Authority.

```text
                 Parent Agent
                /      |      \
               ▼       ▼       ▼
          Explorer   Worker   Reviewer
               │       │       │
               └───────┼───────┘
                       ▼
                 Shared Runtime
                       │
          ┌────────────┼────────────┐
          ▼            ▼            ▼
      Persistence   Ownership   Capabilities
```

Multi-Agent Runtime provides the common mechanical collaboration layer:

```text
child lifecycle
session relationship
ownership
write scope
capability access
background execution
settlement
cancellation
persistence
result handoff
```

Role semantics belong to the Harness. Lifecycle and mechanical correctness belong to Runtime.

Multi-Agent must not create a second lifecycle, authority, or persistence system around the primary Runtime.

---

## 14. Provider & Model Boundary

Model Providers differ in protocol and mechanical capability.

Those differences belong to the Provider / Protocol layer, not to a Harness intelligence-compensation layer.

Negotiable facts include:

```text
tool calling
streaming
reasoning transport
vision
structured output
forced tool choice
context window
output limit
wire format
```

The system should:

```text
discover capability
negotiate explicitly
report honestly
enable or disable dependent capability from mechanical conditions
```

It should not:

```text
change tool semantics to simulate missing provider capability
add hidden behavior to normalize model intelligence
```

---

## 15. Dependency Direction

Architecture dependencies must remain one-way.

```text
Foundation
    ↑
Runtime / Capabilities
    ↑
Harnesses
    ↑
Products
```

From the caller's perspective:

```text
Product
   ↓
Harness
   ↓
Runtime / Capabilities
   ↓
Foundation
```

The core requirements are:

```text
Runtime does not depend on a concrete Harness.
Harness does not depend on a concrete Product UI.
Foundation does not depend on upper-layer domain concepts.
Capability does not depend on the specific Tool or Product that invokes it.
```

Lower layers provide mechanisms. Upper layers provide composition and semantics.

---

## 16. Conceptual Task Flow

From user request to host side effect:

```text
User
  ↓
Product
  ↓
Harness
  ↓
Agent Runtime
  ↓
Model
  ↓
Tool Intent
  ↓
Harness Tool Surface
  ↓
Capability
  ↓
Host Authority
  ↓
Operating System
```

The result returns through:

```text
Operating System
  ↓
Capability Result
  ↓
Runtime Fact
  ↓
Persistence
  ↓
Runtime Event
  ↓
Harness / Product
  ↓
User
```

This flow encodes three core constraints:

1. Model does not directly own host side effects.
2. Runtime records mechanical facts but does not replace domain semantics.
3. Product presents truth but does not redefine it.

---

## 17. Architecture Invariants

These rules are CodeLeveler's long-term architecture constitution.

### 17.1 Intelligence Boundary

```text
Model owns intelligence.
Runtime must not simulate agent intelligence.
```

### 17.2 Harness Boundary

```text
Harness owns domain semantics.
Runtime must not define Coding, Review, or other product semantics.
```

### 17.3 Lifecycle Boundary

```text
Engine owns lifecycle, not agent intelligence.
```

### 17.4 Capability Boundary

```text
Tool is an adapter.
Capability is the reusable ability.
```

### 17.5 Authority Boundary

```text
Agent proposes.
Host Authority decides and performs.
```

### 17.6 Persistence Boundary

```text
One persistent fact, one authoritative owner.
```

### 17.7 Product Boundary

```text
Product projects truth.
Product does not create runtime truth.
```

### 17.8 Dependency Boundary

```text
Dependencies point downward toward more general layers.
Lower layers do not depend on product-specific layers.
```

### 17.9 Multi-Harness Boundary

```text
A new Harness must not require redesigning Agent Runtime.
```

### 17.10 Reliability Boundary

```text
Moving responsibility between layers must not weaken mechanical correctness,
authority, persistence, cancellation, recovery, or safety guarantees.
```

---

## 18. Architecture Guardrails: Allowed and Forbidden

This section is the direct decision rule for architecture changes.

These are not suggestions. They define the allowed design space.

### 18.1 Allowed

#### A. Add Domain Capability Above Runtime

Allowed:

```text
new Coding workflows
new Review semantics
new Research or other Harnesses
changes to a product's Tool Surface
new domain Completion Semantics
```

As long as those semantics remain in the owning Harness / Product and do not leak downward into reusable Runtime.

#### B. Extend Domain-Neutral Runtime Mechanisms

Runtime may gain genuinely reusable mechanical capability such as:

```text
new lifecycle states
stronger resume / recovery
stronger cancellation
generic background lifecycle
generic parent / child session relationships
generic resource budgets
generic event and persistence mechanisms
```

The mechanism must not require Coding, Review, or another domain vocabulary.

#### C. Add Reusable Capabilities

New Capabilities are allowed, for example:

```text
Browser
Remote Execution
Search
External Service
New Code Intelligence
New Workspace Ability
```

when they represent real reusable ability with a clear ownership boundary.

#### D. Add Tool Adapters for Capabilities

A new Tool may expose a new model intent.

The conditions are:

```text
Tool remains an adapter
semantics are stable
Capability Runtime is not duplicated
no new Authority is created
Tool does not become a persistent-state owner
```

#### E. Extend Provider / Protocol Adapters

New models, protocols, and wire formats may be supported.

Provider differences may be normalized mechanically, but Agent behavior semantics must not be changed to fake a mechanical capability the provider does not have.

#### F. Add Products and Clients

Allowed:

```text
new TUI / Web / Desktop / Mobile clients
remote controllers
new Agent products
new domain Harnesses
```

as long as they consume Runtime Truth rather than create a second Runtime Truth.

#### G. Add Local / Remote / Cloud Execution Backends

Host Authority may support different execution locations:

```text
Local Host
Remote Host
Container
VM
Cloud Worker
```

Execution location may change. Authority and Agent semantic boundaries remain the same.

#### H. Introduce an Abstraction When There Is Real Evidence

A new crate, trait, registry, manager, adapter layer, or generic framework may be introduced when at least one real driver exists:

```text
two real implementations or consumers
real dependency inversion
independent security boundary
independent protocol boundary
independent persistence boundary
independent lifecycle
remote deployment boundary
observed ownership or coupling defect
```

Abstraction follows real boundaries, not imagined futures.

---

### 18.2 Forbidden

#### A. Do Not Push Product Semantics Into Reusable Runtime

Kernel, Engine, and Foundation must not learn:

```text
Coding Prompt
Review Finding
Repository Workflow
product-specific Tool Preference
domain-specific Done definitions
```

Those belong to Harnesses.

#### B. Do Not Make Runtime Think for the Model

Do not add hidden reasoning compensation because a model is weak, such as:

```text
deciding when the model should plan
deciding what it should investigate next
automatically replacing a badly chosen Tool with another Tool
hidden task-solving retries
duplicate tool semantics for weaker models
changing domain behavior by model intelligence class
```

Mechanical retry, network recovery, protocol adaptation, and schema validation are engineering reliability and are not prohibited by this rule.

#### C. Do Not Create a Second Host Side-effect Authority

Tool, Harness, Product, or extension code must not bypass the common Authority by creating a parallel:

```text
filesystem mutation path
process execution path
permission path
approval path
sandbox path
workspace write authority
```

One controlled side effect must not have competing Authorities.

#### D. Do Not Turn Tools Into Runtime or Service Locators

Do not place universal service collections, long-lived state, permission decisions, process lifecycle, or global policy into every Tool.

A Tool should not receive a universal context that gives it unrelated capabilities.

#### E. Do Not Create Multiple Sources of Persistent Truth

Forbidden:

```text
UI deriving authoritative Task state
Harness and Engine each storing authoritative versions of the same fact
event stream and database becoming independent authorities
multiple components overwriting one canonical fact
```

One fact must have one canonical owner.

#### F. Do Not Introduce Reverse Dependencies

Forbidden:

```text
Foundation → Harness
Runtime → concrete Product
Capability → concrete UI
generic Harness → another domain Harness
```

Lower layers must not depend on higher product layers for convenience.

#### G. Do Not Use Semantic-Changing Fallbacks to Pretend Capability Exists

Forbidden:

```text
regex search silently becoming literal search
structured edit silently becoming another edit meaning
provider missing a capability but another behavior path pretending it exists
```

A fallback may replace an implementation only if the contract and meaning remain the same.

#### H. Do Not Change Mechanical Capability Boundaries Based on Task or Model "Intelligence"

Capability availability should come from real mechanical conditions such as:

```text
model protocol capability
host capability
runtime capability
harness requirement
user configuration
```

It must not come from:

```text
the task looks difficult
the model looks weak
the model performed poorly this round
```

#### I. Do Not Pre-build Frameworks for Hypothetical Futures

Do not add generic abstractions only because:

```text
we may need it later
it is more generic
Clean Architecture looks more complete
there may be a second implementation someday
```

The architecture must allow future extension. That does not mean implementing the future in advance.

#### J. Do Not Let Multi-Agent Bypass the Shared Runtime

Child Agents, Reviewers, and Workers must not create separate systems for:

```text
lifecycle
permission
ownership
persistence
cancellation
settlement
```

Roles may differ. Mechanical runtime rules remain shared.

#### K. Do Not Encode Local-only Assumptions Into Agent Semantics

Agent, Harness, and Runtime semantics should not assume execution always happens on the current machine.

Local, remote, container, and cloud are deployment choices of Execution Authority, not four different Agent architectures.

---

### 18.3 Architecture Change Test

Before a change enters Foundation, Runtime, or a Capability layer, it should answer:

```text
Who owns it?
Is it domain semantics or mechanical mechanism?
Could a second Harness reasonably reuse it?
Does it create a new Authority?
Does it create a second source of persistent truth?
Does it introduce a reverse dependency?
Does it change the meaning of an existing capability?
Is it abstracting an imagined future rather than a real boundary?
```

If a feature can naturally remain in a Harness, it should not be pushed into Foundation merely to make it "generic."

If a new Harness requires redesigning the Agent Kernel before it can exist, suspect the boundary design before assuming the Kernel needs more domain concepts.

---

## 19. Evolution Direction

CodeLeveler's long-term evolution is not about making one Coding Agent increasingly large. It is about allowing this Foundation to support more agent products and more execution forms.

These are architecture directions, not a version roadmap.

### 19.1 Multi-Harness

Evolve from one Coding product to multiple domain Harnesses:

```text
                    Agent Runtime
                 /       |        \
                ▼        ▼         ▼
             Coding    Review    Research
             Harness   Harness   Harness
```

Each Harness:

```text
owns its domain semantics
selects its Capabilities
defines its Tool Surface
owns its Completion Semantics
```

Runtime remains domain-neutral.

### 19.2 Multi-Agent Runtime

Evolve from a single Agent lifecycle into an Agent collaboration graph:

```text
Single Agent
    ↓
Parent / Child
    ↓
Role-based Agents
    ↓
Multi-Agent Runtime
```

Future roles may include:

```text
Planner
Explorer
Worker
Reviewer
Specialist
```

But they share one mechanical foundation:

```text
lifecycle
persistence
ownership
authority
capability negotiation
cancellation
settlement
```

Multi-Agent is an extension of Runtime capability, not a second system.

### 19.3 Capability Platform

Capabilities evolve from "backends used by the Coding Agent" into an independently composable capability platform.

```text
                  Capabilities
              /        |         \
             ▼         ▼          ▼
          Coding     Review      Other
```

Long-term capability families may include:

```text
Workspace
Execution
Browser
Search
Memory
VCS
Code Intelligence
Media
Remote Compute
External Services
```

Harnesses compose what they need instead of inheriting one giant tool set.

### 19.4 Local → Remote → Cloud

Runtime and Host Authority should not be bound to one local machine.

The long-term shape is:

```text
Harness
   ↓
Agent Runtime
   ↓
Execution Authority
   ├── Local Host
   ├── Remote Host
   ├── Container
   ├── VM / Isolated Worker
   └── Cloud Worker
```

Only execution location changes. Core Agent semantics do not.

### 19.5 Durable Agents

Agent lifecycles will become longer.

Runtime should naturally support:

```text
long-running goals
pause / resume
background execution
cross-device continuation
child session durability
recoverable work
persistent context
```

An Agent is no longer equivalent to one chat request. It becomes a durable, recoverable task entity.

### 19.6 Capability Negotiation

Models, hosts, and workers expose different mechanical capabilities.

The system should explicitly negotiate:

```text
model capabilities
host capabilities
runtime capabilities
harness requirements
user configuration
```

Available capability comes from the intersection of those facts, not hidden guesses.

```text
Available Capability
    =
Model ∩ Host ∩ Runtime ∩ Harness ∩ Configuration
```

If a required mechanical capability is absent, disable the dependent feature honestly rather than change semantics and pretend it exists.

### 19.7 Agent Platform

The final shape is not "a more complex Coding Agent." It is a runtime platform for building different Agent products.

```text
                         Products
             ┌─────────────┼─────────────┐
             ▼             ▼             ▼
          Coding         Review         Others
             │             │             │
             └─────────────┼─────────────┘
                           ▼
                        Harnesses
                           │
                           ▼
                     Agent Runtime
                           │
                 ┌─────────┴─────────┐
                 ▼                   ▼
          Capability Platform    Persistence
                 │                   │
                 └─────────┬─────────┘
                           ▼
                     Host Authority
                           │
                ┌──────────┼──────────┐
                ▼          ▼          ▼
              Local      Remote      Cloud
```

In this model:

```text
Model provides intelligence.
Harness provides domain semantics.
Runtime provides lifecycle and mechanical correctness.
Capability provides reusable ability.
Authority provides controlled real execution.
Persistence provides continuity.
Product provides experience.
```

That is CodeLeveler's long-term architecture direction.
