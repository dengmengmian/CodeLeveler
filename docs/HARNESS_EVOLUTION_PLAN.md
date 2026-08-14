# CodeLeveler Harness Evolution Plan

> Status: architecture roadmap and decision record  
> Snapshot date: 2026-08-14  
> CodeLeveler baseline at snapshot: `f610d1d`  
> Related docs: [`ARCHITECTURE.md`](ARCHITECTURE.md), [`RUNTIME_EVOLUTION_PLAN.md`](RUNTIME_EVOLUTION_PLAN.md), [`FUTURE_RUNTIME_ARCHITECTURE.md`](FUTURE_RUNTIME_ARCHITECTURE.md), [`multi-agent.md`](multi-agent.md), [`BROWSER_CAPABILITY_REPORT.md`](BROWSER_CAPABILITY_REPORT.md)
>
> Core rule:
>
> **Keep the reliable Runtime Kernel authoritative, and add Harness composability around it only when real dogfood proves the need.**

This document is the single roadmap for the Harness-specific evolution of CodeLeveler. It exists to prevent three recurring forms of roadmap drift:

1. treating already-finished Browser work as a future milestone;
2. jumping from the existence of `spawn_agent` directly to a speculative multi-agent rewrite;
3. copying another harness architecture wholesale instead of adopting only the parts that fit CodeLeveler's runtime invariants.

The Runtime evolution roadmap remains authoritative for Task identity, ownership, storage ports, Local/Cloud runtime ownership, Scheduler and NPC. This document is narrower: it defines how the **Coding Harness** should evolve from the current real-usage baseline into a more composable, reliable multi-agent system.

---

# 1. Current frozen facts

At this snapshot:

- CodeLeveler baseline is `f610d1d`.
- Browser Capability V1 is **complete and merged**. It is not a roadmap item to reimplement.
- Browser is daemon-owned, uses structured browser tools, semantic refs, stale-ref safety, page/session isolation, a fail-closed network boundary, and has passed real React/Vite dogfood plus TUI↔daemon disconnect/reconnect validation.
- Runtime Core stability, storage ports, task ownership/fencing and durable local daemon work are already landed.
- Real Usage Batch #1 is the active product-validation line.
- Product code stays frozen during the batch unless a reviewed systemic blocker explicitly ends the batch and opens a repair gate.

The Browser production report remains the source of truth for Browser V1 details. During Batch #1, Browser is a **capability under use and observation**, not unfinished platform work.

---

# 2. Real Usage Batch #1 is the current gate

The immediate objective is not to add more architecture. It is to collect evidence about how the existing Harness behaves on real OSS tasks.

Execution discipline:

```text
run one R00X Goal
        |
        v
collect report + metrics
        |
        v
STOP
        |
        v
human review for systemic blocker
        |
        +-- blocker --> stop batch and classify
        |
        +-- no blocker --> start next frozen task
```

Snapshot matrix:

| ID | Repository / task family | Main pressure | Spawn expectation |
| --- | --- | --- | --- |
| R001 / R001b | `fd` | Small Rust CLI correctness | NONE |
| R002 | `miller` | Ordinary Go task | NONE |
| R003 | `hugo` | Medium Go task + verification scope | NONE / OPTIONAL |
| R004 | Excalidraw | React/TypeScript + real structured Browser verification + TUI observation | OPTIONAL |
| R005 | Cargo | Large Rust repository exploration | USEFUL EXPLORER |
| R006 | Casdoor | Large Go/full-stack cross-layer exploration | USEFUL PARALLEL EXPLORERS |
| R007 | Hoppscotch | TypeScript monorepo impact exploration | USEFUL EXPLORERS |
| R008 | ripgrep | Subtle correctness + independent review | REQUIRED_REVIEWER |
| R009 | go-task | Concurrency/state correctness + independent review | REQUIRED_REVIEWER |
| R010 | TailAdmin D3-style replay | Long task + frontend + Browser + spawn + final verification | USEFUL |

The important design property is that R001-R004 reward **spawn restraint**, while R005-R010 progressively create cases where delegation should become useful.

Batch #1 must answer at least these questions:

1. Does requirement narrowing recur as task/repository complexity increases?
2. Why do small tasks sometimes require 70-90+ rounds?
3. Does the agent use structured Browser capability instead of browser-related shell workarounds?
4. Does spawn reduce repeated exploration and main-context pressure, or only add model requests/tokens/wall time?
5. Does an independent reviewer catch correctness issues the Main agent would otherwise miss?
6. Is completion evidence real, and does false completion remain zero?
7. Are targeted, affected-package, repository-wide and environment failures classified separately?

Two batch-level spawn signals are mandatory:

## Useful Child Rate

How many child findings are actually consumed by Main, rather than merely produced.

This is evidence of information value, not a quality score by itself.

## Spawn Utility

A qualitative/quantitative judgment that combines:

- task completion;
- verification improvement;
- context reduction;
- repeated-read/search reduction;
- useful findings consumed by Main;
- extra model requests;
- extra tokens;
- extra wall time;
- duplicated work;
- child failures, ghost children or ignored child outcomes.

**No dedicated Spawn Reliability Gate starts before Batch #1 produces this evidence.**

---

# 3. DeepSeek Harness comparison: what it is good at

Reference project: [`deepseek-ai/deepseek-harness`](https://github.com/deepseek-ai/deepseek-harness), reviewed at `47f9438` (`dsh 0.1.0-rc.5`, developer preview).

DeepSeek Harness and CodeLeveler optimize for different first principles.

| Area | DeepSeek Harness | CodeLeveler |
| --- | --- | --- |
| Primary design goal | Maximum composability | Reliable, durable Coding execution |
| Core philosophy | Everything is a plugin | Stable authoritative Runtime Kernel |
| Composition | Cordis plugin tree, profiles, bundles, patches | Explicit Rust crates + app composition |
| Agent loop | Replaceable service/plugin | Narrow executor supervised by Engine |
| Session/runtime truth | Event-backed session model | Engine-owned Task/Turn/Event lifecycle + fencing/recovery |
| Tool extensibility | Mature capability seams and interception pipeline | Strong ToolHost/safety boundary; extension framework is still evolving |
| Subagents | Provider seam, capability negotiation, continuable children, reporting | Native `spawn_agent`, roles/limits, star topology |
| Browser | Not the differentiating architecture boundary | Production-gated daemon-owned structured Browser V1 |
| Product orientation | General Harness/framework | Coding-first Durable Agent Runtime |

DeepSeek Harness is ahead mainly in **composability** and **SubAgent abstraction**. CodeLeveler should not mistake that for a reason to weaken its stronger durability and safety invariants.

---

# 4. What CodeLeveler should adopt

## 4.1 Model-visible = durable provenance

### What it means

Any mutable information that reaches a model request must have a durable, reconstructable provenance.

Examples include:

- user messages;
- tool results;
- goal continuation context;
- injected runtime context;
- memory/context additions;
- reviewer findings;
- subagent reports;
- future extension-produced model context.

The forbidden shape is:

```text
process-local value
      |
      v
prompt/model sees it
      |
      X
no canonical provenance
```

The desired shape is:

```text
durable fact/source
      |
      v
context projection
      |
      v
model request
```

### Why it matters

Without this invariant, original execution, resume, replay and audit can observe different effective histories. That directly undermines CodeLeveler's recovery and evidence-backed completion goals.

### Decision

**ADOPT. P0/P1 Harness foundation.** Add explicit invariants/tripwires where practical; do not create a second context event system.

---

## 4.2 Explicit Tool Lifecycle Pipeline

### What it means

CodeLeveler already has most of the required mechanisms, but they should be documented and eventually exposed through a stable interception contract instead of growing ad hoc hook paths.

Target conceptual pipeline:

```text
model tool proposal
      |
      v
pre-execute / soft hooks
      |
      v
hard host guards
(permission/path/risk/sandbox/network/ownership/cancellation)
      |
      v
approval if required
      |
      v
execution
(timeout/retry/metrics where valid)
      |
      v
post-execute / result hooks
      |
      v
authoritative tool result
      |
      v
durable result/event
```

### Why it matters

MCP, built-in tools, future extensions, enterprise policy, Browser, SubAgent tools and future domain capabilities need one predictable execution boundary.

### Decision

**ADOPT. P0/P1 Harness foundation.** Preserve ToolHost as the unique model-proposed execution path; this is not permission to bypass the Engine/Execution boundaries.

---

## 4.3 Monotonic Safety

### What it means

Extension policy may preserve or tighten a host safety decision, but may never reopen something an earlier authoritative guard denied.

Allowed direction:

```text
allow -> restrict -> deny
```

Forbidden direction:

```text
deny -> later extension allow
```

### Scope

This applies to at least:

- path containment;
- permission/risk policy;
- sandboxing;
- network restrictions;
- ownership/fencing;
- cancellation/admission;
- future enterprise policy.

### Why it matters

A capability ecosystem is safe only if third-party/extensible behavior cannot weaken the Kernel's host-enforced boundary.

### Decision

**STRONGLY ADOPT. P0 Harness invariant.** Extension points are outside the hard safety boundary, not replacements for it.

---

## 4.4 Generated Architecture Catalogs

### What it means

Generate machine-derived documentation for stable registries/contracts where possible, for example:

```text
TOOL_CATALOG.md
EVENT_CATALOG.md
CAPABILITY_CATALOG.md
CONFIG_CATALOG.md
```

A tool catalog can record schema/name, risk class, owner/provider, mutation/parallelism semantics and presentation metadata without relying on hand-updated prose.

### Why it matters

As CodeLeveler grows, humans and coding agents need a reliable map of extension surfaces. Generated catalogs reduce documentation drift.

### Decision

**ADOPT. P1 engineering-quality work.** Generated catalogs complement architecture prose; they do not replace design documents.

---

# 5. Spawn decision gate

After R010, Batch #1 produces a branch decision.

```text
Batch #1 complete
      |
      v
Is Spawn Utility materially positive?
      |
      +-- NO --> improve Main Harness first; do not build a larger SubAgent framework
      |
      +-- YES --> open Spawn Agent Reliability Gate
```

This gate is important because architectural elegance is not evidence that delegation improves real coding outcomes.

---

# 6. Spawn Agent Reliability Gate

This gate is about lifecycle correctness, not merely whether a child can run a model.

Required classes:

- child start/publication atomicity;
- failure propagation;
- timeout behavior;
- parent cancellation and child cleanup semantics;
- ghost/orphan child prevention;
- concurrency and total-spawn limits;
- workspace/file ownership;
- duplicate work detection;
- child result delivery;
- ignored child failure detection;
- parent consumption of findings;
- reviewer independence;
- restart/recovery semantics where durability is claimed.

For Reviewer scenarios, the target observable flow is:

```text
Main implementation
      |
      v
independent Reviewer
      |
      v
structured findings
      |
      v
Main consumes/rejects with evidence
      |
      v
repair if needed
      |
      v
reverification
```

### Gate outcome

Only after spawn is both **useful** and **reliable** should the current one-tool abstraction evolve into a first-class SubAgent capability.

---

# 7. SubAgent Capability V2

## 7.1 Structured Child Result

### What it is

A child should return a typed result contract rather than only free-form prose.

Conceptual shape:

```text
ChildResult
|- status
|- summary
|- findings[]
|- evidence[]
|- verification[]
|- metrics
```

A reviewer finding may include file/line, severity, reason, evidence and confidence.

### Why it matters

It makes child value measurable and allows the runtime to distinguish:

- findings produced;
- findings consumed by Main;
- findings acted upon;
- findings verified.

That directly supports Useful Child Rate and Spawn Utility.

### Decision

**First SubAgent V2 upgrade if the spawn gate passes.**

---

## 7.2 Capability Profile

### What it is

Do not grow an ever-larger enum of hard-coded roles such as Reviewer/Tester/Security/Frontend. A child profile should describe the actual capabilities and contract.

Conceptual fields:

```text
persona
allowed/denied tools
workspace scope
write permission/file ownership
output contract
max depth
runtime/model budget
verification expectation
```

Examples:

- Explorer: read/search/LSP, no writes, exploration result schema.
- Reviewer: read/search/test, no writes, review finding schema.
- Worker: read/edit/shell, explicit owned file scope, implementation result schema.

### Why it matters

Behavior is expressed as capability, not accumulated product-specific role names.

### Decision

**ADOPT after structured child results.** Existing `explorer`/`worker` semantics can map into profiles compatibly.

---

## 7.3 Capability Negotiation

### What it is

Every SubAgent Provider advertises what it actually supports, for example:

```text
structured output
tool filtering
workspace scoping
persona
max depth
continuation
```

If a request requires a capability a provider does not support, the operation fails explicitly.

### Rule

**Fail loud; never silently degrade.**

A provider must not accept `tool_filter=required` and then ignore it.

### Decision

**ADOPT with provider abstraction.** Capability checking happens before child publication.

---

## 7.4 SubAgent Provider Seam

### What it is

Separate "what child capability is requested" from "who executes the child".

Target shape:

```text
SubAgent Runtime
      |
      v
Provider Registry
  |       |        |
Native  Codex  Claude Code
                  |
             future remote
```

Possible providers are examples, not required scope. Native CodeLeveler remains the first provider.

### Why it matters

The provider seam can eventually support model/tool specialization without changing parent orchestration. It also aligns with CodeLeveler's product thesis of using Harness behavior to reduce the practical gap between models.

### Decision

**ADOPT only after Batch evidence + Spawn Reliability Gate.** Do not add external providers merely to demonstrate abstraction.

---

# 8. Continuable SubAgent

A one-shot child remains sufficient for many coding tasks. Continuation is a later capability, justified only by real workflows that benefit from keeping child context alive.

## 8.1 Stable child identity

A continuable child has durable identity rather than only a process-local task handle.

This enables future semantics such as:

```text
spawn child
follow up later
cold resume after quiescence
inspect lineage
interrupt under explicit authority
```

## 8.2 One execution model, not a second Runtime

This is a hard constraint.

Forbidden architecture:

```text
Main Engine
SubAgent Engine
SubAgent Queue
SubAgent Scheduler
SubAgent Recovery
```

Required architecture principle:

```text
                 CodeLeveler Engine
                  /             \
           parent work        child work
                |                 |
            Agent Loop        Agent Loop
                \                 /
                 same ToolHost / execution invariants
```

SubAgent topology reuses Task/Session/event/recovery primitives. It does not create a parallel execution truth.

## 8.3 Followup and report channels

If continuation becomes real product scope:

- parent followup should enter the child's normal ordered input/turn path;
- child reports should have durable provenance;
- explicit child-authored reports must be distinguishable from runtime settlement notices;
- authorization must derive from durable/live lineage, not caller-supplied display metadata.

## 8.4 Durability difference from DeepSeek Harness

CodeLeveler should adopt stable identity, continuation and lineage ideas, but **must not weaken durability into best-effort settlement**. A claimed durable child must obey CodeLeveler's stronger canonical persistence/recovery semantics.

### Decision

**DEFER until one-shot SubAgent V2 is proven and real tasks justify continuation.**

---

# 9. Capability / Extension Framework

This is the broader Harness evolution after the SubAgent seam proves the pattern.

## 9.1 Capability seam

A capability has three logical roles:

```text
Definition -> Provider -> Consumer
```

Examples:

- Browser capability: BrowserRuntime provider -> `browser_*` tools.
- SubAgent capability: Native/external provider -> spawn/control tools.
- Filesystem capability: local/remote execution provider -> read/edit/search tools.

The exact Rust representation should be driven by dependency ownership, not by copying Cordis.

## 9.2 Allowed extension surfaces

Future extensions may be allowed to contribute narrowly scoped behavior such as:

- model-facing tools;
- context providers with durable provenance;
- soft pre/post tool hooks;
- SubAgent providers;
- verification policies;
- prompt sections;
- presentation metadata.

## 9.3 Kernel invariants remain privileged

Extensions must not replace or bypass:

- Engine lifecycle authority;
- canonical event truth;
- ownership/fencing;
- persist-before-side-effect ordering;
- ToolHost admission;
- host permission/sandbox/path/network safety;
- cancellation authority;
- evidence-backed completion.

### Decision

**ADOPT the capability-seam pattern; reject a fully dynamic plugin kernel.**

---

# 10. Agent Presets, not arbitrary Runtime patch trees

A future preset can package useful Harness behavior without letting users patch Kernel internals.

Examples:

```text
coding
fast
strict-review
frontend
unattended
```

A preset may select/configure:

- model/provider preferences;
- available tool families;
- child profiles;
- delegation policy;
- verification policy;
- Browser availability;
- budgets.

This borrows the useful composition idea from profiles/bundles while keeping runtime authority explicit.

### Decision

**OPTIONAL later.** Do not implement as arbitrary Engine/Storage/Agent-loop patch rows.

---

# 11. Durable Multi-Agent Runtime

Multi-agent becomes a first-class Runtime topology only after the preceding gates.

At that point the Runtime must be able to answer authoritatively:

- which children exist;
- who owns them;
- which parent/ancestor may follow up or cancel;
- which tools/workspace each child may access;
- which child modified what;
- which finding Main consumed;
- what happened when a child failed;
- what happens on parent/daemon restart;
- whether all terminal/settlement facts are durable.

Conceptual topology:

```text
                 Parent
          /        |        \
     Explorer   Reviewer   Worker
          \        |        /
             durable evidence
                   |
                   v
            verified completion
```

This is the point where CodeLeveler becomes a durable multi-agent Coding Harness, not merely a CLI that exposes `spawn_agent`.

---

# 12. Relationship to Cloud, Scheduler and NPC

Do not make Harness work create a parallel long-term product roadmap.

The existing [`RUNTIME_EVOLUTION_PLAN.md`](RUNTIME_EVOLUTION_PLAN.md) remains authoritative for:

- Control Plane;
- Cloud Worker;
- Local↔Cloud ownership transfer;
- Durable Scheduler;
- NPC Runtime.

The dependency direction is:

```text
reliable Runtime Kernel
      +
proven Harness/SubAgent capabilities
      |
      v
future durable multi-runtime products
```

NPC, Cloud and container deployments reuse the same Engine, ToolHost, safety, event and recovery model. They do not justify a second Agent core.

---

# 13. Explicit non-goals / rejected adoptions

The DeepSeek Harness comparison does **not** authorize these changes:

- **Do not adopt Cordis.**
- **Do not adopt `Everything is a Plugin` for the Runtime Kernel.**
- **Do not make Engine replaceable by ordinary extensions.**
- **Do not make safety/ownership/completion pluggable in a way that can weaken host authority.**
- **Do not create a second SubAgent Runtime/state machine.**
- **Do not make the Agent Loop arbitrarily replaceable as a prerequisite for extension work.**
- **Do not redo Browser Capability V1.**
- **Do not build a SubAgent Provider ecosystem before spawn utility is empirically positive.**
- **Do not create a parallel benchmark runner; keep using existing eval/dogfood infrastructure.**

---

# 14. Recommended execution sequence

```text
CURRENT
Real Usage Batch #1 (R001-R010)
        |
        v
Batch architecture review
        |
        +---------------------------+
        |                           |
        v                           v
Harness foundation             Spawn Utility?
(durable provenance,             /       \
 tool pipeline,                weak     useful
 monotonic safety,              |         |
 generated catalogs)            |         v
        |                        |   Spawn Reliability Gate
        |                        |         |
        |                        |         v
        |                        |   Structured Child Result
        |                        |         |
        |                        |         v
        |                        |   Capability Profile
        |                        |         |
        |                        |         v
        |                        |   Capability Negotiation
        |                        |         |
        |                        |         v
        |                        |   SubAgent Provider Seam
        |                        |         |
        |                        |         v
        |                        |   Continuable Child (only if justified)
        |                        |         |
        +------------------------+---------+
                                 |
                                 v
                     Capability / Extension Framework
                                 |
                                 v
                       Durable Multi-Agent Runtime
                                 |
                                 v
              existing Runtime roadmap: Cloud / Scheduler / NPC
```

The important ordering rules are:

1. **Evidence before architecture.**
2. **Reliability before abstraction.**
3. **Structured contracts before provider proliferation.**
4. **Capability seams outside a stable Kernel, not a plugin Kernel.**
5. **No second execution truth.**

---

# 15. Milestones and decision criteria

## M1 — Batch #1 complete

Answers what the current Harness actually fails at and whether delegation creates value.

Exit evidence includes:

- real task outcomes;
- false completion;
- rounds/tool overhead;
- Browser structured-call behavior;
- Verification Scope classification;
- Useful Child Rate;
- Spawn Utility.

## M2 — Harness Foundation hardened

Exit criteria:

- model-visible dynamic context has durable provenance rules;
- tool lifecycle/interception contract is explicit;
- safety composition is monotonic;
- generated catalogs cover selected stable registries.

## M3 — Spawn Reliability proven (conditional milestone)

Only exists if M1 says spawn is useful.

Exit criteria include no systemic ghost/orphan/failure-propagation/result-consumption defects across dedicated gates.

## M4 — SubAgent Capability V2

Exit criteria:

- typed child result contract;
- capability profiles;
- explicit provider capability negotiation;
- native provider behind the seam;
- no regression in parent-only tasks.

## M5 — Durable Multi-Agent

Exit criteria are defined by durable identity, lineage, ownership, recovery and evidence semantics, not by the number of simultaneously running agents.

---

# 16. Final architectural position

DeepSeek Harness is a useful reference because it demonstrates mature capability seams, a rich SubAgent abstraction and a highly composable Harness. CodeLeveler should absorb those **patterns selectively**.

CodeLeveler's differentiator remains different:

> **A reliable, durable, evidence-backed Coding Runtime whose Harness can become more composable without making runtime truth, safety or completion negotiable.**

The intended evolution is therefore:

```text
reliable Coding Agent
      -> evidence-driven Harness
      -> reliable SubAgent capability
      -> constrained Capability/Extension system
      -> durable multi-agent Coding Runtime
      -> broader Durable Agent Runtime products
```

Not:

```text
working Runtime
      -> copy a plugin framework
      -> weaken ownership/safety boundaries
      -> rebuild reliability later
```
