# CodeLeveler Architecture

Chinese version: [`ARCHITECTURE.zh-CN.md`](ARCHITECTURE.zh-CN.md)

CodeLeveler is a reusable foundation for running agents. The coding agent is the first product built on that foundation, not the highest abstraction in the system.

The architecture separates concerns deliberately: the model thinks, the domain harness defines what kind of agent this is, the runtime keeps it running reliably, capabilities do real work, host authority controls real side effects, persistence provides continuity, and products turn those pieces into user experience.

Custom agents: [`AGENT_EXTENSIBILITY.md`](AGENT_EXTENSIBILITY.md).

---

## 1. The Whole System at a Glance

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

The names in this architecture describe ownership. Each term answers “who is responsible for this?”

| Term | Meaning | Common confusion |
| --- | --- | --- |
| **Agent** | A goal-directed running entity that uses a model and capabilities to do work | An agent is not a model; the model is its source of intelligence |
| **Model** | The reasoning engine: understanding, planning, judgment, generation | The model does not own filesystem, process, or task lifecycle |
| **Harness** | The domain layer that adapts a general model and runtime to a concrete domain such as Coding or Review | It is not the runtime and not a replacement brain |
| **Agent Runtime** | The reusable machinery that lets an agent loop, pause, resume, cancel, persist, and recover | It runs the agent; it does not define domain reasoning |
| **Agent Kernel** | The smallest generic model↔tool execution loop | It knows no Coding or Review concepts |
| **Engine** | The long-lived component that owns sessions, tasks, turns, events, recovery, and continuation | **The Engine owns lifecycle, not intelligence** |
| **Capability** | A reusable system ability such as workspace access, command execution, browser control, or VCS | A capability is not the same thing as a model-visible tool |
| **Tool** | A model-facing adapter for invoking a capability | A tool should not become the capability runtime itself |
| **Tool Surface** | The set of tools visible to a model for a particular Harness and product | Owning many capabilities does not mean exposing all of them every round |
| **Host Authority** | The single boundary with final authority to perform controlled side effects | Authority is stronger than permission configuration; it is who can make the effect real |
| **Persistence** | Durable state that lets tasks, turns, ownership, events, and work continue across time | It is much more than chat history |
| **Authority** | The canonical owner of a class of facts or actions | One fact must not have competing authorities |
| **Product** | The user-facing composition: TUI, Web, Desktop, Mobile, and concrete agent products | A product presents runtime facts; it does not create them |
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

> **The Harness defines what kind of agent this is. The Runtime defines how that agent runs reliably.**

### 2.2 Why “Authority”

Permission configuration says what should happen. Authority says who can make it happen, and no second path may do so. Filesystem mutation, process execution, network access, sandboxing and approval all cross one authority; a plugin, child agent, or product cannot create another.

---

## 3. Seven Core Principles

**Primary objective: maximize the capability each model can realize.**

> **Harness is an amplifier of model capability, not a constraint on it.**
>
> **The model defines what is possible; the Harness determines how fully that capability can be realized.**

```text
The Model owns intelligence.
The Harness owns domain semantics.
The Runtime owns lifecycle and mechanical correctness.
Capabilities own reusable abilities.
Host Authority owns controlled real side effects.
Every persistent fact has one authoritative owner.
Products project Runtime truth; they do not create it.
```

The central lifecycle boundary:

> **Engine owns lifecycle, not agent intelligence.**

That prevents the Engine from slowly becoming a state machine that contains planning, judgment, tool choice, and product-specific semantics.

---

## 4. Model: Where Intelligence Comes From

The model owns work that requires reasoning: understanding the goal, planning, investigation, choosing tools, generating code or content, debugging, trade-offs, semantic judgment, and adapting after failure.

The system provides clear capabilities, stable semantics, precise errors, real environment state, deterministic mechanical constraints, and reliable execution results. It does not build a hidden second reasoning system to compensate for model weakness.

```text
MODEL CAPABILITY LIMIT
        ≠
RUNTIME DEFECT
```

The Runtime fixes engineering problems—failure, concurrency, cancellation, recovery, authority, consistency—not intelligence limits.

### 4.1 Model Independence: Maximize Model Capability, Minimize Model Adaptation

> **Maximize native model capability, minimize model-specific adaptation.**

The Harness amplifies what each model can already do. It does not equalize models, narrow the gaps between them, or take over their planning and judgment. Safety and host-authority boundaries stay in force.

Model identity must not be an input to execution policy: no `if DeepSeek`, `if GLM`, `if GPT` branches. Real differences are expressed through explicit capabilities such as context window, reasoning support, parallel tool calls, vision, structured output, and output limits. The system negotiates capabilities instead of guessing behavior from a model name.

The Kernel owns model-independent mechanics: model steps, tool execution, streaming, cancellation, timeout, budgets, usage, retries, persistence, and recovery. It must not encode beliefs about what a particular model needs.

The Harness may provide soft policy or execution feedback based on observed runtime facts—repeated verification, prolonged lack of progress, repeated identical failures—never model identity. Adaptive policy must be observable, explainable, disableable, measurable, model-independent, and must not replace the model's judgment.

Better models should produce better results without restructuring CodeLeveler or adding model-specific branches.

---

## 5. Harness: Defining What Kind of Agent This Is

The Harness sits between model, runtime, and product. It answers: **in this domain, what can the agent see, what can it do, what rules apply, and what does domain completion mean?**

### 5.1 Coding Harness

A Coding Harness may own the coding-domain contract, repository semantics, coding tool surface, test/build/lint tool semantics, delegation semantics, multi-agent write collaboration, coding completion semantics, and coding context organization.

### 5.2 Review Harness

A Review Harness may own review target, scope, findings, severity, evidence, verdict, and review completion semantics.

### 5.3 Harnesses Are Siblings

```text
                     Agent Runtime
                  /       |        \
                 ▼        ▼         ▼
              Coding    Review    Research
              Harness   Harness   Harness
```

Review is not a mode inside Coding, and Research is not an extension pack under Coding. A new domain reuses the common Runtime and common Capabilities without inheriting another domain's semantic baggage.

### 5.4 What a Harness Owns—and Does Not Own

| Owns | Does not own |
| --- | --- |
| Domain vocabulary, constraints, tool selection | A second task lifecycle |
| Domain completion semantics | A second persistence system |
| Domain collaboration rules | A path around Host Authority |
| Domain context organization | Hidden reasoning on behalf of the model |
|  | A second permission or ownership system |

### 5.5 Execution Feedback

Each request carries a read-only projection of authoritative execution state—elapsed time, budget, executed commands, observed changes, declared plan status—not a second task ledger. The projection is regenerated from the existing owners for every request and never becomes durable history or completion evidence.

Model requests separate the stable control channel (contract, project rules, selected skills, memory recall, request-local state) from the conversation transcript. Control is assembled at one entry point shared by fresh turns and recovery. Accounting reflects the request that is actually sent.

Project-rule delivery is a host contract, not a model decision. The project `.leveler/config.yaml` names the mandatory sections; the runtime delivers every mandatory section first, then optional sections in document order, under one byte budget per request shared by all rule documents. Rule text is ordered ahead of the workspace listing and keeps `ProjectInstruction` authority — position never raises it. Anything not delivered is named by source, section id and heading in the prompt, and a mandatory section that does not fit is reported loudly rather than dropped. `read_project_rules` is an optional lookup over the same loader; no mandatory rule depends on the model choosing to call it.

Every model attempt is recorded against the same resource ledger before any retry decision; a failed attempt, an interrupted stream, and a child call are not free. Children, reviews, and workflow stages spend the parent task's remaining budget. A background child does not keep spending against a launch-time snapshot. Cancellation and deadlines wake even when no new event arrives. The model owns strategy changes based on these facts: tool activity is not goal progress, and a plan is a declaration, not evidence.

### 5.6 Prompt / Context Authority

Instruction and data are different channels. Every control block records its source, how the model should interpret it, and how long it lives.

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

`CoreContract` is the harness contract: permissions, tool protocol, evidence, completion, and product delivery. `UserIntent` is the current user objective and stays in the transcript. `ProjectInstruction` is `AGENTS.md`, project instructions, and scoped repository rules; a project file cannot change permissions or raise its own authority. A user-named skill is `UserSelectedProcedure`; the skill catalog is advisory. Memory and compaction summaries are `AdvisoryContext`. Tool results are `ExternalData` and never become instructions.

Providers change representation, not authority. Historical rows whose origin is unknown are not promoted to user intent or system contract by their transport role.

### 5.7 Context Lifecycle

Two owners decide what one request contains: the lifecycle decides which semantic blocks are still active, and the request projection decides how they enter this route. Pressure is measured from the request that is about to be sent, including control context, tool schemas, and the reasoning channel.

When history is folded, mid-transcript project rules are carried verbatim, the tail working set is bounded, and the elided middle becomes a model-written briefing marked as advisory rather than a system fact. The raw transcript is never rewritten; resume rebuilds the active surface from the persisted snapshot. A parent receives a bounded child settlement, not the child's transcript.

---

## 6. Agent Runtime: Keeping Agents Alive Reliably

```text
Agent Kernel
    +
Persistent Engine
    =
Agent Runtime
```

### 6.1 Agent Kernel

The Kernel owns the generic model execution loop: model interaction, the tool loop, streaming, round progression, budgets, usage accounting, retry and backoff, timeouts, cancellation, and stopping.

It does not know Coding, Review, repository workflows, product prompts, domain completion, UI behavior, or user acceptance. Its value is that every Harness runs on the same loop.

### 6.2 Engine

The Engine owns long-lived lifecycle mechanics: sessions, tasks, turns, events, persistence, pause and resume, recovery, cancellation, background work, parent-child relationships, ownership, and runtime outcomes.

It may know that a task is running, cancelled, or finished. It must not infer from “tests passed” or “the tree changed” that the user's request is semantically complete.

### 6.3 Goal Mode Is Scoped to a Goal

Collaboration mode — ordinary conversation, plan, goal — is the Harness's choice of execution contract, not a session-wide permission switch and not a permanent mode. A Goal begins explicitly (`/goal <task>`, a goal entry point) or when the model declares that an ordinary turn's request is work to be carried to completion, with a stated deliverable and an acceptance need. Every mode keeps the full tool surface its permission allows; the Goal contract only adds progress tracking, an explicit `update_goal` terminal, completion verification and recovery.

Goal lifecycle, message ownership and execution mode are three separate facts, and no single field infers one from another:

- **Goal lifecycle** (`goals.state` with `sessions.outcome`): whether the goal still owes work and whether it can be re-entered. Owed work and re-enterability are the same decision; a blocked, recoverably failed, interrupted or budget-stopped window leaves the goal owing work, while delivery or an explicit cancellation settles it.
- **Message ownership**: whether a new message is conversation, a continuation of the goal in front of it, or an explicitly declared new goal. A continuation re-enters the goal's durable identity; `/goal <task>` is the one way any message declares a new goal.
- **Execution mode** (`sessions.collaboration`): whether THIS turn runs conversation or the goal executor. A goal is an execution window of one goal, not a permanent session mode.

When a goal window closes, the route ends in the same commit as the terminal fact: after a closed window (delivered, blocked, failed or cancelled) later ordinary messages are conversation again, and clients are told so rather than left showing a Goal the runtime no longer runs. An interrupted or budget-stopped window is only paused, so it keeps the route and the user's supplement continues the same goal. A session created on the goal axis whose window never opened a goal (a parallel dispatcher, say) keeps its declaration axis: a later message there declares a new goal rather than continuing work nobody ran.

Keeping these facts apart matters because "the goal still owes work" does not mean "the next message must run as goal work". After a block, asking "why was that denied?" is a follow-up to the model, not a new execution order: it is answered as ordinary conversation, and the goal keeps its identity, objective and recovery path; a later `继续` re-enters the same goal by id. No leftover route turns an ordinary message into a new goal, and no message length, keyword, tool count or round count decides any of this. When the model declares execution work from an ordinary turn while the task already owes a goal, the declaration joins that existing identity instead of opening a second one.

```text
Engine owns lifecycle.
Harness owns domain semantics.
Model owns reasoning and judgment.
```

---

## 7. Capabilities and Tools: What the System Can Do vs. How the Model Invokes It

### 7.1 Capability

A Capability is a reusable ability the system actually owns: workspace access, command execution, browser control, search, code intelligence, version control, memory, media processing, skills, remote execution, external services.

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

> **How does the model request that ability?**

```text
Tool ≠ Capability
```

One Capability may have multiple Tool adapters, and a Harness or the Runtime may use a Capability directly when that belongs to its responsibility.

### 7.3 Tools Stay Thin; Capabilities Have Clear Ownership

A Tool owns its model-facing name and description, parameter schema, input decoding, capability invocation, result rendering, and precise errors.

A Capability owns real domain behavior, reusable logic, its necessary state and lifecycle, and consistency. Long-lived state, global policy, service discovery, permission decisions, and process lifecycle stay out of Tools.

### 7.4 Capability Is a Responsibility, Not a Mandatory Package Boundary

“Capability” describes ownership, not a required crate, service, process, or trait. Physical separation follows real boundaries: multiple real consumers, independent lifecycle, security, protocol, or persistence boundaries, remote deployment needs. Do not manufacture abstractions to make the diagram symmetric.

---

## 8. Tool Surface: How Much Should the Model See?

Owning many capabilities does not mean the model should see every tool in every round.

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

> **Expose the clearest, most stable, least ambiguous interface that still preserves the necessary capability.**

A good tool expresses one clear intent, has stable semantics and predictable input and output, reports failure precisely, and has a clear boundary from neighbouring tools.

The system may enable or disable a Tool based on real mechanical capability—whether the host has a browser or the model supports vision. It must not silently change Tool semantics because “this model is weak” or “this task looks hard”.

### 8.1 Available, Permitted, and Exposed Are Three Different Facts

```text
AVAILABLE   what this runtime can provide
PERMITTED   what configuration, permissions, and sandbox allow
EXPOSED     what the current request carries

EXPOSED ⊆ AVAILABLE ∩ PERMITTED
```

Availability is mechanical: a configured search provider, `git` on `PATH`, vision support, a drivable browser. Permission is owned by host policy and project configuration. Loading changes exposure; it cannot create availability or grant permission.

The model decides which capability its task needs; the harness validates the request and loads the existing subsystem. It never selects capabilities from task wording, model reputation, provider identity, or step count. A loaded capability stays until its goal ends; an unfinished goal restores its durable capabilities, and a new goal starts from the base surface. Collaboration mode (`chat`, `plan`, `goal`) and execution permission keep independent meanings.

### 8.2 Browser Control and Web Search Are Separate Capabilities

They are configured separately, become available for different reasons, and neither is the other's fallback. A missing search key does not turn the browser into a search tool, and a browser that cannot start does not redirect to search.

Browser control is a runtime-owned automation session, not control of an ordinary window the user already has open. Web search keeps a provider-neutral tool contract: the schema and the results are fixed by the tool, so replacing the backend cannot widen it.

### 8.3 Durable Memory Has One Identity and One Lifecycle

Durable memory is advisory and user-owned. Project sources (`AGENTS.md`, project instructions, the current code) outrank it; a recalled memory records what was true when written and never becomes a system fact.

One fact has one identity: normalization is deterministic, known spellings unify, and unknown spellings stay distinct instead of being merged by fuzzy similarity. Memory is written only through the same host authority path as any other mutation, and credentials do not enter memory, logs, or projections. Disabling memory removes its tools, guidance, recall, and extraction without deleting data.

Fact meaning is supplied by the model or by a person; the runtime never infers it from prose. The runtime may parse protocols, exact CLI syntax, paths, structured values, evidence ids, exit codes, permission rules and credential shapes. It must not decide identity, equivalence, causality, applicability, or whether two facts are one through English/Chinese topic lists, stopword lists, synonym maps, sentence patterns, connectives, or text similarity. `./dev arch` enforces this shape and requires every exception to name a reason, an owner and a test in `docs/ARCHITECTURE_GUARD.exceptions`. The guard is a structural screen, not a proof of intent; code review remains the final owner of meaning.

---

## 9. Host Authority: Who Is Allowed to Change the Real World?

The model may request a side effect, but it does not own host power.

```text
Agent requests
      ↓
Host Authority decides and performs
      ↓
Operating System changes
```

```text
what the agent wants to do
          ≠
what the host allows to happen
```

No Tool, plugin, child agent, or Product may create a second path around Host Authority.

### 9.0 Permission Mode Product Contract

`FullAccess` (Full) selects unrestricted execution before admission policy, bypassing permission rules, approval, sandbox, resource scope, credential filtering, and task ownership restrictions. Files, HOME, credentials, arbitrary Git remotes, loopback/LAN/Internet, process control, and implemented future capabilities produce no permission ASK or DENY; the approval count is zero. Unavailable capabilities, invalid tool inputs, and OS or external-service errors remain real failures. Cancellation, deadlines, durability, and resource cleanup remain runtime responsibilities. Full removes permission decisions, not tool-surface disclosure: every implemented capability stays usable and enabling one needs no approval, while optional packs may enter the session's request surface on demand without taking any tool away.

`Assisted` (Auto) allows ordinary reads, writes, builds, tests, localhost, owned-task operations, and Git operations. Auto asks only at three boundaries:

| Boundary | Examples |
| --- | --- |
| Deletion | `rm`/`rmdir` of an absolute path, `..`, `~`, a variable, or a glob; `dd`, `mkfs`, shutdown. A literal relative path inside the working directory runs directly |
| Privilege | `sudo`/`su`/`doas`, opening a host application, MCP, cross-session task control, agent/skill/memory definitions |
| Security | Credential plaintext, a Git remote whose destination has a stored credential, writes outside the workspace, Git configuration or arguments that execute a local program |

An approval is frozen for that exact call and consumed by the execution path. The same approved action cannot be rejected by another permission layer, and one-call approval never elevates later unapproved calls.

**The sandbox must not be stricter than the permission policy.** Where the permission layer decides ALLOW, the execution stage must not answer the same capability with a silent denial. Where a capability genuinely cannot enter the sandbox, the permission layer asks or refuses with a structured reason instead of exposing a bare OS error.

`RequestApproval` (Restricted) uses scoped resources: destinations and resolved addresses are described, and every redirect is revalidated. Classification alone is not enforcement evidence. Where a scope cannot be enforced, the answer is an explicit ASK, DENY, or Unsupported—never a silent widening.

Current process-network support: macOS denies networking when the scope is `None`, and loopback is partial (canonical `127.0.0.1`/`::1` outbound only, no listeners). Linux and Windows report restricted network scopes as unsupported. `Internet` and Full keep open networking. Resource grants and credential brokering may constrain Auto and Restricted; they never add interception to Full or to an already approved action.

### 9.1 Why There Must Be One Execution Authority

If file mutation follows one authority, commands another, and child agents a third, there is no unified safety boundary.

> **Requests may come from many places. Final execution authority must remain singular.**

### 9.2 Data Egress and Privacy Boundary

Network access is a real host side effect, so data egress follows the same authority and least-necessary-data principles.

By default, CodeLeveler does not collect or report product telemetry or user work data—source code, file contents, conversations, prompts, model output, command output, environment information, usage analytics, or crash context—to the CodeLeveler project or a central service.

Explicitly enabled external capabilities—a model provider, MCP, web search, remote execution, update checks—may make the network requests required for that capability. Such egress is triggered by an explicit feature, explainable in destination and purpose, limited to the data necessary, never broadened through hidden fallback, and never used to send credentials or secrets as telemetry. Future diagnostics must be explicit opt-in and disableable.

---

## 10. Mechanical Facts, Semantic Completion, and User Acceptance

```text
Mechanical Truth
      ≠
Semantic Satisfaction
      ≠
User Acceptance
```

### 10.1 Mechanical Facts Belong to the Runtime

The Runtime can authoritatively record whether a command ran, its exit status, whether a file changed, whether an event occurred, whether an artifact exists, and whether a process exited.

### 10.2 Semantic Completion Belongs to Model + Domain

“Tests passed” does not automatically imply “the requested feature is complete”. Whether the evidence satisfies the domain goal requires semantic interpretation in the context of the Harness.

### 10.3 The User Owns Final Acceptance

```text
Runtime owns facts.
Model interprets those facts semantically.
User owns final acceptance.
```

A green check, a successful tool call, or a changed file is not an automatic “done”. The model cannot shrink the user's scope by shrinking its own acceptance list.

### 10.4 Final Prose Is Not a Task Terminal

Final assistant text proves only that generation stopped.

```text
Running
   ↓ final assistant text ends
Finalizing
   ↓ dependency settlement, required review, outcome resolution
Terminal (TaskFinished)
```

`TaskFinished` is the only authoritative task terminal. Clients project completed, failed, or blocked results only from a persisted `TaskFinished`; final prose, turn completion, and background cleanup cannot substitute for it. Declarations, verification evidence, and lifecycle describe separate facts. Once the authoritative result is committed, the product publishes the terminal immediately.

### 10.5 Tests, Builds, and Linters Are Ordinary Tools

The Runtime does not generate a verification plan after task completion, and it does not map test, build, or lint results into a second terminal state. The model runs them when the user asks.

---

## 11. Persistence: Letting Work Continue Beyond One Conversation

Long-lived agents require durable state.

### 11.1 Persist Before Forward

Input, ownership, and the intent to act are persisted before the work they authorize becomes visible or irreversible. A crash can lose the future, not the admitted past.

### 11.2 Persistence Provides Continuity

Sessions, turns, events, approvals, artifacts, and background processes survive a closed UI. Resume rebuilds the active surface from durable facts; it never invents progress that was not recorded. Settings are version-protected so a stale client cannot silently overwrite a newer choice.

### 11.3 Durable Turn Admission

A turn is admitted durably before its model call. A repeated client request is matched to the same admitted turn instead of running twice; an unknown outcome is never automatically retried as if it had not happened. Cancellation is requested, delivered, and terminated as separate facts, and only the terminal fact is shown as stopped.

---

## 12. Model Providers and Capability Negotiation

The Provider / Protocol layer normalizes request and response protocols, streaming, tool calling, reasoning transport, structured output, vision, context and output limits, and error mapping.

### 12.1 Protocol Differences Can Be Adapted; Semantics Cannot Be Faked

Wire differences can be normalized. A genuinely missing mechanical capability is reported as absent instead of being simulated by a hidden behavioural path.

### 12.2 Available Capability Is an Intersection

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

### 12.3 Declared Facts vs Harness Policy

A model profile declares facts: context window, completion capacity, the quality boundary past which long-context recall degrades, the protocol it speaks, and that protocol's compatibility facts.

It does not declare how much of that capacity the harness spends. Completion reservation, safety headroom, the fold threshold, and the recent-tail budget are harness policy. Replay follows the same split: what a protocol carries back is a route fact, not a preference.

A route fact can be imposed by the model family's wire protocol even when the user never wrote it down. Resolution has one order: the provider/model protocol capability, then the user's explicit declaration, then the conservative default (`never`). An explicit declaration may strengthen a documented default and may opt out of a family default a deployment knows it disabled; it may not weaken a provider's hard requirement. Weakening a hard requirement (DeepSeek with tools) is a startup error naming the model and field, not a request the provider rejects with HTTP 400. `leveler login`, a hand-written config, the lab, dogfood and every client resolve through the same function.

Only families whose published protocol requires historical reasoning are listed, and each is listed with the mode that makes its rule binding: DeepSeek thinking mode (a request carrying `tools` must carry the full `reasoning_content` of every historical assistant turn, including turns that called no tool; an empty string is the measured form for a turn that captured none — without `tools` the endpoint ignores the field, so the scope is tool-keyed), GLM while the route declared preserved thinking (the endpoint is asked to keep prior turns' thinking, so every request that spans the history owes the complete, unmodified chain back, tool-bearing or not; a deployment that turned preservation off keeps only the documented tool-loop default and may opt out), and Kimi K2.7 Code and later including K3/K3.1 (thinking and preservation are unconditional, the complete assistant message comes back on every request, and there is no `thinking` parameter to send). A family not listed carries nothing — the harness never sends a field a route did not ask for — and Kimi ids below K2.7 are deliberately unlisted because the product no longer supports them. Replay is not squeezed into one boolean: "the channel must exist", "its content must be complete", "an empty value is legal", and "which requests owe the chain" are separate facts, and a route that cannot stop thinking does not advertise `off`.

### 12.4 Replayed Reasoning Has an Owner

`reasoning_content` carries no signature, so a replay is only meaningful as the same route's continued thought. Every assistant turn records the route that produced it — provider alias, wire model id, protocol, and the deployment when the provider layer knows it — so a resumed session can tell its own history from another model's. The comparison is per turn, not per session: after A → B → A, A's earlier turns are A's own reasoning again, while B's stay foreign.

A provider alias identifies exactly one deployment (one `base_url`), so the same wire model id behind two gateways is two routes. What a handing-off request keeps is everything the task depends on — the user's objective, the assistant's prose, the tool calls and the tool results. What it drops is the foreign private reasoning; where the protocol validates the channel's presence, it carries the legal empty value rather than presenting foreign content as native. Nothing is fabricated, and nothing is folded into the assistant text, because a model must not read another model's thinking as its own visible reply.

Missing, empty, incomplete and unknown are distinct facts. An assistant turn this route completed that produced no reasoning is *empty*; a payload with no recorded route is *unknown*; an interrupted stream never becomes a message at all, so partial reasoning can never be replayed as complete. `leveler login` writes model facts only: a generated config does not need the replay fields, and an advanced override remains accepted for a deployment that differs from its family's published requirement.

Preserved thinking is a route declaration, not a harness preference: where the wire spells it (`thinking: {type: "enabled", clear_thinking: false}`), the request carries it; where it does not, no field is invented. Turning thinking off is offered only on a route that can honour it.

---

## 13. Product and Clients: Experience Belongs to Product, Truth Belongs to Runtime

```text
CLI
TUI
Web
Desktop
Mobile
Remote Client
```

Products decide how information is presented, how interaction is organized, navigation, default UX, and which capabilities compose a product. They do not redefine runtime truth.

> **The UI is a projection of Runtime truth, not a new source of Runtime truth.**

```text
Client Command
      ↓
Runtime
      ↓
Runtime Event
      ↓
Client
```

```text
                     Runtime
                 /     |      \
                ▼      ▼       ▼
              TUI     Web    Mobile
```

One Runtime can serve multiple clients, and a client and the Runtime do not need to live on the same machine.

### 13.1 What is shipped today

| Product | Entry |
| --- | --- |
| CLI | `leveler` |
| TUI | `leveler tui` |
| Web | `leveler web` |
| Mobile | `apps/leveler-mobile` (source preview) |
| Remote host bridge | `leveler remote` and `services/leveler-relay` |

A standalone desktop app and cloud workers are not shipped. Custom agents: [`AGENT_EXTENSIBILITY.md`](AGENT_EXTENSIBILITY.md).

### 13.2 Client Protocol and Runtime Host

One semantic client protocol defines commands, events, and session snapshots, so every client speaks the same session, turn, and tool semantics instead of inventing its own. The protocol does not decide how a process starts or which local connection carries it.

Runtime Host is a separate boundary: it makes a connectable Runtime available by discovering, starting, adopting, or reviving it, and it handles compatibility, handoff, authentication, transport binding, and idle policy. It does not run the agent loop and does not own models, tools, permission decisions, session persistence, or recovery.

### 13.2.1 Optional Workspace and Global History Contracts

A session may have a primary workspace or none. A no-workspace session uses the same runtime and does not substitute HOME, the launch directory, a previous project, or a scratch path. Tools that require a workspace are refused even under Full access, because the required resource is absent.

Global history aggregates existing project and no-workspace records. It reads durable facts without creating a database, running a migration, starting a daemon, or keeping a second task ledger. A missing checkout keeps its history and is marked unavailable, and global scanning is available only to trusted local clients.

### 13.3 TUI projection and performance invariants

The terminal projects runtime facts and never derives its own. Internal performance constraints—bounded caches, incremental wrapping, windowed result pages—exist to keep that projection affordable and are not part of the product contract.

---

## 14. How One Task Actually Flows

Consider a coding task: “fix this bug and verify the result.”

| Step | Owner | What happens |
| --- | --- | --- |
| 1 | Product | Receives the goal and passes it to the Coding Harness |
| 2 | Harness | Decides the coding context, tool surface, rules, and completion semantics |
| 3 | Runtime | Creates the task and turn lifecycle, records events, manages budget, cancellation, and recovery |
| 4 | Model | Investigates, plans, and requests a Capability through a Tool |
| 5 | Capabilities | Perform the work; real side effects cross Host Authority |
| 6 | Runtime | Records mechanical facts: exit status, file changes, artifacts, lifecycle events |
| 7 | Model | Judges domain completion from the goal, semantics, and facts |
| 8 | Product / User | Presents the result; the user accepts or rejects |

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

Roles and domain semantics may differ. A child agent must not invent a second lifecycle, permission system, persistence system, or write path.

### 15.1 Declarative Agents Live Above the Engine

A user- or project-defined agent (`agent.yaml` + `instructions.md`) is Harness state, not Runtime state. The registry decides which definitions exist and whether each is usable; the model decides when to use one. A definition maps onto an existing capability class, and every bound it declares can only narrow. The resolved definition is snapshotted into the child's durable spawn record, so a restarted child keeps the definition it started with.

See [Custom Agents](AGENT_EXTENSIBILITY.md).

### 15.2 Declarative Skills and the Skill Registry

A skill (`SKILL.md` plus optional `scripts/` and `references/`) is a domain knowledge pack, not a runtime role. Project sources outrank user sources, which outrank built-in ones; a covered definition stays diagnosable. Compatible skills already installed on the machine are read where they live and never copied. The index carries only name, scope, and description; the body loads on demand. The registry is read-only, and skills CodeLeveler manages are written through the skill store.

---

## 16. Dependency Direction: Lower Means More General

```text
Product
  ↓
Harness
  ↓
Runtime / Capabilities
  ↓
Foundation
```

Higher layers are more product- and domain-specific; lower layers are more general, stable, and ignorant of upper-layer semantics.

```text
Runtime must not depend on a concrete Harness.
Harness must not depend on a concrete Product UI.
Foundation must not know Coding or Review concepts.
A Capability must not depend backward on the specific Tool or Product consuming it.
```

Lower layers provide mechanisms. Upper layers provide composition and semantics.

---

## 17. Architecture Guardrails: Allowed and Forbidden

| Layer | Allowed | Forbidden |
| --- | --- | --- |
| Product | New UIs, clients, interaction changes, product compositions, local view state | Deriving authoritative task state, treating tool success as completion, a second runtime truth, owning durable task lifecycle |
| Harness | New domain rules and Harnesses, tool-surface changes, domain completion and collaboration semantics | Pushing domain semantics into the Runtime, hidden planning for the model, a second lifecycle/permission/persistence system, bypassing Host Authority |
| Agent Runtime | Generic lifecycle mechanisms, stronger pause/resume/cancellation, background lifecycle, parent-child relationships | Coding or Review semantics, planning for the model, product-specific prompts |
| Capability | Reusable behaviour, necessary state and lifecycle | Global policy, service discovery, permission decisions, product presentation |
| Tool | Parameter schema, decoding, capability invocation, result rendering, precise errors | Becoming a runtime, a policy centre, durable state, or a process owner |
| Host Authority | The single execution path for files, processes, network, sandbox, and approval | Domain completion judgment, changing facts to match the UI |
| Provider / Protocol | Normalizing wire formats and negotiating capabilities | Injecting hidden behaviour for a model identity, changing domain semantics |
| Multi-Agent | Roles and domain collaboration above the Engine | A second runtime, ownership model, or persistence for children |
| New abstractions | A second real consumer, a real boundary, or an observed coupling problem | Speculative generality or symmetry for its own sake |

---

## 18. Architecture Invariants

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
Moving responsibility between layers must not weaken mechanical correctness,
authority, persistence, cancellation, recovery, or safety.
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
Necessary data egress must be triggered by an explicitly enabled capability and
follow least-necessary-data rules.
Hidden data upload must not become part of Runtime, Harness, or Provider adaptation.
```

---

## 19. Evolution Direction

### 19.1 Single Domain → Multiple Domains

Coding is the first domain. Review, research, and other Harnesses reuse the same lifecycle, permissions, persistence, and Host Authority.

### 19.2 Single Agent → Multi-Agent Runtime

Roles collaborate under one runtime rather than each owning a separate one.

### 19.3 Tool Collection → Capability Platform

Capabilities become reusable across Harnesses, products, and execution locations.

### 19.4 Local → Remote → Cloud

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

What changes is where the work executes, not what the Agent means.

### 19.5 Request-Scoped Agents → Durable Agents

Long-running goals, pause and resume, background execution, durable child sessions, and long-lived context. A single chat request is the shortest lifecycle, not the architectural ceiling.

### 19.6 Capability Negotiation as a First-Class Mechanism

Models, hosts, and remote workers expose different capabilities. The system negotiates them explicitly instead of relying on hidden semantic fallback.

### 19.7 Final Shape: Agent Platform

Harnesses sit on one shared Runtime, which sits on Capabilities, Persistence, and Host Authority, and Products compose all of it without owning any of its truth.
