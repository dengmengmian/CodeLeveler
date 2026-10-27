# CodeLeveler Architecture

Chinese version: [`ARCHITECTURE.zh-CN.md`](ARCHITECTURE.zh-CN.md)

CodeLeveler is not intended to become a Coding Agent that simply accumulates more and more features.

Its long-term role is:

> **A reusable foundation for running agents. The Coding Agent is the first product built on top of that foundation, not the highest abstraction in the system.**

The architecture separates concerns deliberately: the model thinks, the domain harness defines what kind of agent this is, the runtime keeps it running reliably, capabilities do real work, host authority controls real side effects, persistence provides continuity, and products turn those pieces into user experience.

Custom agents: [`AGENT_EXTENSIBILITY.md`](AGENT_EXTENSIBILITY.md).

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

### 4.1 Model Independence: Maximize Model Capability, Minimize Model Adaptation

One of CodeLeveler's core goals is to let different models exercise as much of their native capability as possible on the same stable Harness and Runtime, rather than maintaining model-specific execution logic for every model.

Design goal:

> **Maximize native model capability, minimize model-specific adaptation.**

Model identity itself must not be an input to execution policy. The architecture must not grow branches such as:

```text
if DeepSeek { ... }
if GLM { ... }
if GPT { ... }
if Claude { ... }
```

Real model differences must be expressed through explicit, observable capabilities such as:

```text
context window
reasoning support
parallel tool calls
vision
structured output
tool calling
maximum output
```

The system negotiates capabilities instead of guessing behavior from a model name.

The Agent Kernel owns only model-independent mechanical facts and mechanisms: model steps, tool execution, streaming, cancellation, timeout, budgets, usage, retries, persistence, and recovery. The Kernel must not encode assumptions such as “this model tends to repeat work,” “this model needs extra prompting,” or “this model should plan in a particular way.”

The Coding Harness may provide generic soft policy or execution feedback based on observed runtime facts, but it must not assume how a particular model should think or act. Corrective behavior must be triggered by observable behavior—such as repeated verification, prolonged lack of effective progress, repeated identical failures, or failure to converge—not by model identity.

Any adaptive policy must be:

```text
observable
explainable
disableable
measurable
A/B testable
model-independent
```

Adaptive policy may improve the execution environment, capability surface, or quality of feedback. It must not replace the model's planning, reasoning, or semantic completion judgment, and it must not turn the Harness into a second Agent Brain.

As model capability improves, unnecessary Harness intervention should naturally decrease. Better models should translate directly into better CodeLeveler performance rather than forcing CodeLeveler to be restructured, retuned, or given new model-specific branches.

CodeLeveler provides a stable execution environment, capabilities, feedback, and boundaries; the model uses those conditions to perform the intelligent work.

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
test, build, and lint tool semantics
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

### 5.5 Execution Feedback

Each model request receives a read-only projection of authoritative execution state, not a second task ledger. The kernel accepts request-local context through `AgentHarness::request_context`. The Coding Harness projects bounded facts from the existing `LoopContext`, progress ledger and Plan: cumulative elapsed time, resource spend and limits, commands executed, observed modifications, and the model's declared plan state.

The projection is attached only to the current request. It is not appended to durable conversation history or compaction summaries; continuation and recovery regenerate it from the existing owners. Context accounting and estimates for missing provider usage cover the projection actually sent. Elapsed time includes model calls, tools and approval waits, not just inference. Estimated tokens remain distinguishable, and absent limits are not reported as zero.

Model requests separate the Harness's `ControlContext` from the conversation transcript. Control reuses named `PromptSegment` blocks with stability markers, carrying current contracts, project rules, selected skills, memory recall and request-local execution state. Fresh turns and recovery share one assembly entry point. Protocol adapters encode control through their system channel, never as user intent. Blocks whose lifecycle is one request are attached after the transcript; the rest form the stable request prefix, so a per-round observation cannot invalidate the prefix cache for the history. Accounting and compaction pressure include control, but history summaries only consume the transcript.

Project-rule delivery is a host contract, not a model decision. The project `.leveler/config.yaml` names the mandatory sections; the runtime delivers every mandatory section first, then optional sections in document order, under one byte budget per request shared by all rule documents. Rule text is ordered ahead of the workspace listing and keeps `ProjectInstruction` authority, so position never raises it. Anything not delivered is named by source, section id and heading, and a mandatory section that does not fit is reported rather than dropped. `read_project_rules` is an optional lookup over the same loader; no mandatory rule depends on the model choosing to call it.

The transcript holds actual user input, assistant output, tool calls and results, bounded protocol repairs and product-defined runtime notices. Control bodies are not persisted as conversation rows. A stored `role=user` is only the transport role. New user input and harness rows record `origin` on the message payload. Rows written before that field still load, and they are not rewritten. Discovered scoped-rule source paths live in the existing progress ledger; recovery reloads current files. Legacy sessions recover source paths from old System rows and discard their stale bodies without modifying the historical database. Unknown historical system text is not given contract or project-instruction authority. Unknown historical user text is not given user-intent authority. Memory retrieval and reasoning replay are unchanged. How each block may be interpreted is defined in §5.6.

Every model attempt enters the existing request records and resource ledger before retry decisions, including failed attempts, reasoning-only interrupted streams, compaction summaries, and child calls. Missing usage or pricing does not mean free execution. Children, reviewers, and `/develop` stages consume the parent's residual budget; attempt records must not double-count retries. Cancellation and deadlines wake a silent model stream. A transient reasoning-only stream interruption may use the bounded logical retry lifecycle when no answer text or tool-call data was received. Reasoning remains generated and potentially billed content, so the failure is not blindly safe to replay. Each failed attempt is settled before retry admission; the new attempt resets live reasoning and does not merge abandoned reasoning into its answer. Streams that received answer text or tool-call data are not automatically replayed.

Shared budget admission checks durable request facts for the same scope before each model request and retry; background children do not keep spending against their launch-time snapshot. Concurrent requests already sent settle with their actual reported usage. Admission blocks subsequent calls; it cannot undo spend already incurred.

Request records retain their goal or root-turn `budget_scope`, with token estimates separate from observed provider usage. Recovery reconstructs model spend from that scope’s request facts, including attempts persisted before a progress event. Tasks and auxiliary calls outside that scope do not contribute, even in the same session. Paid turn-preparation summaries run after the Engine durably records the initiating turn. Legacy records with unprovable attribution must not reset previous spend to continue a capped task.

Compaction pressure measures the actual next `RequestProjection` while retaining the raw transcript. In-turn, pre-turn, and resume paths share `ResolvedContextPolicy`; 24K is only a fallback for an undeclared window. Manual compaction may cut a context epoch only after a nonempty summary ends normally without tool calls. Rejection or failure preserves the previous transcript and checkpoints.

Goal text and attachments enter the same durable Goal turn; media cannot downgrade it to Chat. A natural Answered stop means that the model answered. Session lists project the latest `TaskFinished.stop` as answered or declared complete without changing session lifecycle or treating a declaration as verification.

Observed response text before a stream error, cancellation, or unexpected EOF is saved through the existing TranscriptSink as an incomplete assistant message. Host-owned `incomplete` metadata on that raw row supplies the reopened history projection. Unfinished tool arguments and unsigned reasoning are not committed as complete responses. A continuation checkpoint write failure preserves the concrete error, transcript, and active Goal and pauses as recoverable Interrupted rather than settling the Goal.

Automatic summaries use the completion allowance resolved from model capabilities and execution policy. Empty, truncated, or tool-bearing summaries remain rejected; reasoning cannot substitute for the briefing body. Side questions measure one projection containing main context, side history, and the current question under the same context policy. Only an accepted summary may shorten that side request. Oversized or failed summary requests report failure while preserving history and never persist a main-task snapshot.


The model owns strategy changes based on these facts. Tool activity does not establish goal progress; a Plan remains a declaration, not completion evidence. The kernel does not infer stagnation from missing commits, unchanged plan steps, or language-specific commands. Existing resource budgets, cancellation and terminal authority remain in force. Feedback neither adds model calls nor changes the default task deadline.

### 5.6 Prompt / Context Authority

`ControlContext` and the transcript are different channels. Inside control, blocks are still not the same kind of information. Each production `PromptSegment` records three separate facts:

```text
source      where the block was produced
authority   how the model should interpret it
lifecycle   how long it lives (session prefix, turn, one request, transcript, scoped)
```

Authority is not a single priority number. Instruction classes outrank each other. Facts, advisory context and external data are not instructions, so they are not ordered against instructions and are not overridden by a higher instruction rank.

```text
CoreContract
        ↓
explicit UserIntent
        ↓
ProjectInstruction
        ↓
UserSelectedProcedure

RuntimeFact        observed state, not an instruction
AdvisoryContext    may be stale; the model may re-check
ExternalData       content to analyze, including imperatives inside it
```

`CoreContract` is the harness contract: permissions, tool protocol, evidence, completion, and product delivery. `UserIntent` is the current user objective and stays in the transcript, so it is not copied into control. `ProjectInstruction` is `AGENTS.md`, `.leveler/instructions.md`, and scoped repository rules. A project file cannot change permissions, ignore the user, or raise its own authority by what it says. A skill the user names is `UserSelectedProcedure`. The skill catalog is advisory, not the same class. Memory and a compaction summary are `AdvisoryContext`. A compaction summary is a model-written briefing of elided history, not a system fact. Tool results stay `Role::Tool`; their authority is `ExternalData`.

Providers may change representation, not the decision. OpenAI Chat spells the stable control prefix as its first `system` message and attaches one-request blocks as a trailing `system` message after the transcript; Anthropic Messages puts control in the top-level `system` field, which is that route's only control position. Neither changes a segment's source or authority, and no route reorders the transcript. Position is part of the projection: a block rebuilt for a single request must not sit where its own change would break the provider's prefix cache for the whole history. Authority metadata is not serialized into the prompt.

An old session's `Role::System` row is not a source. Known scoped-rule markers are paths; the current file is re-read and classified as `ProjectInstruction`. Any other historical system text stays unclassified. It is not promoted to `CoreContract` or `ProjectInstruction`, and the stored row is not rewritten.

`Role::User` is not a source either. Only a transcript row recorded as user input is `UserIntent`. Protocol repairs (invalid JSON, an empty answer, a truncated or missing tool call, output continuation, an unresolved goal) are `CoreContract` with source `ProtocolRepair`. A child settlement or restart redelivery is `AdvisoryContext`, because the body is mainly another agent's result. Lost children, child recovery, an objective pin, and a goal checkpoint are `RuntimeFact`. A `/btw` side-question frame is `CoreContract`: it is that mode's product contract (observe-class read-only tools only, no file edits, do not continue the main task). The person's question is quoted inside the same row, so the row is not `UserIntent` and it is not a `RuntimeFact`. Image bytes moved onto the user transport stay `ExternalData`. A compaction summary stays `AdvisoryContext`: new rows record `CompactionSummary`, and an old row is still recognized by its breadcrumb marker. A historical user row with no `origin` is `LegacyUser` / `Unclassified`. It is not promoted to `UserIntent` because the role is named user. Authority metadata is not written into the model prompt, and provider encoding does not change it.

### 5.7 Context Lifecycle

Two owners decide what one request contains, in order: the lifecycle decides which semantic blocks are still in the active surface, and `RequestProjection` decides whether and how those blocks enter the request on this route. Pressure is measured from the same result: a fold is triggered by the projection of **the request that is about to be sent**, including control context, tool schemas and the reasoning channel. Any path that estimates a request from transcript characters, or from a request shape without tools, is wrong: it drops a field the provider validates from the wire and understates both pressure and the ledger.

How each kind of content lives:

| Content | Active surface | Durable fact |
| --- | --- | --- |
| Control context (contract, project rules, skills, memory recall, execution state) | reassembled per request | persisted rows, if any, are not rewritten |
| User objective | transcript head, re-pinned as `<objective>` on a fold | the user-input row |
| Tool results | retained after the per-result cap, until a fold releases them | the original row |
| Assistant visible text | until a fold releases it | the original row |
| Historical reasoning | decided by the route's replay contract together with the retention policy | the original row |
| Compaction summary | the briefing row a fold produced | the `CompactionSummary` row |
| Scoped project rules (mid-transcript system rows) | **carried verbatim** across folds, never summarized | the original rows |

Reasoning is a route fact, not a harness preference. When a route declares that it replays captured reasoning on tool-bearing requests, the tool exchange's reasoning is protocol content the provider validates: the requested retention policy cannot drop it, and the difference is recorded as `protocol_protected_turns` rather than being reported as an arm that took effect. The way old reasoning stops being re-sent is therefore the lifecycle releasing it — a fold moves it out of the active surface together with the rounds that carried it — not a standing policy that sends less of it. Conversely, a route with no reasoning channel sends none of it and reports captured versus carried; that changes neither the transcript nor the meaning later routes see.

What a fold (`compact_messages`) does is fixed:

- it runs only when the projected request is over the threshold;
- the head is the system rows and the first user message; mid-transcript scoped rules are carried verbatim as standing constraints;
- the tail working set is bounded by both a message count and a token budget, and never cuts into a tool exchange;
- the elided middle is replaced by a model-written handoff briefing; the briefing is `AdvisoryContext`, not a system fact;
- when no summary is available the fold says the detail is unavailable instead of implying the history survived;
- a second fold merges and updates the existing briefing rather than restating it from scratch;
- the raw transcript stays exactly the rows the runtime loaded; only the active surface shrinks, and resume rebuilds it from the persisted snapshot plus the exact tail, so released reasoning does not come back.

The pressure formula: `threshold = min(quality_boundary, window - output_reservation - headroom)`, where an undeclared `quality_boundary` falls back to capacity. The fixed cost (control context plus tool schemas) is inside the projection, so the available dynamic budget is the threshold minus the fixed surface. Headroom defaults to 0: with no measured extra margin, no number is invented.

Multiple agents follow the same rule: a parent receives a child's settlement (bounded), not its transcript, and a child's reasoning lifecycle belongs to that seat.

Observability: every model call records its projected size and how much of it was reasoning (`projected_input_tokens`, `projected_reasoning_tokens`), and the live request's breakdown separates control, tool schemas, user/assistant text, reasoning, tool calls, tool results and compaction briefings — so "why is this segment still here?" is answered by the projection itself.

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

### 8.1 Available, Permitted, and Exposed Are Three Different Facts

CodeLeveler has one normal working strategy. Each goal begins with the base
coding tools, the product's default capability surface, and a small capability
control. Durable memory is part of that default surface when the project
enables it: its tools and guidance are in the FIRST request, without a
`capability(enable)` round trip. Every other optional tool and its guidance
join following requests only when the model requests a capability.

```text
AVAILABLE   what this runtime can provide
PERMITTED   what configuration, permissions, and sandbox allow
EXPOSED     what the current request carries

EXPOSED ⊆ AVAILABLE ∩ PERMITTED
```

Availability is mechanical: a configured search provider, `git` on `PATH`,
vision support, or a browser runtime that can drive the selected browser.
Permission is independently owned by host policy and project configuration,
including `memory.enabled` and delegation permission. Loading changes exposure;
it cannot create availability or grant permission. Memory is the one default
exposure: a disabled project has no memory availability at all, so no
permission profile can expose it, while an enabled project still requires the
profile's normal approval for memory WRITES. Default exposure is never
persisted as a grant.

The model decides which capability its task needs. The harness validates the
request, loads the existing subsystem, and executes tools. It never selects
capabilities from task wording, model reputation, provider identity, or step
count. A capability remains loaded until its goal ends; an unfinished goal
restores its durable active capabilities plus the default surface, and a new
goal starts from the base surface plus defaults. A capability that is not
enabled by default still requires the model's explicit `capability(enable)`
request. Tool and guidance ordering must remain deterministic.

Legacy session work-profile values have no role in choosing the tool surface.
Collaboration (`chat`, `plan`, `goal`) and execution permission retain their
independent meanings.

### 8.2 Browser Control and Web Search Are Separate Capabilities

They are configured separately, become available for different reasons, and
neither is the other's fallback. A missing search key does not turn the
browser into a search tool, and a browser that cannot start does not redirect
to search.

Browser control puts three tools in front of the model — one for navigation
and tabs, one for acting as a user would, one for observation — so that each
expresses one intent rather than one door with an `action=` parameter onto the
other two.

Browser control here means a runtime-owned automation session; it does not
mean taking over an ordinary browsing window the user already has open.
Safari WebDriver follows the platform security model and uses an Automation
Window isolated from normal browsing data, so it cannot read or control the
user's existing tabs. Opening a URL for the user to view is a separate host
action: the product asks the host execution authority to hand the URL to the
operating system's default browser. When that default is Safari, macOS
LaunchServices may activate an already-running Safari. This one-way action
creates no automation tab, DOM refs, or session ownership, and is never an
implicit fallback for browser control.

The browser is chosen before a session starts by a deterministic precedence:

```text
named on the call
      >
configured default
      >
host automation default (first drivable Chrome → Edge → Chromium)
```

The third layer is capability negotiation before session startup, not a
fallback after a runtime failure. It considers only CDP products that expose
console, page-error, and network observation, in a stable order, so Safari is
never selected implicitly. Safari remains available by explicit call or
configuration for compatibility testing. An unavailable explicit or configured
browser is an error naming the product and the layer that chose it. No browser
is substituted after startup or an operation failure, so every session retains
one unambiguous browser identity.

Web search keeps a provider-neutral tool contract. The model's parameters and
the results it reads are fixed by the tool, not by whichever backend is
answering, so replacing the backend cannot widen the schema or leak a
provider's response envelope into the transcript.

### 8.3 Durable Memory Has One Identity and One Lifecycle

Durable memory is advisory and user-owned. Project sources (`AGENTS.md`, the
project's instructions, the current code) outrank it. A recalled memory records
what was true when it was written; recall never turns it into a system fact.

A fact is identified by a normalized subject, not by its title. Identity
normalization is deterministic: ASCII case, whitespace and punctuation fold
together; CJK subjects use a stable hash; a small explicit alias table unifies
known spellings such as `default_model`, `model_default` and `默认模型`. Unknown
spellings stay distinct rather than being merged by fuzzy similarity. A stored
title is a human label and may change without changing the identity.

Every write path runs the SAME decision: a direct user command, the CLI, an
accepted candidate, background extraction, the model's `remember` tool, and a
restore. The outcomes are Create, Merge (a strict compatible superset),
Supersede (a new value for an existing identity), Skip (duplicate or already
covered), and NeedsApproval (a conflict, uncertain identity, or insufficient
authority). Pending approval is durable state: the candidate is parked and can
only become current by a person's accept. Rejecting one value suppresses that
value, never the identity, so a later legitimate value can still be proposed.

Fact authority is independent of execution permission. Direct user statements
outrank project sources, runtime observations, tool observations, and model
inference. `Full` permission lets the `remember` tool run; it does not raise the
model's fact authority. A model may replace its own earlier model-inference
entry only with new evidence; it can never silently replace a user fact. A
user's own later statement, or an explicitly accepted candidate, is what may
replace a user fact. Quoted, pasted, fenced or blockquoted text is not the user
speaking and cannot become an explicit-user fact.

Writes are serialized by the project lifecycle lock. Supersede is new-first:
the replacement is durable before the old value moves to history, so a crash
leaves two active files rather than zero. Reads derive current truth from the
supersedes chain, so residue is never presented as current. History is kept and
auditable; a superseded or archived value is never recalled as current. Restore
is an explicit user command that creates a new current version and supersedes
whatever held the identity; it refuses redacted sensitive history. Credential-
bearing records are redacted at every retention boundary (forget, expiry,
supersede), so history and restore cannot resurrect a secret.

`leveler memory audit-identity` reports what historical records can establish:
ready, needs confirmation, possible duplicate under another key, stale residue,
or sensitive. It is a read-only dry run and never rewrites user memory.

Recall, the catalog, standing preferences, the model's memory tool and the
status surface all read one effective-active view: lifecycle status, expiry,
and the terminal entry per identity. Chat, Goal, Resume and Headless therefore
see the same current value. Memory bodies stay out of the stable system prefix;
catalog entries are titles for discovery. A disabled project exposes no memory
surface and admits no extraction.

Agent experience is the same durable memory with an evidence contract, not a
second store. After a completed task, the runtime reads only its own structured
facts (commands, exit codes, model steps, mutations, structured observations —
a project verification script that exited 0 without output, an edit that
reported success but changed nothing, and at most two bounded key/value lines
printed by a verification command, such as `engine: legacy`,
`execution: skipped_by_engine`, or a receipt's `result=passed tests=1` — and the
terminal outcome) and builds a bounded evidence pack. Raw tool logs never enter
the pack. A model may propose a lesson, but the runtime derives verification: a
`verified` experience requires an observed failure and, for a fix or root cause,
a later success of the same command family after the last change. One project
verification script is one family across engine flags; its `status`/`history`
subcommands are informational and never resolve a failure. A cited failure with
exactly one same-family later pass records that pass as evidence even when the
model omitted its id. A changed verification entry plus printed status lines is
also a signal, because a task can repair the official entry without any command
failing. An environment constraint may be
verified by a structured observation alone. A different explanation under the
same failure signature is not an update: it waits for a person instead of
silently replacing the current entry. An observation-only lesson takes its
identity from the observed script or edit tool, so repeated silent checks
update one lesson rather than stacking duplicates. Its explanation is model
inference beyond that observed fact, so a later runtime-observed claim under
the same identity waits for a person instead of silently replacing it; a
failure-backed experience still updates on new evidence. A candidate that
cannot meet that contract stays unverified and waits for a person; it is never
current truth. Experience has its
own durable work queue, admitted with the turn and claimed only after the turn
is terminal. A staged result is replayed after a crash instead of asking the
model again.

Experience is never a standing preference, never occupies the per-request
catalog, and cannot replace a fact of another kind without a person. Recall is
relevance-based; expiry, supersession, sensitivity and the terminal entry per
identity filter it. User preferences and the current code still win. A passing
check proves the behavior changed, not the explanation, so a recalled line
marks its explanation as a model reading, carries the conditions under which
the lesson applies, and carries the date it was observed. `invalid_if` is
advisory: no service re-runs the environment, so the reader judges whether a
change invalidated the lesson.

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

### 9.0 Permission Mode Product Contract

`FullAccess` (Full) selects unrestricted execution before admission policy, bypassing CodeLeveler permission rules, approval, sandbox, resource scope, credential filtering and task ownership restrictions. Files, HOME, credentials, arbitrary Git remotes, loopback/LAN/Internet, process control and implemented future capabilities produce no permission ASK/DENY. Unavailable capabilities, invalid tool inputs, OS/program/external-service errors remain real failures. Cancellation, deadlines, durability and resource cleanup remain runtime responsibilities.

`Assisted` (Auto) allows ordinary reads/writes, builds, tests, localhost, owned-task operations, and every Git operation (`commit`, `fetch`, `pull`, `push`, `rebase`, `reset`, `clean`, `checkout --`, `restore`). Auto asks only at three boundaries: deletion (`rm` / `rmdir` of an absolute path, `..`, `~`, a variable, or a glob, plus `dd` / `mkfs` / shutdown; a literal relative path inside the working directory runs directly; not `git clean` or `git reset`), privilege (`sudo` / `su` / `doas`, opening a host application, MCP, cross-session task control, agent / skill / memory definitions), and security (credential plaintext, a Git remote whose destination has a stored credential, writes outside the workspace, and Git configuration or arguments that execute a local program, such as `core.hooksPath`, `core.fsmonitor`, `alias.*`, `--upload-pack`, `--exec-path`, `GIT_SSH_COMMAND`). A transport whose remote cannot be named (`git fetch $REMOTE`, `git fetch --all`) cannot bind a credential and still asks. Ordinary system diagnosis (`ps`, `top`, `sysctl` reads) and ordinary temporary-file I/O (workspace, `$TMPDIR`, system `/tmp`) are ordinary development actions. A stored credential still asks at admission; a public remote gets no credential binding and no prompt. Full and explicitly approved execution authority is frozen for that exact call in `ResolvedExecutionPolicy`, consumed by ProcessRequest, filesystem, HTTP, MCP and task control. The same approved action cannot be rejected by another permission layer. One-call approval never changes the session profile or elevates later unapproved calls.

**The sandbox must not be stricter than the permission policy.** Where the permission/risk layer decides ALLOW, the execution stage's OS sandbox must not answer the same capability with a silent `EPERM`. The macOS seatbelt profile therefore has to carry the capabilities Auto already permits: the sysctl name→OID translation read (`sysctlnametomib`, without which even a listed `kern.argmax` is denied), a shared temporary write root outside the workspace, and a `process-exec (with no-sandbox)` exception for the two setuid process observers `ps` and `top`. The last one is an OS constraint, not an implementation choice: the kernel forbids an already-sandboxed process from executing a setuid/setgid image and no SBPL rule can relax it (`(allow default)` fails identically), so these two read-only observers can only run outside the sandbox — granted by literal path for exactly those two images, while every other setuid program (`sudo`, `crontab`, `su`, `login`, `traceroute`) still fails closed. Where a capability genuinely cannot enter the sandbox, the permission layer must ASK or refuse with a structured reason instead of letting the caller see a bare `Operation not permitted`.

`RequestApproval` (Restricted) uses scoped resources. NetworkScope/NetworkResource describe destinations and resolved addresses; the HTTP broker disables environment proxies, pins addresses and revalidates every redirect. Full and explicitly approved calls use unrestricted HTTP execution. Classification alone is not enforcement evidence.

Current process support: macOS None denies networking; Loopback is **PARTIAL**, allowing only canonical `127.0.0.1`/`::1` outbound, denying every listener including loopback, without full `127/8` support. Actual probes proved seatbelt localhost inbound predicates allow wildcard listeners to receive LAN traffic; they are not emitted. Linux None uses bwrap network namespaces; host Loopback/LAN/ConfiguredRemote are explicitly Unsupported. Windows restricted network scopes are explicitly Unsupported. macOS LAN/ConfiguredRemote are also Unsupported. Internet and Full retain open networking. ConfiguredRemote identity records do not yet have an authoritative Git remote resolver/socket broker consumer and are not a completed scope.

Browser/MCP process destination isolation remains unavailable; tool operations reject partial network scopes and require Internet or unrestricted authority. Autonomous networking from existing browser/MCP processes remains an open lifecycle boundary. The host controls MCP inherited-credential launch authority. Model providers, fixed local RuntimeHost/ExecutionHost transports, configured relay and updates have independent infrastructure purposes rather than arbitrary tool network authority.

Phase 3 Resource Grants and Phase 4 Credential Brokers must preserve this invariant: they may constrain Auto/Restricted, never add permission interception to Full or to the same explicitly approved action.

**Permission Architecture Mainline: CLOSED.** The Permission Mode contract, NetworkScope, Resource Grants, the Credential Broker, `ApprovedGitTarget`, filesystem object binding and the Full bypass invariant are implemented and validated against real execution. What remains — and does not block the mainline — is Restricted network hardening (loopback-only listen, `ConfiguredRemote` address pinning/redirects, Linux host-loopback), Windows/Linux real-host qualification, optional deeper SSH brokering, and non-blocking release advisories.

#### Phase 3: Reusable Resource Authorization

**Current gate: CLOSED.** Storage and resource resolution exist. Filesystem write and delete commit through an object-bound transaction: the publish is a single `renameat2`/`renameatx_np` `RENAME_EXCHANGE`/`RENAME_SWAP` (delete moves the entry aside once), and the commit is then decided from the object that was actually displaced rather than from a name re-read before the syscall. A substitution therefore cannot be silently committed; the displaced object is restored and the commit fails closed, and no object that was not staged and verified is ever unlinked. Adversarial race tests cover replace, delete, symlink and parent-directory substitution. Grant-eligible Git remote commands (`fetch`, `ls-remote`, `push`) execute a frozen target: the admitted `ConfiguredRemote` binding (repository identity, remote name, canonical URL, transport, capability) is recovered as an `ApprovedGitTarget`; the execution layer reads the verified git directory's config exactly once, verifies it (no effect-critical key, resolved remote URL equal to the approved one), neutralizes any repository credential helper, then runs Git against a private mirror git directory: frozen config, empty global/system config, hooks disabled, the remote URL pinned explicitly, and every `GIT_*` override cleared. Git re-reading `.git/config`, the global/system config or the environment after that cannot change the approved target; a target that cannot be reproduced from frozen bytes fails closed. A real HTTP Git probe shows the server only ever sees the approved URL — or no request — while the remote URL, pushurl, `insteadOf`, credential helpers, SSH override or repository identity are mutated after approval. Production admission now accepts the filesystem, repository and remote bindings the execution layer can bind (unix) and rejects the combinations it cannot. Windows file and Git reuse remain UNSUPPORTED / fail-closed. The types and persistence semantics below describe the framework.

`ToolHost` owns execution authorization: it builds `GrantRequest` from actual typed tool inputs and host resource resolvers, freezes exact capability/resource bindings, and enforces that identity where the real side effect happens (for consumers with a proven execution binding), so the approved resource and the effected resource are the same. Clients display that request and return a decision; descriptive text, model-supplied identity claims and UI state cannot create authority. `ResourceGrantStore` validates, matches and persists records through the existing production database. Persistence follows the approval event's durable barrier. Resolver or store failures require fresh consent rather than default allowance.

Once is transient for the current call and is never persisted. Session binds the durable session's actual `SessionId`, rather than a turn id, agent id or display name; child executors retain their owning session. Project binds the canonical checkout and Git common directory's object incarnation; non-Git projects bind their canonical directory object. Filesystem grants bind an exact canonical path and object identity (the existing parent for a new target), never a directory prefix. Background tasks bind the runtime incarnation, task number, immutable owner and process identity, preventing replay against the same task number after restart. Changed objects, configuration or remote identities require fresh authorization.

Capability and resource match separately: remote reads/mutations, filesystem reads/writes/deletes, task observation/control, credential use and raw credential reads/writes do not imply each other. Resource grants create neither unrestricted authority nor broader `NetworkScope`. Full branches to unrestricted execution before resolution or store lookup. Ordinary allowed Auto actions do not query the store; only actions requiring consent attempt exact reuse. Legacy session/always rules retain their existing semantics without migration into resource grants.

Unsupported attribution offers no reusable resource grant and retains existing exact-call approval: SSH remotes, unattributed credential helpers, a remote that cannot be named, alternate PATH, and Git environment or command configuration overrides. A resolvable `sh -c "git …"` auto-runs under Auto like the direct command and resolves the same remote grant, so a stored credential for that destination still asks at admission. The shell is not a frozen Git target; the approved script runs with the host helper. External PIDs lack trusted process birth proof and therefore receive no persistent grant. The Phase 3 `CredentialUse`/raw capability types are implemented by the Phase 4 broker below. Pinning a `ConfiguredRemote` at the network layer remains Phase 2: the frozen Git execution target proves the actual network destination is the approved URL, and does not claim redirects are fully isolated at the network layer.

#### Phase 4: Credential Isolation / Broker

**Current gate: CLOSED.** Using a credential and reading its value are separate capabilities over the same Phase 3 grant store, identity and scope. A host-side broker — never the agent shell — resolves the destination's credential by running the user's own global Git credential helper in a scratch directory with every caller-supplied `GIT_*`/askpass override filtered out, so neither a repository-local helper nor the caller environment can become the source. The value never becomes model input, a tool result, a log line or a command line: it is written to an owner-only (0600) file inside the frozen Git target's private directory, and a generated helper prints it only to the approved Git process. A grant binds the credential's one-way incarnation commitment (`sha256` over username and value); rotating the token changes the commitment, so a reused Session/Project grant fails closed instead of authenticating as the new secret.

`credential.use` is orthogonal to `NetworkScope` and to filesystem reads: an approved credential grants neither egress nor `~/.git-credentials`/`~/.ssh` access, and a raw credential path (`read_file`, `cat .git-credentials`, `security find-generic-password -w`, `secret-tool lookup`, `pass show`, `git credential fill`) requires its own decision. In Auto and Restricted a Git remote whose destination has a stored credential escalates to a real decision even though `git fetch` is otherwise read-only; a destination with no stored credential (a public repository) gets no credential binding and no prompt. Full bypasses the broker entirely: no ASK, no DENY, no injection.

Reusable credential consent is offered only because the frozen Git target re-resolves the credential at execution and refuses when its incarnation changed — the same execution-bound rule as the remote URL. SSH agent-mediated authentication (`SSH_AUTH_SOCK`) remains **PARTIAL**: the agent is used without a broker and private key files stay denied. Brokering the SSH identity itself, and Windows/Linux real-host credential qualification, remain open. A detached (background) authenticated Git command is fail-closed UNSUPPORTED: a detached process cannot reap its private isolation directory at task settlement, so the runtime refuses rather than leave the credential file on disk for the operating system's temp cleanup. Such an effect must run in the foreground, where the guard reaps the directory and the credential file when the process exits.

### 9.1 Why There Must Be One Execution Authority

If file mutation follows one authority, commands another, and child agents a third, there is no unified safety boundary.

For controlled real side effects:

> **Requests may come from many places. Final execution authority must remain singular.**

### 9.2 Data Egress and Privacy Boundary

Network access is a real host side effect, so data egress follows the same Host Authority and least-necessary-data principles.

By default, CodeLeveler itself does not collect or report product telemetry or user work data to the CodeLeveler project or a central service, including:

```text
source code and file contents
conversations and prompts
model output
commands and command output
environment information
usage analytics
crash context
```

This does not mean that CodeLeveler never uses the network. Explicitly enabled external capabilities—such as a model Provider, MCP, web search, remote execution, or update checks—may make the network requests required to perform that capability.

Such egress must be:

```text
triggered by an explicit feature
explainable in destination and purpose
limited to data necessary for that capability
not broadened through hidden fallback
never used to send credentials or secrets as telemetry
```

If diagnostics, crash reporting, or usage analytics are added in the future, they must be explicit opt-in, disableable, and decoupled from core Agent execution. Using CodeLeveler must never implicitly grant permission to upload user work data.

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

This prevents a green check, successful Tool call, or changed file from becoming an automatic “done” signal. The model cannot shrink the user's scope by shrinking its own acceptance list: explicitly enumerated deliverables are accounted for one by one, and a missing, not-done, or mechanically contradicted requirement refuses completion.

### 10.4 Final Prose Is Not a Task Terminal

The model producing its last assistant text proves only that the turn is no longer waiting for model generation. It does not prove that the task is complete.

Closeout crosses one runtime lifecycle boundary:

```text
Running
   ↓ final assistant text ends
Finalizing
   ↓ dependency settlement, required review, outcome resolution
Terminal (TaskFinished)
```

`Finalizing` is a domain-neutral Runtime state. The Engine records its phases and timing. Durable phase starts, the next phase start, and the `TaskFinished` timestamp are sufficient to calculate each interval mechanically; pure phase-completion timing must not add persistence to the authoritative terminal critical path or alter the result.

`TaskFinished` is the only authoritative task terminal. Clients project completed, failed, or blocked results only from a persisted `TaskFinished`; final prose, turn completion, and background cleanup cannot substitute for it. If the terminal transaction did not commit, a client may surface only a recoverable error, never synthesize `Failed` or any other terminal.

Declarations, verification evidence, and lifecycle describe separate facts. File mutations do not promote `Answered` to `Completed`; `Completed` does not establish that formatting, builds, or tests passed. Shipping descriptions report observed verification only, or unknown when no evidence is attached. `Stalled` retains its stop reason and maps to resumable `Interrupted`, leaving outstanding goals unsettled. `/develop` requires its own Review PASS and otherwise stops at a resumable boundary; this workflow contract does not make ordinary advisory reviews blocking.

Once the authoritative result is committed, the Product must publish the user-visible terminal immediately. Only work already detached to an immutable task/run identity may continue afterward; it must not delay terminal visibility or move a client back into a running state. Session-scoped cleanup must snapshot its exact resource ids before publication. A continuation checkpoint belongs to the authoritative window boundary and is committed before `TaskFinished`, because letting it re-read “current session” afterward could absorb the next turn; failure to create that checkpoint preserves the concrete error and pauses the window as recoverable `Interrupted`, retaining the owed Goal. It must not claim a successful checkpoint; a later resume rebuilds from canonical facts. A review configured as required may still produce completion warnings. Findings remain advisory model conclusions, not mechanical terminal verdicts; an advisory review must not block.

A background process chooses its cleanup boundary explicitly when it starts. The default `goal` lifetime is reaped when its creating goal reaches a terminal state. `runtime` skips goal-terminal cleanup but is still reaped when the current Runtime exits, preserving its existing contract. `session` and `persistent` processes are owned by the independent Execution Host from spawn, while retaining their creating session as access owner. `session` tasks stop on explicit session deletion. `persistent` is for explicitly requested services or watchers that must survive Runtime restarts; it ends on process exit or explicit stop. Execution Host failure is a separate failure domain and does not guarantee service termination. Runtime updates do not stop hosted services or automatically convert existing local tasks. All lifetimes remain observable and stoppable through the same background-task interface.

Write ownership becomes an operating-system execution boundary before dispatch, including allowed paths and paths exclusively owned by other executors. Unsupported enforcement fails closed instead of granting workspace-wide writes. Mutation settlement observes only the enforced scope; whole-tree diffs, filtering other executors' changes, and whole-tree rollback are not permission enforcement. A background workload ends in this order: the entire workload exits, mutations settle, then its terminal state is published. Parent-shell exit is insufficient. In Auto/Restricted, log access, waiting and stopping require the creating session; cross-session operations request approval. Full/approved calls use explicit authenticated host unrestricted authority without impersonating the creator.

Live stdout/stderr channels and line buffers have capacity limits. Pipes remain drained when a consumer falls behind, and dropped bytes explicitly mark output as truncated. Tool results retain exit codes, errors, and sandbox facts; they do not interpret connection refusal as a non-code problem or prescribe escalation or another tool call.

Incremental background-task observation belongs to the process capability. `observe` validates reader positions, registers change notifications and delivers bounded output under the same task lock. New output, a non-running status, or the bounded wait interval can return control. Explicit cursors are independent and do not advance the default reader; truncated history reports a gap and undelivered bytes remain available. `wait_task` only adapts arguments and model-facing output; `get_task` reads the full retained log. Its model-facing contract is a terminal wait: it blocks for a status change, the caller's timeout or cancellation, then delivers the accumulated delta in one result, so observing a long task costs one model round rather than one per interval. Output arrival never satisfies completion-only `wait` or causes early settlement.

Terminal settlement facts remain immutable for the lifetime of the task record: reading must not consume a permission violation or make another reader see success. Mutation paths and snapshots may be reported once, but the delivery marker does not change the settlement facts.

### 10.5 Tests, Builds, and Linters Are Ordinary Tools

The Runtime does not generate or execute a verification plan after task completion, and it does not map test, build, or lint results into a second terminal state. When the user explicitly asks, the model may still run these operations through ordinary command tools. Their output and exit code are recorded as ordinary tool facts; they do not create a `Passed / Failed / NotRun / Unavailable` session status, and clients do not render a verification status.

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

### Optimistic session settings

Replacement commands for the session title, model, default model, permission,
thinking level, and collaboration carry the snapshot `last_sequence` actually
observed by the caller (0 for an empty log). Rust `SNAPSHOT_VERSIONED_COMMANDS`
is the sole wire-tag policy; the schema exports it for generated Web/Desktop
consumers, while TUI calls the Rust policy directly. One SQLite writer
transaction rejects negative or future observed log positions and any
`SessionMetadataChanged` after the observed position, replaces only the selected
field, and appends `SessionMetadataChanged`. Answer progress and other events
that do not change settings may advance the log without causing a conflict.
Metadata and its version are read in one
snapshot transaction. A conflict changes neither metadata nor the log; clients
refresh and show the error without automatically retrying an overwrite.
Completed identical command identities retain receipt-based idempotency.
LocalSocket/TCP use the distinct `DeliverVersionedSettings` operation (wire tag
`deliver_versioned_settings`); WebWS
uses `deliver_versioned_setting`. An old peer rejects the unknown operation,
and clients never fall back to legacy delivery. A new peer refuses protected
settings through legacy delivery, and the new operation only accepts classified
replacement commands with an explicit observed version.

Submit/Steer/Cancel and request-id waiter replies retain their own queue,
cancellation, or identity contracts. Internal RunGoal/ConfirmPlanToGoal axis
changes update only collaboration and advance the log. SetDefaultModel CAS
protects the current session model; GlobalConfig owns the global default file
and does not provide cross-session CAS. If that file write fails after the
session change, report the partial result and retain the unresolved dispatch
receipt rather than blindly replaying it.

### 11.3 Durable Turn Admission

A fresh user or chat turn may become durably `running` only when the same turn
row already contains a versioned, replayable initiating user message. That
write-ahead payload is the canonical recovery input; `session_messages` is the
ordered transcript projection used by clients and future model requests.

```text
durable running turn
        ⇒
durable replayable initiating input
```

The wire ACK is emitted only after this boundary commits. If the
process dies before the normal transcript append, restart recovery projects the
initiating message exactly once under the current ownership token, using the
turn id rather than message content as identity, and only then settles the
orphan turn. `UserMessageAdded` is an optimistic client notification; it is
neither the canonical input, a persistence command, nor a durability witness.

Every interactive client holds the same ACK contract: a command that starts or
continues a turn returns success only after the corresponding durable admission
boundary commits. The in-process TUI, the daemon `leveler serve` exposes, the
`/web` UI that process serves, and a connected WebUI all wait on it; Embedded
and Daemon no longer have different turn-ACK contracts. A success ACK means the
execution input was durably admitted (`EngineEvent::TurnStarted` is emitted
after `TurnStore::start_owned` commits) — not that the turn finished, and not
that tool side effects are exactly-once. A refused turn returns an explicit
error, never a success ACK. No response or a dropped connection cannot be read
as "the command did not run": receipts keep their existing dedup semantics and
an unknown outcome never authorizes an automatic rerun.

For an attachment-free `SubmitMessage`, the App command entry recognizes continuation with the existing `parse_continuation` and invokes the existing `ResumeTask` handler. It preserves the original task, Goal and plan, treating amendments as additions to the original objective. The existing Runtime owner still decides resumability, rejects explicitly cancelled tasks, and treats continuation without resumable work as an ordinary message. Submissions with attachments retain the content and image validation path; clients do not add separate continuation parsers.

Chat delegation is a turn-scoped Goal identity, not a fourth collaboration axis. A top-level authoring Chat turn may call the injected `commit_task(objective, acceptance)` tool; the runtime validates the declaration and binds that turn through the existing `GoalBinder` (goal row, `turn.payload.goal_id`, budget-scope migration). The durable goal objective is the user's own request in the turn's objective anchor, not the model's restatement, so the model cannot narrow the task by declaring a smaller objective. From the next round the same turn exposes `update_goal`, Goal closeout and the verification gate. The session's collaboration remains `chat`, and a later ordinary message remains Chat. The declaration is structured: the runtime never classifies user text for execution intent and adds no classifier model call. Questions, discussions, read-only and Plan surfaces never receive the tool; sub-agents cannot declare tasks. Resume treats `turn.payload.goal_id` as sufficient Goal identity, so an interrupted delegated Chat turn resumes under the Goal profile without changing the session axis. A paused task remains visible until it settles or is explicitly cancelled; an explicit logical cancel settles its goal and refuses a later continuation.

The user's explicitly enumerated deliverables are the scope authority. The persisted goal records the model's candidate acceptance checks and every later user-authored scope instruction; `update_goal(complete)` must account for each extracted requirement and refuses completion when one is missing, declared not done, or contradicted by a checked artifact or command. Extraction is deliberately narrow: file paths, inline-code tokens, short delimiter-separated lists, numbered items, named executable commands, and an explicit demand that tests pass. Open-ended semantic goals produce no mechanical requirements and are completed only with an honest unverified boundary, never a fabricated proof. A user-authored waiver is the only way a requirement leaves the effective set.

Web restores a saved conversation using the newly received `SessionList`, rather than a React state list whose dispatch may not yet have committed. Existing current / pending selection guards prevent restoration from overriding a conversation the user is already opening.

#### Cancellation ACK: requested ≠ delivered ≠ terminated

A command ACK answers delivery, not execution. Cancellation makes that a
load-bearing distinction, with separable stages:

```text
CancelRequested → CancelDelivered → ExecutionTerminated → TerminalPersisted
```

These are semantic stages, not wire types. A successful `CancelCurrentTurn`
ACK means the request reached the runtime that owns the turn; the turn stays
`running` until its terminal commits. Clients learn the real end from the
durable terminal (`TaskFinished` plus the matching `RuntimeEvent`), never from
the ACK, and render "stopping" rather than "stopped" while the turn is live.

`CancelTask` differs in intent, not in ACK strength: it asks for a terminal
`cancelled` outcome that settles the goal and refuses a later continuation,
whereas `CancelCurrentTurn` leaves a resumable `interrupted`. A running task's
cancellation is committed to the existing event log before ACK, bound to its
exact task, turn, boot and owner epoch. Cancellation and terminal commits share
the same durable writer boundary: a cancellation that wins requires `cancelled`,
while a completed terminal cannot be overwritten by a late cancel. After a boot
dies, the runtime reaps only turns proven dead and the Harness supplies goal
settlement from that turn's durable intent. The terminal, session projection and
ownership release commit atomically. Recovery does not replay tools with unknown
results, and ACK must never be presented as execution terminated. When a crashed boot leaves only the latest fenced recovery Interrupted turn without a TaskFinished for that work window, App determines eligibility from the same durable SessionFacts turn ordinals and terminal boundary, requiring a valid user/chat initiating payload and released Task ownership. A late finish on the same turn cannot reopen Cancelled/Completed. Harness retains original objective, Goal, plan, and ledger reconstruction. Mutating tools with unknown outcomes require explicit human reconciliation; recovery cannot automatically replay them or fabricate success.

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

### 12.3 Declared Facts vs Harness Policy

A model profile declares FACTS: the context window, the completion the model can produce, the quality boundary past which long-context recall is expected to degrade, the protocol it speaks, and that protocol's compatibility quirks.

It does not declare how much of that capacity the harness spends. The completion reservation, the safety headroom, the pressure threshold at which old history is folded, and the recent-tail retention budget are HARNESS POLICY, resolved per seat from those facts. Keeping them out of the profile is what lets two seats spend one model's window differently without inventing a second model.

Replay obeys the same split. A route's reasoning-replay contract — when captured reasoning is carried back, and what an assistant turn that captured none carries — is resolved from the protocol and its compatibility facts, never from a model or provider name.

A route fact can be imposed by the model family's wire protocol even when the user never wrote it down. Resolution has one order: the provider/model protocol capability, then the user's explicit declaration, then the conservative default (`never`). An explicit declaration may strengthen a documented default and may opt out of a family default a deployment knows it disabled; it may not weaken a provider's hard requirement. Weakening a hard requirement (DeepSeek with tools) is a startup error naming the model and field, not a request the provider rejects with HTTP 400. `leveler login`, a hand-written config, the lab, dogfood and every client resolve through the same function.

Only families whose published protocol requires historical reasoning are listed: DeepSeek thinking mode (a tool-bearing request must carry the full `reasoning_content` of every historical assistant turn, including turns that called no tool; an empty string is the measured form for a turn that captured none), Kimi K3 and its point releases (the complete assistant message must come back, `reasoning_content` included), Kimi's K2.x thinking models while thinking is on, and GLM interleaved/preserved thinking. A family not listed carries nothing — the harness never sends a field a route did not ask for. On this wire the trigger is the request exposing `tools`, never a historical turn's `tool_calls` count. Replay is not squeezed into one boolean: "the channel must exist", "its content must be complete", "an empty value is legal", and "thinking is preserved across turns" are separate facts, and a route that cannot stop thinking does not advertise `off`.

### 12.4 Replayed Reasoning Has an Owner

`reasoning_content` carries no signature, so a replay is only meaningful as the same route's continued thought. Every assistant turn records the route that produced it — provider alias, wire model id, protocol, and the deployment when the provider layer knows it — so a resumed session can tell its own history from another model's. The comparison is per turn, not per session: after A → B → A, A's earlier turns are A's own reasoning again, while B's stay foreign.

A provider alias identifies exactly one deployment (one `base_url`), so the same wire model id behind two gateways is two routes. What a handing-off request keeps is everything the task depends on — the user's objective, the assistant's prose, the tool calls and the tool results. What it drops is the foreign private reasoning; where the protocol validates the channel's presence, it carries the legal empty value rather than presenting foreign content as native. Nothing is fabricated, and nothing is folded into the assistant text, because a model must not read another model's thinking as its own visible reply.

Missing, empty, incomplete and unknown are distinct facts. An assistant turn this route completed that produced no reasoning is *empty*; a payload with no recorded route is *unknown*; an interrupted stream never becomes a message at all, so partial reasoning can never be replayed as complete. `leveler login` writes model facts only: a generated config does not need the replay fields, and an advanced override remains accepted for a deployment that differs from its family's published requirement.

Preserved thinking is a route declaration, not a harness preference: where the wire spells it (`thinking: {type: "enabled", clear_thinking: false}`), the request carries it; where it does not, no field is invented. Turning thinking off is offered only on a route that can honour it.

Anthropic thinking is selected by declared `reasoning.style`: `adaptive_thinking` or `budgeted_thinking` with explicit `budget_tokens`. The adapter validates the budget against the output cap and preserves signed and redacted thinking blocks in their original order. It does not fabricate a signature or replay unsigned text as signed thinking. Cache reads and writes are normalized into total input usage; cache-write cost remains unknown when the pricing profile cannot express its tariff.

The provider-visible request is then produced by ONE projection owner: the semantic conversation goes in, and the wire encoder and the context accounting both read the same result. An estimate standing in for a missing provider usage figure is an estimate of that projection, so "what was sent" and "what was counted" cannot diverge.

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

Interactive creation uses `create_session_identified` (Web: `/api/sessions/identified`;
Desktop: an explicit identified IPC operation). Clients retain one `request_id` across
unknown-response retries. IDs share the global CommandId namespace: another original
request or an ordinary command using the same ID causes an explicit conflict.
TaskCreationStore atomically writes the session, task, original-request fingerprint,
and existing command receipt. The session row durably contains its initial model,
workspace, permission, sandbox, and collaboration. Replay returns the original session's
current snapshot without rewriting later settings; changed defaults do not change the
caller request identity. Explicit session deletion also deletes its receipt, so dedup
retention follows the durable session lifetime. ApprovalPolicy retains its existing
runtime and transport trust owner; the creation receipt adds no durable permission truth.
Legacy requests without request_id remain non-idempotent and cannot be automatically
retried after an unknown response. Old peers must reject the distinct identified operation;
clients never fall back to legacy creation or infer support from ignored fields.

The client and Runtime do not need to live on the same machine.

### 13.1 What is shipped today

These products exist in this repository:

| Product | Entry |
| --- | --- |
| CLI | `leveler` |
| TUI | `leveler tui` |
| Web | `leveler web` |
| Mobile | `apps/leveler-mobile` |
| Remote host bridge | `leveler remote` and `services/leveler-relay` |

A standalone desktop app and cloud workers are not shipped. Custom agents: [`AGENT_EXTENSIBILITY.md`](AGENT_EXTENSIBILITY.md).

### 13.2 Client Protocol and Runtime Host

`leveler-client-protocol` defines the single **semantic contract** between
clients and the Runtime: `ClientCommand`, `RuntimeEvent`, `UiSessionSnapshot`,
and their envelopes. CLI, TUI, Web, and future Desktop clients use this
contract when connecting to a Runtime, instead of defining shell-specific
session, turn, or tool events. The protocol does not decide how a process starts
or which local connection carries it.

Runtime Host is a separate boundary. It makes a connectable Runtime available
by discovering, starting, adopting, or reviving it. It orchestrates build and
configuration compatibility, retirement and handoff, daemon authentication
tokens and the readiness contract, transport binding, and idle policy. It does
not run the Agent loop or own models, tools, permission decisions, session
persistence, or recovery.
**Runtime Host** orchestrates processes and connections; it is distinct from
the **Host Authority** in Section 9, which controls real side effects.

Long-lived background processes use an independent **Execution Host** within
the `leveler-execution` boundary. It is distinct from Runtime Host. From spawn,
Execution Host alone owns hosted child handles, process trees, sandbox
resources, output readers, and exit monitoring. CodeLeveler Runtime is its
client; Runtime updates, exits, and crashes do not transfer process ownership.
Execution Host reuses `CommandRunner` and the background registry rather than
introducing another spawn, log, or stop implementation. Models, the Agent loop,
session lifecycle, Goals, verification, and file-change settlement remain in
CodeLeveler Runtime.

OS workload exit and semantic settlement are separate facts. Before spawn,
Runtime durably records the creating session, writer, write scope, and mutation
baseline. Execution Host does not interpret settlement inputs; it records
process facts. Once the complete process group exits, Host records the exit
durably. Runtime reconciles immediately while online or after reconnecting.
The existing semantic terminal cannot be published, and write ownership cannot
be released, before settlement commits. Concurrent clients cannot commit
independent settlement results; Runtime's durable settlement record is the
single authority for that fact.

The Execution Host process protocol negotiates major/minor compatibility and
capabilities, rather than requiring the Runtime's build fingerprint. An
incompatible client reports the error and leaves services running. Runtime
updates do not replace a working Execution Host. Host upgrades do not promise
live process migration. Legacy tasks already owned by an older Runtime retain
their cleanup and update-blocking behavior; migration must not be fabricated.
When an older Runtime returns `RetireDecision::Unsupported`, clients send no
shutdown command, force-retirement command, or process signal. Reported active
turns or background tasks defer the upgrade. An apparently idle health snapshot
also blocks automatic migration because it cannot atomically close admission.
The user must let the old owner finish its work and exit normally before the
existing Runtime Host connection entry starts the new version. Modern atomic
retirement and Full tool-permission semantics retain their existing behavior:
lacking atomic retirement is a compatibility error, not a permission ASK/DENY.

Execution Host crashes are a separate failure domain. Normal destructor cleanup
does not prove tree cleanup after SIGKILL. Recovery requires durable metadata
and verifiable process identities. When identity cannot be established safely,
report unknown / recovery-required, retain unsettled write ownership, and never
signal a potentially reused PID or process group or claim an unobserved exit.

Phase one does not implement automatic adoption with a provable process birth
identity. Persisted Running/Killing records or unresolved spawn intents prevent
automatic replacement of a lost host and require explicit recovery. A process
possibly exiting on its own cannot resolve this uncertainty by assumption.
Durably confirmed exit facts and logs can be restored by a new Host without
restarting those workloads.

The current Execution Host owner-only state-directory security backend supports
Unix. Windows hosted lifetimes fail closed until that security backend exists;
there is no fallback to unprotected TCP or permissive token files. Existing
Runtime Host Windows support and local `goal` / `runtime` execution are unchanged.

Each boundary has one owner:

| Boundary | Responsibility |
| --- | --- |
| Shell / Product | Arguments, interaction, project selection, and client-owned process lifetime policy |
| `leveler-runtime-host` | Runtime discovery / spawn / adopt / compatibility / handoff / revive; token, readiness, bind, and idle orchestration |
| `leveler-local-transport` | Connections, listeners, transport trust and endpoint ownership; platform adapters hold Unix socket/flock or Windows pipe/instance guard |
| `leveler-client-protocol` | Semantic client commands, runtime events, and snapshots |
| `leveler-app` / `leveler-engine` | Internal runtime state, session and turn lifecycle, persistence, and crash recovery |

Host orchestrates transport binding and ownership acquisition; it does not
reimplement the socket, `flock`, or internal runtime ownership and recovery.
Once connected, commands and events flow through the protocol and transport;
business commands do not pass through Host. “One Runtime” means one shared
runtime implementation, not one OS daemon process for every project or an
obligation to use a daemon in embedded mode.

Before Phase 1, the CLI and Web each implemented a startup path: the default
TUI path orchestrated a detached per-repository daemon, while Web's
`ProjectManager` probed or spawned project daemons and owned the children it
started. Both paths now call `leveler-runtime-host`: TUI uses its detached
discovery, version handoff, and revival entry point; Web uses its owned-child
adopt-or-start entry point. CLI retains terminal input and text. Web retains
its project registry, routing, status, and spawned-child monitoring and
termination policy. Both process ownership behaviors retain their existing
semantics. The Web page access token remains a Web
Product concern, distinct from the daemon authentication token. Embedded mode
continues to use the same `leveler-app` Runtime implementation without
requiring another daemon.

Below the client protocol, transports share framing, request handling and
reconnection:

```text
Client Protocol
    └── Transport
        ├── Unix socket: trusted-local
        ├── Windows named pipe: trusted-local
        └── TCP + bearer token: authenticated-remote
```

The server transport establishes trust. Declared ClientKind can only reduce
privilege. Loopback TCP with a valid token cannot grant local AutoApprove or
count as a local interactive waiter. PermissionProfile retains its existing
contract; the transport gate restricts client-granted AutoApprove without
redesigning execution permissions. The daemon owner's global AutoApprove
configuration retains its existing behavior.

Unix retains an owner-only socket and server lifetime flock. The Windows
adapter rejects remote named-pipe clients, installs a protected current-user
SID DACL, and checks the actual pipe peer process TokenUser SID in both
directions. The client also checks the connected pipe handle's owner SID,
binding identity directly to the security object. `leveler-win-local-ipc`
encapsulates the Win32 security operations.
Like Unix 0600, this is an OS user boundary, not application-signature
authentication. Same-user processes and administrators are within that
boundary; a different service account is not automatically trusted.

Windows pipe names derive from the current SID and a hash of Layout's logical
socket path, encoded as UTF-16 rather than embedding Unicode repository paths.
The readiness contract retains `socket` as a logical endpoint; build/config
identity remains in runtime_info. Unix flock and the Windows retained first
pipe instance handle are each the platform's single lifetime ownership
authority, acquired before recovery and ready publication. UI disconnect
does not release ownership; the OS releases it after runtime crash. Windows
pipes need no filesystem stale-socket cleanup.

RuntimeHost shares start/adopt/revive/handoff across platforms. Only transport,
OS security primitives and detached-process configuration differ. Security
failure is explicit, with no trusted-TCP fallback. Implementation and cross
compilation do not establish Windows real-host acceptance; see
[`DESKTOP_PHASE2_WINDOWS_TRANSPORT.md`](../DESKTOP_PHASE2_WINDOWS_TRANSPORT.md).
The Phase 3C Electron skeleton lives in `apps/leveler-desktop`. Its Renderer
reaches Main through restricted preload IPC; Main starts only the thin Rust
`leveler desktop-bridge` adapter. The adapter reuses Runtime Host's detached
attachment, cross-source connection and revival, and the existing client
protocol, without duplicating process orchestration or business state.
Closing Desktop disconnects the bridge without cancelling turns or stopping
Runtime; Runtime retains its existing idle reclamation policy. Desktop Main and the Rust bridge accept the existing `CancelTask` / `ResumeTask` commands bound to the selected session, with bounded continuation content. App / Harness owners retain cancellation and recovery lifecycle authority.

Desktop interactive terminals reuse the same Execution Host,
BackgroundTaskRegistry, and CommandRunner. The host execution authority owns
the PTY process, input, dimensions, raw output, and exit facts. Application
validates session ownership and freezes the actual execution policy at creation;
Desktop only forwards bounded input, dimensions, and output cursors. Electron
does not launch another shell execution path. Terminals are independent of Agent
turns and their input does not enter the model conversation. Full remains
unrestricted; other modes retain enforceable host boundaries. Opening a terminal
does not grant unrestricted authority. Missing PTY capabilities, workspaces, or
platform support fail explicitly.

Creation uses a stable identity so reconnect cannot launch a duplicate. Input
with an unknown outcome is never automatically resent. Switching sessions or
hiding a pane only detaches presentation; recovery observes the same terminal.
Output uses host raw-byte cursors and explicitly marks retention gaps. Ctrl+C is
written to the PTY so the terminal driver signals its foreground job. Closing a
tab stops its process tree and confirms exit before releasing the tab. Normal
Desktop exit closes terminals created by that client; disconnection alone is
not an exit fact. An older host lacking required capabilities must not trigger
a silent fallback to a local process.

### 13.2.1 Optional Workspace and Global History Contracts

Session, TaskEngine, and Runtime retain one implementation. A Session's durable `repository` is its optional primary workspace: existing projects remain `Some(path)` and No Workspace is `None`. Application state always exists; execution working directories depend on capability and authorization. HOME, startup cwd, the previous project, and internal scratch paths cannot substitute for any of these facts.

`Layout::no_workspace` has its own `state/no-workspace` source, socket identity, and app-owned configuration. Project runtimes reject explicit None; No Workspace runtimes reject project associations. Creation distinguishes omitted `RuntimeDefault`, explicit `None`, and `Workspace`. Existing CLI/TUI/Web project requests retain their default project. No Workspace uses the same Runtime without project configuration, rules/hooks, skills, MCP, Git/LSP, file checkpoints, or shell context. Tool dispatch checks actual workspace resources before execution. Tools require a workspace by default and reject direct calls even with FullAccess or absolute HOME paths. Independent web/search/browser capabilities follow the Permission Mode contract in §9.0.

The `leveler-app` Global Task Index scans existing project stores and the No Workspace store through SQLite read-only connections. It projects durable facts without creating databases, migrating sources, starting daemons, or maintaining another task ledger. Source failures accompany partial results. Missing checkouts retain history with workspace availability. Opening a cross-source Session first resolves its actual index owner, then asks Runtime Host to connect to that source; Web's online multi-project Router retains its responsibilities. Global discovery is restricted to trusted local clients; project Remote/Web access does not authorize other sources' history.

`session_projection` derives `task_status` and `task_terminal` from durable turn/event order, boot liveness, and actual live waiters for local lists, snapshots, and the global index. Existing `status` retains Session lifecycle semantics. Only committed `TaskFinished` establishes an authoritative terminal. Answered means an answer; stop=Completed means a domain completion declaration, not independent acceptance. Legacy TaskFinished with outcome=Completed and stop=None retains its terminal compatibility without inventing a declaration. A committed terminal outranks its old live lease during cleanup. A later admitted turn's durable identity/ordinal and event order prevent the old terminal from overriding new activity. Final prose and successful tools without a terminal cannot imply completed. Read-only discovery can establish a running owner but cannot infer waiter liveness from persisted approval records; actionable WaitingUser comes from the connected owner's snapshot.

Reconnect snapshots restore durable prose, plans, terminals, and surviving approval/clarification waiters. Answered, timed-out, cancelled, or restarted interactions cannot become actionable again; dropping a future during cancellation must also remove its waiter. Runtime restart restores durable facts without automatically resuming provider streams; old turns report interrupted/unknown according to owner epoch. Desktop uses the same durable ACK path as every other interactive client: embedded in-process and daemon no longer differ in their turn-ACK contract, and the in-process ACK is durable admission too. Lost ACKs are resolved using the same command identity; unknown outcomes do not authorize automatic reruns.

The local connection entry negotiates `optional_workspace`. Legacy clients entering project sources still receive a JSON string `repository`; only capable new clients may enter No Workspace sources and receive null. JSON schemas and TS DTOs use the existing generation workflow; Mobile Dart uses handwritten JSON-map consumers without a separate generator. Nullable repository migration runs only on the source owner's write connection and preserves old associations, foreign keys, indexes, triggers, and task facts. Global read-only discovery never migrates a source.

### 13.3 TUI projection and performance invariants (Do Not Regress)

The TUI presents Runtime facts; it does not create them. These contracts are
verified by deterministic tests and real dogfood — do not regress them:

- **The footer context bar is presentation only**: use runtime projected input / full model context window for the percentage, denominator, bar length and color. Provider usage is only the no-snapshot fallback; a snapshot with unknown window has no gauge. Show percentage and model-window size without a repeated Context label. The existing `/context` inspector explicitly names input capacity, model window and output reservation; compaction policy and thresholds remain on the input-capacity axis. The inspector derives the grid and breakdown free remainder from its selected axis; the runtime snapshot's `free_tokens` remains model-window free. The eight-cell bar uses info below 70%, warning at 70–89%, and error at 90% or above, with no animation. Narrow rows drop the bar, cache, then capacity; the clock and permission / collaboration controls retain priority. Missing usage is never fabricated; cache retains the existing provider-usage basis.
- **Projection rebuild is decoupled from paint cadence**: a streaming delta only
  accumulates semantic input; the conversation projection rebuilds once per
  painted frame, never once per delta.
- **Bounded highlight cache**: fenced-code syntect results use a bounded LRU
  (512 entries / 4 MiB) keyed by `(lang, exact code)`, independent of the theme;
  the theme only maps colors at render time, eviction is O(1).
- **Complete Markdown code is reachable**: closed and unclosed fences preserve
  all code, without source-line folding or wrapped-row clipping. Long lines wrap
  to the terminal width and the existing Conversation viewport owns scrolling;
  the beginning, middle, end and neighboring messages remain accessible after
  resize and history replay.
- **Finalized-item wrap memo**: immutable transcript items reuse their wrapped
  lines; only the item whose content changed is recomputed. Items that read live
  state (ToolGroup / SubAgent / UserShell) are never memoized. **A height-only
  resize must still invalidate the projection.**
- **Complete Shell output is windowed, not capped higher**: the Conversation
  keeps a bounded preview; the full-output page costs per frame in proportion
  to the visible window, its row index and a bounded prefetch, never to the
  capture size. Neither layer materializes the whole result.
- **Workspace Diff is O(1) in git subprocesses**: tracked changes use one
  `git diff --numstat HEAD` plus one multi-file `git diff HEAD`, and untracked
  files are rendered as new-file patches in Rust; the process count does not grow
  with the file count.
- **A decision overlay's focused row is always visible**: approval /
  clarification / every picker keeps the focused row and its decision block
  inside any drawable height, and a resize returns to the focus.
- **An interaction is not a panel**: an approval, a question or a picker opens
  on the ordinary divider above the conversation, shares that conversation's
  background and divider ink, and states its question once — while the card is
  open, the running call's row and the status strip do not repeat it.
- **An approval or a clarification never hides the conversation behind it**:
  the panel owns Enter, Esc and the arrows. Page keys, and a wheel over the
  conversation viewport, keep scrolling that conversation. A wheel over the
  panel itself does not. Every single-choice clarification still offers a
  free-text answer even when the model did not ask for one (a deliberate
  exception to the client presenting runtime facts and nothing else: a list of
  picks is not a closed set, and the answer is still the user's own string).
  The agent roster names each child's role (explorer / worker / reviewer),
  never the spawn brief it was given.
- **`Diff Failed` is not Empty**: an unreadable workspace must be shown as a
  failure, never as "no changes".
- **Error source and delivery are structured facts**: `LocalConfiguration`,
  `Auth` (structured `error.code`) and `ConnectionRefused` (structured
  `io::ErrorKind`) are decided from facts; `message.contains(...)` string
  matching is forbidden.
- **A confirmed Edit Diff keeps its whole record one gesture away**: the
  Conversation keeps edits separate from ordinary tool receipts and retains
  file/change statistics. A patch long enough to flood the turn is summarized —
  its structure, both ends of every change run, and the number of hidden rows —
  because a three-hundred-line diff scrolled the rest of the turn off the screen;
  opening the row (`Ctrl+O`, or a click) prints every row, so no change is ever
  out of reach, and a patch short enough to read is shown whole. The full-screen
  Diff page always retains complete content. The execution tool retains the
  complete diff after a successful commit without a byte budget dropping it.
  When an edit moves a file, both diff paths identify the actual source and
  committed destination.
- **Failure details start collapsed with a reachable disclosure**: failed ordinary
  tools remain inside their group; its closed header shows only a failure marker
  and count. Clicking it exposes the failed targets and sanitized concrete errors.
  Standalone terminal failures, including request failures, also start with closed
  details and disclose them through their own clickable header. No detail hint may
  lack a corresponding interaction. Error source, code, recoverability and raw
  facts remain unchanged.
- **A transcript entry's fold is presentation state only**: tool-group and failure
  disclosure state belongs to the TUI, never the Runtime, event log or persistence.
  Folding cannot portray running, cancelling, cancelled or unknown work as complete;
  headers still report real state. Later events do not arbitrarily override a
  reader's explicit disclosure choice.
- **A live Thought paints beside the work; a settled Thought leaves**: while
  reasoning is arriving, the Conversation shows `◆ Thinking…` and the streaming
  body on its own rail, as its own row. That row is not a member of the process
  receipt. When the segment settles, the row leaves; opening the receipt or
  pressing Ctrl+O does not restore it. The status strip still reports the real
  streaming fact and clears it at completion. Raw reasoning text, duration, events,
  persistence and provider replay remain unchanged; narration deduplication cannot
  delete reasoning or repeat reasoning already emitted by the stream.
- **Process receipts accumulate within visible stages**: consecutive ordinary tools
  (Read / Search / List / Run / MCP) in one segment accumulate in a receipt that
  starts closed, including a lone tool. Neither a model round nor a Thought is a
  receipt boundary. User messages, assistant progress prose, the Final and separately
  displayed edits, permission/user decisions and plan/goal management entries
  delimit real visible stages; the view never guesses or generates a stage.
  Receipts are view-time folds; original tool and reasoning records remain intact.
  Labels accumulate real tool kinds and call counts (for example, "Read 3 times ·
  Searched 2 times · Ran 2 commands") without inventing action names. Failed calls
  stay in their kind buckets, with a separate failure marker and count.
  A live receipt's header explicitly reports running work and its real count;
  folding does not hide busy or cancellation facts. A returned background starter
  cannot portray its still-running process as settled. Background terminal events
  update the starter matched by its real task_id, without a duplicate transcript
  notice when that starter exists; unmatched outcomes retain their notice. Transient
  notifications and background-task details remain accessible.
- **Tool details start collapsed**: a closed receipt shows no successful output,
  failure reason or live output. Clicking its header restores tool members and their
  own disclosures in real chronology; error bodies appear only after expansion.
  Closing a parent hides members while retaining explicit disclosure choices for
  reopening; new members start closed. Successful silent internal tools neither
  contribute to receipt counts nor delimit visible stages; their original records
  and available output remain accessible through tool details. Ctrl+O opens the
  latest execution's details — a tool group or a user-typed `!command` — and
  expands that block inline; it never substitutes a Thought when no execution
  exists or becomes a full-history toggle.
- **Progress prose and the Final are shown in full**: model text uses natural
  paragraphs without a `●` / `◆` prefix on every paragraph. The coding domain's
  communication contract and the model own progress content; the TUI never generates
  discoveries or next steps. These grouping, disclosure and diff rules change only
  product presentation, not Runtime lifecycle, execution permissions, side-effect
  ownership, event facts or persistence.
- **Command details remain accessible**: ordinary commands expand in place in the conversation, showing the full command from the original arguments wrapped to terminal width; the running status does not repeat long commands. The background-task page retains a task list above its detail pane, with task identity and stop controls fixed while only detail content scrolls. Full commands and long logs wrap to width; mouse clicks select tasks, and the wheel selects tasks or scrolls details according to its region. Each task retains its own scroll and follow state, and cancellation results still come from Runtime.
- **Shell Mode is input state, not a second execution surface**: `!` on an
  empty Normal composer is a mode marker and never becomes command text; the
  prompt, accent and placeholder change without moving the composer. Esc,
  Backspace and Ctrl+C on an empty Shell composer leave the mode; the global
  key map is otherwise unchanged (Shift+Tab keeps the permission cycle,
  Ctrl+J/Alt+Enter keep newline entry, Tab only completes inside Shell Mode).
  Completion is deterministic and offline over files, directories and PATH
  executables; a shared prefix fills only the common part, an ambiguous or
  capped (truncated) result opens the menu instead of accepting, and accepting
  never executes. The candidate menu owns Tab/Enter/arrows/PageUp/PageDown/Esc
  while open and is pointer- and wheel-operable.
  Each execution is one inline record fed by the same runtime event as the
  fixed live-activity row above the composer: a left status rail distinguishes
  Running (purple, time-based breath), Ran (green), Failed (red, real exit
  code) and Cancelled (amber, explicit); stdout and stderr keep their
  provenance ink; the exact command has its own `$` line; output is terminal-
  safe, folds with an explicit marker and remains fully reachable. A result
  above the inline budget is never solved by raising an in-memory cap: the
  record keeps a bounded preview and Enter or a header click opens the full
  output page inside the TUI. That page holds only a logical-line byte index
  and per-width wrapped-row counts, reads `combined.log` for the visible
  window, and supports g/G, PageUp/PageDown, the wheel, drag-copy and `a` to
  copy all. A width change rebuilds the index in the background and keeps the
  top logical line. Capture directories are `0700` with `0600` files; a live
  session retains every complete capture its records name, and a deleted
  session's directory is reclaimed. A persisted path is accepted only under
  the local `shell-output` root with `combined.log` as a regular file.
  The inline header's rightmost cells and the live row are clickable stops; Ctrl+C cancels
  exactly that execution, never the agent, the side thread or the application.
  Only one foreground user command runs at a time. Resume restores the record
  from the runtime without re-execution. A running foreground command can move
  to the background with Ctrl+B or by clicking `↗ background` on the inline
  header or the live row: the same execution ID, process and process group are
  adopted by the existing background-task registry, with no respawn, re-run or
  appended `&`. Output, exit code, duration and cancellation stay continuous;
  the live row disappears, the record survives as Background and the existing
  background list finds it by task ID. Ctrl+C cancels only the foreground
  execution; a background task is stopped only through the background stop.
  Resume never re-executes a background record, and a record without a terminal
  fact after a runtime restart is shown as unknown, never as Running or
  Completed.
- **Real transcripts start at the viewport top**: until content fills the viewport, unused rows stay below the transcript so streaming and tool-group folding do not move the first message. Only the empty-session splash is centered. Painting and mouse/selection mapping use the same Conversation geometry policy; overflowing content retains normal scrolling and live-tail following.
- **Folding must not move the reading position**: the line anchored at the
  viewport's top edge keeps its place when an entry above or inside the viewport
  is folded. Runtime completion or interruption that regroups tool receipts uses the same
  anchor. If the anchored body is
  removed, stay near its disclosure header; following the live tail stays at the
  bottom without an anchor.
- **Ctrl+G jumps to the Final answer**: the anchor reuses the same memoized
  projection; with no Final it only notifies and never moves the viewport.
- **Clipboard delivery evidence**: TUI terminal-environment and clipboard modules own copy routing without changing Runtime permissions. Only a native write followed by matching readback on a local, non-tmux host is verified. SSH skips the remote native clipboard; tmux also skips native writes even without SSH environment variables, because inherited server environment cannot establish the current attached client's location. A tmux buffer write or OSC 52 send reports unverified delivery; DCS passthrough support remains unknown and tmux configuration is never silently changed. Terminal diagnostics only report observations and unknown capabilities without reading or writing the clipboard.
- **Performance baseline**: `leveler-tui`'s `perf_bench` is the deterministic
  performance regression baseline and `ux_experiment` the 80×24 UX regression
  fixture; both run manually with `--ignored`.

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

### 15.1 Declarative Agents Live Above the Engine

A user- or project-defined agent (`.leveler/agents/<name>/agent.yaml` + `instructions.md`) is **Harness state, not Runtime state**:

```text
Declarative agent (agent.yaml + instructions.md + skills)
        ↓
Agent Registry (Coding Harness: resolve, validate, precedence)
        ↓
Capability admission (an existing child contract, narrowed)
        ↓
The same durable multi-agent Runtime
```

- The Registry decides which definitions exist and whether each is usable; the model decides when to use one. The Engine never selects agents by file type or task content.
- A definition maps onto an existing capability class (read-only, late-bound writer, scoped writer). A new agent never adds a runtime role, and every bound it declares can only narrow its class.
- The resolved definition is snapshotted into the child's durable spawn record. A restarted child runs under that snapshot; the Registry is not consulted again.
- The Registry must not move down into the Engine: agent identity is a product and harness concept, and the Engine keeps owning only lifecycle, ownership and persistence.

See [Custom Agents](AGENT_EXTENSIBILITY.md).

### 15.2 Declarative Skills and the Skill Registry

A skill (`SKILL.md` plus optional `scripts/` and `references/`) is a domain knowledge pack, not a new runtime role. Like declarative agents, skills live above the Engine:

```text
Compatible skill sources (project/user × native/Codex/Agent Skills/Claude)
        ↓
Skill Registry (resolve, validate, identity, precedence, shadowing, status)
        ↓
load_skill · $mention · per-turn index · Agent.skills · /skills · CLI
```

- **One identity**: the directory name is the canonical skill name; a frontmatter `name` that disagrees is `invalid` rather than a silent second name. Discovery and loading always point at the same skill.
- **Sources rank by locality**: project over user over built-in, and a fixed source order within a scope. A covered definition stays diagnosable (`shadowed`), and a broken higher-precedence definition never silently falls back to a lower-precedence one of the same name.
- **Consume in place, never copy**: compatible skills already installed on the machine are read where they live (`~/.codex/skills`, `~/.agents/skills`, `~/.claude/skills`), so another tool's update is visible on the next resolve.
- **Read side and write side are separate**: the Registry is read-only; the project/user skills CodeLeveler itself manages are written through the Skill Store — validated, confirmed by a person, then replaced atomically. Built-in and external skills can be loaded but not rewritten or deleted here.
- **Progressive disclosure holds**: the index carries name + scope + description only; the full `SKILL.md` is loaded on demand by `load_skill` or a `$name` mention.
- Neither the Registry nor the Skill Store may move down into the Engine: a skill is a product and domain concept, and the Engine keeps owning only lifecycle, ownership and persistence.

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

### 18.11 Model Independence Boundary

```text
Model identity does not determine runtime behavior.
Model differences are expressed through capabilities.
Behavioral correction is based on observable runtime facts, not model names.
As models become stronger, unnecessary Harness intervention should decrease.
```

### 18.12 Data and Privacy Boundary

```text
CodeLeveler does not report product telemetry or user work data by default.
Necessary data egress must be triggered by an explicitly enabled capability and follow least-necessary-data rules.
Hidden data upload must not become part of Runtime, Harness, or Provider adaptation.
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

Parent/child sessions, the Explorer / Worker / Reviewer runtime roles, and declarative project/user agents are already in the product. Further roles and other domains should reuse that same mechanical model rather than inventing a second runtime. Automatic delegation is available when the model chooses it; it is not a product guarantee that spawning children is faster or cheaper.

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
