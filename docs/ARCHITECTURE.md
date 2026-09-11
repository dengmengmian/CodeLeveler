# CodeLeveler Architecture

The canonical architecture document. `AGENTS.md` states the constitution in
short form and points here; this file is where the boundaries are defined and
where the gap between the current implementation and the target foundation is
recorded honestly.

Chinese version: [`ARCHITECTURE.zh-CN.md`](ARCHITECTURE.zh-CN.md).

Everything below was verified against the workspace at the Foundation Freeze
baseline (`70e63900`), using `cargo metadata` and the crate sources. Where the
code does not yet match the target boundary, it says so rather than describing
the target as if it were already true.

---

## 1. Architecture principles

CodeLeveler used to be read as "a coding agent, and everything else beneath
it". That reading makes the coding product the top abstraction, and every new
capability ends up pushed down into shared crates to serve it.

The architecture is instead:

```text
Foundation provides reusable agent-runtime capability.
Harness defines domain semantics.
Product defines experience, composition and delivery.
```

Seven sentences carry the whole model:

```text
The Kernel does not know the product.
The Harness defines domain semantics.
The Harness exposes capability; it does not emulate intelligence.
The Engine owns lifecycle, not agent intelligence.
The Host Authority exclusively owns controlled side effects.
Every durable fact has one authoritative owner.
Mechanical Truth is not Semantic Satisfaction and is not User Acceptance.
```

Simplification means moving complexity to its correct owner. It never means
deleting reliability. `persist-before-forward`, the ownership fence,
compare-and-swap edits, stale-write protection, sandboxing, approval and crash
recovery all stay exactly as strong as they are.

### 1.1 Model capability policy

CodeLeveler used to carry a second goal alongside the architecture: flatten
model capability differences, and carry a weaker model with extra harness and
tool behaviour. **That goal is withdrawn.** It is not deferred and not
conditional.

```text
The model owns reasoning.
The Harness owns domain semantics and capability exposure.
The Runtime owns mechanical correctness.

CodeLeveler does not attempt to normalize model intelligence.
```

```text
                  Model
                    │
                    │ reasoning / planning / tool choice
                    ▼
                 Harness
                    │
                    │ domain semantics / tool surface
                    ▼
                 Runtime
                    │
                    │ deterministic execution / authority
                    ▼
                   Host
```

```text
Model intelligence is an input to the system, not a runtime invariant.
```

**What the system therefore owes the model.** Clear primitives, small
schemas, deterministic semantics, precise errors, bounded output, real
repository state, typed mechanical evidence, fast and reliable execution. Then
the model plans, navigates, chooses tools, edits, debugs and recovers from its
own mistakes.

**What it does not owe.** Guessing what a malformed argument meant, quietly
changing a tool's semantics after a failure, selecting a different tool because
the model chose badly, hidden task-solving retries, a duplicate tool whose only
purpose is to be easier for a weaker model, or a planning/review framework that
exists because the model cannot plan or judge its own next step. Mechanical
validation — JSON and schema checking, path normalization, a compatibility
alias, provider protocol adaptation — is not on this list and stays required.

**The line is ownership, not effort.**

```text
ENGINEERING FAILURE     → the Runtime handles it.
MODEL CAPABILITY LIMIT  → the model owns it.
```

Machine failure, concurrency, filesystem races, process death, protocol and
network failure, security risk and invalid host state are engineering failures.
Every reliability property in this document exists for them and none of it is
weakened here: `persist-before-forward`, F7 Grounded Authority, ToolHost
admission, permission, approval, the ownership fence, sandboxing, path safety,
CAS, stale-write protection, atomic mutation, rollback and its conflict
protection, crash recovery, cancellation, durability, process lifecycle, the
EvidenceLedger and multi-agent write safety.

**Provider capability is not model intelligence.** Tool-calling support,
streaming, reasoning transport, vision, structured output, forced tool choice,
context and output limits and wire format are protocol facts. They belong to
`leveler-model`, `leveler-protocol`, `leveler-provider` and capability
negotiation. Negotiate and report them honestly; if a required mechanical
capability is absent, disable the dependent capability and say so. Do not grow
a second behavioural path from a protocol gap — a provider without forced tool
choice is reported as such, not compensated for with an alternate reasoning
strategy.

**Errors are precise, not prescriptive.** Say what failed, why, which
mechanical constraint was violated, and what is actually available: *file is
not valid UTF-8*, *path does not exist*, *pattern is invalid regex*, *write
rejected because the observed version is stale*, *result exceeded the
configured limit*. Do not hand the model a multi-step recovery strategy unless
that strategy is the mechanical contract.

**Fallback is still allowed; semantic compensation is not.** A second
implementation that produces exactly the same contract, with equivalence
mechanically established, is ordinary engineering. A fallback that changes what
the call *means* — regex search degrading to a literal scan, a failed patch
becoming a different edit, a structured operation becoming a different solution
— is the thing that is forbidden.

**Do not encode intelligence classes.** `weak`, `strong`, `small`, `large` are
not foundation concepts. A configured model either has a required mechanical
capability or does not.

**The decision test.** A feature must justify itself by at least one of: it is
a canonical coding capability; it materially improves the interaction for
supported models; it provides deterministic runtime correctness; it provides
security or authority; it materially improves efficiency without changing
semantics; it answers a demonstrated product requirement. "Weaker models need
this", "this makes different models behave the same" and "this compensates for
model weakness" are not on the list, and are not admissible as a reason to keep
existing architecture either.

---

## 2. Layer model

```text
┌─────────────────────────────────────────────────────────────┐
│                         PRODUCTS                            │
│   CodeLeveler        Review Product        Future Product   │
│   app / cli / tui / web / remote / relay                    │
└──────────┬──────────────────────┬───────────────────────────┘
           │                      │
           ▼                      ▼
┌─────────────────────────────────────────────────────────────┐
│                        HARNESSES                            │
│   Coding Harness                 Review Harness             │
│   leveler-agent                  (future) leveler-review    │
│   ├─ tool host (admission)       ├─ its own tool host       │
│   └─ tool surface selection      └─ its own tool surface    │
└──────────┬──────────────────────────┬───────────────────────┘
           │                          │
           └────────────┬─────────────┘
                        ▼
┌─────────────────────────────────────────────────────────────┐
│                      AGENT RUNTIME                          │
│   leveler-agent-core                                        │
│   ToolRuntime { definitions, execute }  ← tool boundary     │
└─────────────┬──────────────────────┬────────────────────────┘
              │                      │
              ▼                      ▼
┌──────────────────────┐  ┌──────────────────────────────────┐
│ Persistent Runtime   │  │ Reusable Capabilities            │
│ engine / storage     │  │ context / project / vcs / lsp    │
│ lifecycle            │  │ browser / memory / skills / media│
└──────────┬───────────┘  └───────────────┬──────────────────┘
           │                              │
           └───────────────┬──────────────┘
                           ▼
┌─────────────────────────────────────────────────────────────┐
│                    HOST AUTHORITY                           │
│                  leveler-execution                          │
└──────────────────────────┬──────────────────────────────────┘
                           ▼
                    Operating System
```

The diagram is the real dependency shape. Two boundaries were open when this
document was first written; both have since closed, and §18 records how:

- **The engine sits below the harness, not above it.** The edge is
  `leveler-agent → leveler-engine`, and `leveler-engine` names no harness crate
  at all. See §14 and §18.1.
- **A tool is an adapter; it does not implement the capability.** Every tool
  now calls a named capability owner (`WorkspaceReader`, `WorkspaceEditor`,
  `CommandExecution`, `LspSessions`, …). `leveler-tools` still holds both
  halves, which §5.3 says is correct — capability is a responsibility, not
  necessarily a crate. See §5.2 and §18.3.

---

## 3. Foundation primitives

| Crate | Owns |
| --- | --- |
| `leveler-core` | Typed identifiers, timestamps, resource budgets, base traits. No internal dependencies. |
| `leveler-model` | The provider-neutral model vocabulary: `ModelRequest`, `ModelResponse`, `ModelEvent`, `ModelError`, and the `ModelRuntime` trait. |
| `leveler-protocol` | Vendor wire adapters (OpenAI Chat Completions shape, SSE decoding). Knows no transport and no agent. |
| `leveler-provider` | Provider configuration, model catalog, HTTP transport with retry, the `ProviderRegistry` implementing `ModelRuntime`. |
| `leveler-lifecycle` | The execution lifecycle vocabulary: `SessionStatus`, `TaskOutcome`, `VerificationStatus`, `TurnOutcome`, plus the Coding workflow types. No internal dependencies. |

These crates must not learn coding, review, finding, repository-workflow, TUI,
web, CLI or any other product concept. `leveler-model` used to — it carried a
table of Coding tool names — and no longer does (§18.6);
`crates/leveler-tools/tests/ownership_boundaries.rs` is the tripwire.

`leveler-lifecycle` already carries the split internally: its `runtime` module
is domain-neutral and its `workflow` module holds Coding vocabulary, with
`runtime` forbidden from referencing `workflow`. A future non-Coding domain
depends on `runtime` without pulling Coding semantics in.

---

## 4. Agent kernel

`leveler-agent-core` is the product-neutral agent kernel. Its only non-dev
dependency is `leveler-model`.

It owns:

```text
model ↔ tool loop        round management
streaming                budgets (rounds / tokens / cost / duration)
retry and backoff        usage and cost accounting
deadlines                cancellation
neutral stop reasons     one tool-dispatch seam
```

It owns none of:

```text
coding semantics      review semantics      repository semantics
prompt semantics      persistence           UI
what verification means                     task completion semantics
user acceptance       product policy
```

The whole contract between the kernel and its host is the `AgentHarness`
trait: the loop calls each seam at a fixed point in every round and the
harness answers with a `Flow` (continue, start the next round, or stop with
the harness's own outcome). Every seam except the two that name and run tools
has a neutral default, so a plain tool-calling agent is `BasicHarness` over a
`ToolRuntime` and nothing else.

The kernel never judges whether the model's work is good, complete or
acceptable. A run ends where the model stops, where the host stops it, or
where a mechanical limit stops it.

A scan of the crate for product vocabulary (`coding`, `repository`,
`permission`, `prompt`, `review`, `finding`, `verify`, `patch`, `filesystem`)
returns hits only inside doc comments, and every one of them is the crate
saying that concern belongs to somebody else. **The kernel is clean today.**

---

## 5. Tool architecture

### 5.1 The Foundation tool boundary already exists

An earlier version of this document named `leveler-tool-core` as the target: a
new crate holding the tool trait, schema, registry and dispatch contract,
extracted from `leveler-tools`.

**That target is withdrawn.** Re-reading the source shows the foundation tool
boundary is already there, and it is smaller than the proposed crate would
have been:

```rust
// leveler-agent-core::tool_runtime
pub trait ToolRuntime: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;
    async fn execute(&self, call: ToolCall, cancellation: CancellationToken)
        -> Result<ToolOutcome, ToolRuntimeError>;
}
```

Two methods. The crate's own comment states the rest of the contract: "the
kernel knows nothing about how a tool is authorized, sandboxed, or run …
admission, permission, workspace, side-effect durability — belongs to the
host."

So:

```text
leveler-agent-core::ToolRuntime  =  the Foundation Tool Boundary
```

A second harness implements those two methods and owns everything behind
them. It does not need `leveler-tools`, the `Tool` trait, or `ToolRegistry`.

> **Do not introduce a second generic tool core for architectural symmetry.**
> `leveler-tool-core` is a rejected proposal, not deferred work. Revisit it
> only if a real second consumer appears *and* `ToolRuntime` is demonstrably
> insufficient for it.

### 5.2 What a tool is

```text
A Tool is a model-facing adapter to a capability.
A Tool is not the runtime that implements the capability.
```

A tool owns:

```text
model-facing name, schema, description
argument decoding and compatibility repair
capability invocation
model-facing result rendering
the small intrinsic execution metadata mechanical correctness requires
```

A tool does not own:

```text
service discovery            permission resolution
global policy                approval orchestration
ownership management         durability
persistent state ownership   process lifecycle
workspace transaction runtime LSP lifecycle
browser lifecycle            provider configuration
runtime evidence storage     UI state
```

The rule in one line: **tool thin, capability thick.** "Thick" does not mean a
single large service. It means the domain implementation has a named owner
that is not the model-facing adapter.

```text
ReadFileTool      → WorkspaceReader
GrepTool          → WorkspaceSearch
ApplyPatchTool    → WorkspaceEditor
RunCommandTool    → CommandExecution
ShellCommandTool  → CommandExecution
FindSymbolTool    → LspSessions
GitStatusTool     → GitWorkflow
BrowserTabTool    → Browser
ViewImageTool     → leveler_media::process_image
MemoryTool        → MemoryStore
McpTool           → McpClient
WebSearchTool     → Tavily, keyed at construction
```

The arrow is a CONSTRUCTOR, not a lookup. Each tool holds the handle on the
left of its own arrow and nothing else — see §5.5.

### 5.3 Capability is a responsibility, not necessarily a crate

```text
Capability != crate
```

`WorkspaceReader`, `WorkspaceSearch`, `WorkspaceEditor`, `CommandExecution`
and `CodeIntelligence` are architecture responsibilities. They may be
implemented as a module, a struct, a subsystem of an existing crate, or a
service — whatever the code makes natural.

Do not create `leveler-workspace-search`, `leveler-workspace-editor` or
`leveler-code-intelligence` to make the diagram symmetric. A crate split needs
two real consumers, a real dependency inversion, an independent
security/runtime/protocol boundary, or an observed coupling defect. Prefer a
concrete struct over a trait until a second implementation exists.

All five are now real, and they are exactly that — concrete structs, no
trait, no registry, no new crate:

| Responsibility | Where it lives |
| --- | --- |
| `WorkspaceReader`, `WorkspaceSearch`, `WorkspaceEditor` | `crates/leveler-tools/src/workspace/` (crate-private) |
| `CommandExecution` | `crates/leveler-tools/src/tools/command_execution.rs` |
| `CodeIntelligence` | `leveler_lsp::LspSessions` — the crate that owns `LspClient` |
| VCS | `leveler_vcs::GitWorkflow` |
| Media | `leveler_media::process_image` |
| Browser | `leveler_browser::Browser`, over one `BrowserBackend` per protocol (CDP, WebDriver) |
| Memory | `leveler_memory::MemoryStore` |
| Web search | Tavily, called directly. One backend written down once, no seam in front of it (§18.3 G) |
| MCP | `leveler_tools::mcp::McpClient` |

`CommandExecution` stays inside `leveler-tools` rather than moving down to
`leveler-execution`, because it reads the per-call authority off `ToolContext`
and returns a `ToolOutput` — and `leveler-execution`, which sits below the
tool layer, must not depend on either. It is a module both command tools are
constructed with, and neither owns the other: `shell_command` used to import
`run_command`'s internals for its own runtime.

### 5.4 ToolHost admits; Host Execution performs

```text
ToolHost admits.
Host Execution performs.
```

The Coding tool host is `crates/leveler-agent/src/executor/host.rs`. It is the
one path from a model-proposed call to an execution, and the code enforces
that rather than documenting it:

```text
side-effect barrier → pre-hooks → permission rules → profile policy
→ auto-review / approval → barrier again → execution
```

Admission produces an `AdmittedCall`, the only value `dispatch` accepts —
executing without admission does not typecheck, and
`crates/leveler-agent/tests/tool_host_boundary.rs` fails if any other file in
the crate reaches `registry.execute` or the hook gate directly. The barrier
runs twice on purpose: the approval outcome must be durable before the side
effect it authorizes.

`leveler-execution` performs: filesystem enforcement, process execution,
sandbox, path safety, host process mechanics.

Neither `ToolRegistry`, nor a `Tool` implementation, nor a capability may open
a second permission or approval path. `ToolRegistry` used to open one — the
read-only overlay, the zero-write-authority refusal and the profile's hard
forbid were all re-decided there — so a build had three authorization owners
that had to agree. All three now live in `resolve_policy`, and
`crates/leveler-tools/tests/ownership_boundaries.rs` fails if any of them
reappears in the registry.

One authority the host cannot enforce is now refused instead of pretended: an
MCP tool is a separate, unsandboxed process, so a run with network access
denied refuses it rather than running it under a denial that cannot reach it.
The same reasoning already kept MCP away from a delegated agent, whose claimed
write scope it also could not honour.

### 5.5 ToolContext

```text
ToolContext = ExecutionResources + ToolPolicy + session_scope
```

- `ExecutionResources` — the execution substrate this call is anchored to: the
  workspace whose root the write scope resolves against, the process runner,
  the rollback checkpoint, the read fingerprints, the workspace-wide command
  gate. Process-wide `Arc`s, one instance for the whole run including
  sub-agents.
- `ToolPolicy` — the per-call authority: the live permission profile, the
  read-only overlay, the write allowlist and budgets, and the
  `ResolvedExecutionPolicy` the ToolHost froze at admission.
- `session_scope` — which session this call belongs to.

**It carries no capability handles.** There used to be a third facet,
`ToolServices`, holding the language-server pool, the browser runtime, the
memory root, the artifact store and the background task registry. Every tool
received all six: `read_file` was handed the browser, and `grep` could start a
language server. That is a service locator, and a locator answers everyone.

Every tool is now constructed with the handles it uses and no others:

| Tool | Constructed with |
| --- | --- |
| `find_symbol`, `read_symbol`, `find_references`, `diagnostics`, `blast_radius` | `leveler_lsp::LspSessions` |
| `run_command`, `shell_command` | the shared `CommandExecution` |
| `get_task`, `wait_task`, `kill_task` | `BackgroundTaskRegistry` |
| `memory`, `remember`, `forget` | the memory store root |
| `browser_tab`, `browser_act`, `browser_inspect` | `leveler_browser::Browser` |
| the core read/search/edit tools, `git_*`, `view_image`, `load_skill`, `web_*` | nothing but the context |

The handles reach the composer as `leveler_tools::Capabilities`, which the
composition root holds and `model_surface` consumes ONCE. A tool that has no
business with a capability has no way to reach it, and
`crates/leveler-tools/tests/ownership_boundaries.rs` fails if a handle
reappears on the context.

Two facts the RUNTIME needs in its own right moved to the runtime rather than
riding on a tool's handle: the executor holds the memory root it reads for
per-turn recall and for parking an unapproved `remember`, and
`ExecutorFactory` holds the background task registry the engine reaps at
terminal settlement.

**ANTI-GROWTH RULE.** A new top-level field is not allowed, and a new
capability is not a candidate for one — it goes to the tools that use it, at
construction. What may live here is per-call authority or per-call identity,
and it must name its owner in review.

### 5.6 ToolRegistry

```text
ToolRegistry
    register
    lookup
    definitions
    normalize_input
    schema validation
    dispatch
    one bounded result
```

`ToolRegistry::execute` normalizes the arguments, validates them against the
tool's JSON Schema, dispatches, and caps the result. It decides nothing about
whether the call may happen: possession of an `AdmittedCall` is what proves
that, and only the ToolHost can produce one.

Argument handling stays here, and that is not a policy: JSON and schema
validation, field aliases, path-syntax normalization, legacy compatibility
aliases, and a precise invalid-argument error are mechanical. What the
registry may never do is guess a malformed intent, substitute a different
tool, or change a call's meaning.

The bounded result is a mechanical runtime guarantee, not a budget a caller
negotiates: every dispatch is capped to the model's result budget, no tool can
opt out, and there is one such cap. A capability may still have an
INTRINSIC bound of its own — `run_command` spills oversized output to the
artifact store and hands back a recovery locator — and the two are different
things: an intrinsic bound is about what the capability can produce, the
central cap is about what the model's context can hold.

What left, and where it went:

| Concern | Owner now |
| --- | --- |
| read-only overlay, profile forbid, zero-write-authority refusal | ToolHost (`resolve_policy`) |
| which tools exist at all | the harness composition root |
| capability handles | each tool, at construction |
| harness controls | `leveler_agent::register_harness_controls` |

The registry must not become a policy engine again.
`crates/leveler-tools/tests/ownership_boundaries.rs` is the tripwire.

### 5.7 The tool layering, drawn

```text
                         MODEL
                           │
                           ▼
┌──────────────────────────────────────────────────┐
│                AGENT KERNEL                      │
│              leveler-agent-core                  │
│ model loop / retry / budget / cancel             │
│ ToolRuntime { definitions, execute }             │
└───────────────────────┬──────────────────────────┘
                        ▼
┌──────────────────────────────────────────────────┐
│               HARNESS TOOL HOST                  │
│ admission / authorization / approval             │
│ ownership / durability barrier                   │
│ execution scheduling / runtime evidence          │
└───────────────────────┬──────────────────────────┘
              ┌─────────┴──────────────┐
              ▼                        ▼
┌──────────────────────────┐  ┌──────────────────────────┐
│ Capability Tool Adapters │  │ Harness Control Tools    │
│ read_file  grep          │  │ update_goal              │
│ apply_patch run_command  │  │ update_plan              │
│ browser_*  git_*         │  │ request_user_input       │
│ …                        │  │ request_permissions      │
│                          │  │ spawn_agent              │
│                          │  │ claim_write_scope        │
│                          │  │ report_finding           │
└─────────────┬────────────┘  └──────────────────────────┘
              ▼
┌──────────────────────────────────────────────────┐
│                  CAPABILITIES                    │
│ Workspace Read/Search   Workspace Edit           │
│ Command Execution       Code Intelligence        │
│ Browser                 VCS      Memory          │
│ Skills   Media   Web/Search   MCP Runtime        │
└───────────────────────┬──────────────────────────┘
                        ▼
┌──────────────────────────────────────────────────┐
│               HOST EXECUTION                     │
│              leveler-execution                   │
│ filesystem / process / sandbox / path safety     │
│ process lifecycle / controlled host effects      │
└──────────────────────────────────────────────────┘
```

The harness-control column is separate in the code: those tools live in
`crates/leveler-agent`, not in the tool crate. Seven are answered inside the
loop from an injected `ToolDefinition` (`injected_tools.rs`); `update_plan` is
a registered `Tool` (`update_plan.rs`) because it has a real result to render,
and `register_harness_controls` puts it on the surface after the capability
packs. It reuses the registry's one mechanical seam — normalization, schema
validation, dispatch, the result cap — rather than reimplementing it.

---

## 6. Tool surface policy

### 6.1 The principle

```text
Expose the smallest tool surface that preserves capability.
```

Every model-facing tool costs schema tokens, tool-choice entropy, overlapping
semantics the model must disambiguate, routing mistakes, policy branches,
replay semantics and test surface. Capability count and model-facing tool
count are different numbers, and only the second one is a cost paid on every
round.

```text
Many capabilities  ≠  many tools visible to the model each round
```

A tool belongs on the surface when it answers yes to enough of these:

```text
Does it express a distinct model intent?
Does it expose a distinct capability?
Does it have deterministic semantics?
Does it remove round trips a competent model would otherwise pay?
Does it materially improve task success or efficiency?
```

and no to these:

```text
Does it overlap another tool and add tool-choice entropy?
Could an existing primitive express the same operation cleanly?
Does this belong to model control at all, or to the runtime or the user?
```

The objective is to **maximize useful model agency while minimizing harness
and tool complexity** (§1.1). "Does this help a weaker model?" is not a
criterion and is no longer asked.

### 6.2 What the model sees today

The surface is composed, not inherited. `leveler-app` builds it from what this
host can actually do, once, before the turn starts:

```text
CORE                 always
HARNESS CONTROLS     injected by the executor, each on its own condition
OPTIONAL PACKS       when the capability's precondition holds
EXTENSIONS           MCP tools from configured servers
```

| Set | Count |
| --- | --- |
| `core_surface(&capabilities)` | 11 |
| `model_surface(CapabilityPacks::ALL, &capabilities)` = core + 26 | 37 |
| the same, on a host with no search key | 36 |
| Harness controls (`update_plan` + 1–7 injected, by condition) | 2–8 |
| MCP extensions | as configured |

Both numbers matter. A pack needs the product to have asked for it AND the host
to be able to provide it, and for `web_search` "able to provide it" means
holding a key — so a key-less host composes 36, and `default_registry()`, whose
whole point is that it needs nothing from a host, is one of them.

A pack reaches the model only when the product mode ENABLED it and this host
can provide it (§6.4). Neither answer is ever about the task or the model:

| Pack | Tools | Available when | Enabled when |
| --- | --- | --- | --- |
| Code Intelligence | `find_symbol`, `read_symbol`, `find_references`, `diagnostics`, `blast_radius` | always — the scan fallback needs nothing installed | outside Economy |
| VCS | `git_status`, `git_diff` | `git` is on `PATH` | outside Economy |
| Web fetch | `web_fetch` | always | outside Economy |
| Web search | `web_search` | `LEVELER_SEARCH_API_KEY` holds a non-blank Tavily key | outside Economy |
| Media | `view_image` | the model's profile declares `vision` | outside Economy |
| Memory | `memory`, `remember`, `forget` | always — the app hands the tools a store root | outside Economy |
| Skills | `load_skill` | always | outside Economy |
| Browser | `browser_tab`, `browser_act`, `browser_inspect` (3) | the browser this host would SELECT — the call's, then `[browser].default`, then the system default — is one it can drive | outside Economy |

`WorkProfile::Economy` enables `CapabilityPacks::NONE`: the primitives and the
protocol, nothing else. That is a user's decision about cost, not an inference
about the task — and it holds however capable the machine is. An Economy turn
on a laptop with a browser runtime, a search key and a vision model still sees
eleven primitives plus the controls.

**Availability no longer implies exposure.** The two answers are separate
functions over separate inputs, and the intersection is the only way either
reaches the model:
`Application::capability_availability` (mechanical),
`Application::capability_selection` (product), `CapabilityPacks::intersect`.

**The browser is network-authorised by exposure.** Every other capability that
reaches the network re-checks `network_denied` inside `execute`, because for an
in-process `reqwest` call that check is the only enforcement point. The browser
does not, and the omission is the decision, not an oversight: the capability
exists to open real pages — a dev server, a staging host, a docs site — and
verify what the user's own browser would show. A browser that could navigate
only where a policy re-permitted it per request would be a different, much less
useful capability, and the enforcement to make that real (a forward proxy, DNS
pinning, per-page grants, request interception in two protocols) is a second
network runtime CodeLeveler would then own and maintain.

So the rule is stated once, at the surface:

```text
Browser Pack EXPOSED  =  browser network egress authorized
```

Localhost, LAN dev servers and the public internet are all reachable, and a
click that navigates, a page's `fetch`, a WebSocket and a subresource behave as
they do in the user's browser. One navigation-target rule survives, and it is
not a network boundary: `browser_tab navigate` refuses link-local and cloud
instance-metadata addresses, because pointing a browser at
`169.254.169.254` is a credential read wearing a URL. It does not, and does not
claim to, constrain what an already-open page does.

### 6.3 The Core Primitive Foundation

Seven operations are the architectural baseline. They are primitives because
they express fundamental coding operations, not because any model needs help
with them:

```text
read    ls    find    grep    edit    write    bash
```

Current CodeLeveler mapping:

| Primitive | Tool |
| --- | --- |
| `read` | `read_file` |
| `ls` | `list_files` |
| `find` | `find_files` |
| `grep` | `grep` |
| `edit` | `apply_patch` — the canonical structured edit |
| `write` | `write_file` — complete-file materialization |
| `bash` | `run_command` / `shell_command` |

`write_file` and partial edit are distinct model intents — create or
deliberately replace a whole file, versus change part of one — so both are
primitives. That distinction is stable and has nothing to do with whether a
model can construct a patch.

`core_registry()` and `full_registry()` were the historical answer to this
question and were not the same set. They are gone: the composition is now
`core_surface(&capabilities)` plus explicit `CapabilityPacks` (§6.2), so the
primitive baseline and the model-visible surface are stated separately and
neither is inferred from the other.

`get_task` / `wait_task` / `kill_task` are in the core surface too, and not as
a ninth primitive: `run_command` can start a background task, and a task the
caller can neither observe nor stop is an orphan. They are that primitive's
lifecycle. No harness control is in the core surface — a control is not a
capability a host could turn off, so `leveler_agent::register_harness_controls`
adds those separately (§18.5).

### 6.3.1 The five categories

Every model-facing capability ends in exactly one of these.

**CORE** — the Core Primitive Foundation above, plus what it entails:

```text
read_file  list_files  find_files  grep  apply_patch  write_file
run_command  shell_command
get_task  wait_task  kill_task
```

`run_command` can start a background task, and a task the caller cannot observe
or stop is an orphan — so the lifecycle trio is part of the command primitive,
not an optional capability.

**OPTIONAL CAPABILITY PACKS** — real capabilities with real preconditions,
composed by the harness from host facts (§6.2). Optional does not mean hidden
behind model-triggered discovery; it means the product exposes the pack when
the capability is there.

**HARNESS CONTROLS** — the Coding harness's control protocol, not reusable
capability. `crates/leveler-agent/src/injected_tools.rs` injects them around the
registry, each on its own condition:

```text
request_user_input (alias ask_user)   always
request_permissions                   mode != full-access
spawn_agent                           delegation configured, depth < max
claim_write_scope                     child turn with write access
report_finding                        child turn
update_goal                           goal mode
```

`update_plan` belongs here and is registered here (`leveler-agent`); §18.5.

**EXTENSIONS** — MCP-discovered tools, registered from configured servers.
This is the ONE extension boundary, and it is a process boundary on purpose:
an MCP server runs in its own process, speaks JSON-RPC over stdio, and reaches
the model only as an `McpTool` adapter that the ToolHost admits like any other
call. There is no native plugin SDK and none is planned — a third-party
in-process plugin would be code inside the runtime with the runtime's
authority, which is exactly what admission exists to prevent.

What the boundary costs, stated plainly: an MCP server is outside the OS
sandbox and outside any claimed write scope, so two authorities cannot be
enforced on it, and in both cases the call is REFUSED rather than run under an
authority that does not reach it — a delegated agent may not use MCP at all,
and a run with network access denied may not either. Under a confined profile
every MCP call needs approval; standing trust is a permission rule, never a
configuration default.

**NOT A MODEL TOOL AT ALL** — these were implemented, taken off the surface in
W1, and deleted here. An unregistered `Tool` implementation is a second answer
waiting to be re-registered:

```text
create_checkpoint / restore_checkpoint   the runtime checkpoints before every
                                         write and owns rollback and recovery
consolidate_memory                       memory-subsystem maintenance
create_skill                             system customization; the user makes
                                         skills, the model loads them
```

Whether both `run_command` and `shell_command` stay is a semantic question
about distinct intent and analysability (§6.5), decided in favour of both.

### 6.4 Who decides the surface

```text
The Harness decides what tools exist for the product.
```

Three different questions, kept apart:

```text
AVAILABLE   can this MACHINE provide the capability at all?
            a browser runtime installed, a search key configured, git on
            PATH, a model that accepts an image

ENABLED     does the current product mode / session ASK for it?

EXPOSED     what the model actually sees = ENABLED ∩ AVAILABLE
```

Being available buys nothing on its own. `Economy` enables no optional pack,
so a machine with a browser runtime installed still shows a plain Economy turn
zero browser tools. And asking for something the machine cannot do buys
nothing either: a host with no search key exposes no `web_search` however much
the product mode wants one.

Neither side can widen the other, which is what makes the intersection a
boundary rather than a suggestion
(`crates/leveler-tools/tests/capability_composition.rs`).

In the code: `Application::capability_availability` answers the first question
from mechanical facts alone, `Application::capability_selection` answers the
second from the work profile, `CapabilityPacks::intersect` produces the third,
and `model_surface(packs, &capabilities)` composes it once, before the turn
starts. `register_harness_controls` then adds the controls, which are not a
capability a host could turn off.

None of the three consults the model's ability or the task's difficulty
(§1.1). `expand_tools` inverted this ownership and is deleted; the
architectural objection stood on its own, and the implementation turned out
never to have worked (§6.5).

### 6.5 Surface value is an evaluation decision, not an aesthetic one

```text
No tool is removed merely because another tool could theoretically
reproduce it.
```

Remove or demote a tool when evidence shows: no measurable success
improvement, significant semantic overlap, extra routing errors, unnecessary
complexity, or that the capability belongs to the runtime or the user rather
than to the model.

This is about *product surface*, not about implementation correctness. A tool
whose implementation is mechanically wrong — non-deterministic semantics, a
second path to the filesystem, environment-dependent meaning — is fixed
directly; see §6.6.

Dispositions, and the evidence behind each. The usage numbers are from
`evals/baselines/tool-surface-t0-e623f53/`: 1725 tool calls across 99 sessions,
one model, one profile — strong enough to reject a claim, too weak to establish
one.

| Tool | Disposition | Why |
| --- | --- | --- |
| `expand_tools` | **DELETED** | Not a surface judgement: it could never work. Nothing consumed its `expand_categories` metadata, the registry it claimed to grow is an immutable `Arc` and the definitions are snapshotted once per drive, so the promise "the host will register matching tools" was unbacked. It also advertised `mcp` and `subagent`, which registered nothing, and rejected `browser`, the one category with an implementation. One call in the whole evidence set, and it failed. |
| `replace` | **DELETED** | Zero calls in 1725, including through six `apply_patch` context-matching failures — the exact case it was built to absorb, where the model retried `apply_patch` every time. Its weak-model rationale is withdrawn (§1.1), and `edit` + `write` cover the intent. |
| `read_symbol` | **OPTIONAL + EVAL_LOCKED** | Zero calls, but so are `find_symbol` and `find_references`, which suggests the language server never came up rather than that this tool is redundant. Off the default surface as part of the Code Intelligence pack; A/B-READ-SYMBOL is still owed. |
| `blast_radius` | **OPTIONAL + EVAL_LOCKED** | An advanced derived operation, not a primitive. Same evidence problem as `read_symbol`; same disposition. |
| `create_checkpoint` / `restore_checkpoint` | **DELETED** | Ownership, not usage: the runtime already checkpoints before every write and owns rollback and crash recovery. Asking the model when to checkpoint duplicates a runtime facility. Zero calls agrees. W1 took them off the surface and left the code; the code is gone now — an unregistered `Tool` implementation is a second answer waiting to be re-registered. |
| `consolidate_memory` | **DELETED** | Subsystem maintenance, not an agent tool. Off the surface in W1, deleted here, along with the `extract_memory_candidates` heuristic whose only caller it was. `remember` / `forget` remain the model-facing memory writes, and both stay consent-gated. |
| `create_skill` | **DELETED** | System customization. `load_skill` stays; authoring a skill is the user's act, through the CLI. |
| `forget` | **OPTIONAL (Memory pack)** | Destructive, but consent-gated: it raises an approval prompt, and correcting a memory the repository has outgrown is part of the work. Classified per operation, not per crate (§25). |
| `shell_command` vs `run_command` | **BOTH STAY** | Distinct model intents, not two spellings of one. `run_command` is program+args — cross-platform, no shell quoting, trivially analysable for approval, and the only one that can background. `shell_command` is a shell line, which is what a pipeline or an `&&` chain actually is. Both now run on one `CommandExecution`; the background asymmetry is deliberate and §18.3 D says why. |
| `git_status` / `git_diff` | **OPTIONAL (VCS pack)** | Reproducible via `run_command`, but git inspection is high-frequency, needs no shell and replays cleanly. The interface stayed; the implementation moved to `leveler_vcs::GitWorkflow::inspect`, so there is one place that knows how this product invokes `git`. |

`list_files` and `find_files` are deliberately **not** merge candidates. They
express different model intents — "what is in this directory" versus "where in
the repository does this pattern exist". What should be unified is the
filesystem traversal and ignore semantics beneath them.

### 6.6 Two different questions, two different burdens of proof

The earlier rule — measure before touching anything — was too broad. It forced
a usage-baseline ceremony onto refactors that are settled by reading the code.

```text
Architecture correctness is established from mechanical evidence.
Product-surface value is established with Eval, when the value is
genuinely uncertain.
```

**Fix directly, no A/B required.** A defect proven by inspection is a defect:
wrong ownership, non-deterministic or environment-dependent semantics, a
duplicate runtime implementation, cross-platform divergence, service-locator
coupling, a runtime capability living inside a tool adapter. Refactoring these
changes what the code *is*, not what the product *offers*.

**Measure first.** When two legitimate primitives overlap, when an optional
capability's value is unclear, or when removing a tool could materially change
product behaviour, take a usage baseline from real sessions: which tools are
called, at what success and retry rate, and whether their presence improves
task success. Of the original priority list, `replace`, `expand_tools` and
the checkpoint tools were settled by ownership or by a mechanical defect rather
than by measurement (§6.5); `read_symbol` and `blast_radius` still owe theirs.

---

## 7. Host execution authority

`leveler-execution` is the host side-effect authority. It owns:

```text
workspace path resolution and enforcement     process execution
permission profiles and rules                 process-tree termination
approval policy and approvers                 sandbox backends
risk classification                            hooks
checkpoints for rollback                       trust gating
artifact storage for oversized output          background task registry
```

The principle:

```text
An agent requests a side effect.
The host authority performs it.
```

### Verified state

Counting production (non-test) call sites of `fs::write`, `fs::remove`,
`fs::create_dir`, `fs::rename`, `Command::new` and `tokio::process` outside
`leveler-execution`:

| Crate | Production sites | Reading |
| --- | --- | --- |
| `leveler-agent` | 0 | The Coding harness performs no side effect directly. |
| `leveler-context` | 0 | Read-only assembly. |
| `leveler-vcs` | 0 | Every git invocation goes through the execution runner. |
| `leveler-tools` | 13 | `workspace/editor.rs` writes through the workspace root fd (symlink-safe). `mcp.rs` spawns configured MCP servers directly. |
| `leveler-browser` | 3 | The browser process is spawned directly (Unix process group, Windows Job Object); the profile directory is created under the Leveler home. |
| `leveler-memory` | 11 | Writes the memory store under the Leveler home. |
| `leveler-lsp` | 4 | Spawns language servers directly, from the session pool that owns their lifetime. |
| `leveler-engine` | 3 | `git rev-parse` for the baseline commit. |
| `leveler-skills` / `leveler-project` | 2 / 1 | Create state directories under the Leveler home. |

Two distinct things sit in that table:

1. **Model-requested effects on the user's repository.** These all go through
   `Workspace` and `CommandRunner`. `leveler-agent` reaching zero is the
   meaningful number.
2. **The runtime's own state and sidecars.** Writes under the Leveler home and
   long-lived sidecar processes (MCP servers, language servers, the browser
   driver) do not pass through the permission/approval path, because they are
   not something the model asked for.

That second category is a real, deliberate boundary — but the sidecars are
outside `CommandRunner`'s process-tree termination and sandbox semantics. See
§18.4.

---

## 8. Reusable capabilities

| Crate | Capability |
| --- | --- |
| `leveler-context` | Bounded repository context assembly: map, candidate files, related tests, merged project rules, token estimate, repeated-read guard. |
| `leveler-project` | Project language detection and filesystem layout. |
| `leveler-memory` | Durable project memory store and its promotion pipeline. |
| `leveler-skills` | Skill discovery and loading. |
| `leveler-vcs` | Git operations, performed through the execution authority; `GitWorkflow::inspect` is how the read-only git tools invoke `git`, on the caller's runner. |
| `leveler-lsp` | Language-server sessions (`LspSessions`), reused across tool calls; the one owner of client lifetime, startup and dead-server eviction. |
| `leveler-browser` | Browser product selection, refs and tab ownership, and the two protocol adapters (CDP for Chrome/Edge/Chromium, WebDriver for Safari). |
| `leveler-media` | Content-typed image import: real MIME from content, decode and pixel limits, EXIF stripping, downscaling, content-addressed storage. |

A Coding harness selects context, project, VCS, LSP, browser, memory,
filesystem mutation and process execution. A Review harness would plausibly
select context, project, VCS, LSP, read-only filesystem and memory, and skip
the rest.

`leveler-media` has two consumers, `leveler-app` and `leveler-tools`:
`view_image` calls it rather than reimplementing image handling. See §18.3.F.

---

## 9. Persistent runtime

`leveler-engine` is the persistent runtime. It owns:

```text
session and task lifecycle       checkpoint and resume
turn boundaries                  crash recovery and reaping
event ordering                   ownership registry
append-only event log            runtime outcome
persist-before-forward           context window policy
```

`persist-before-forward` is the guarantee that matters: a turn's events reach
the log in emission order before any client sees them, so a client can never
observe a fact the runtime has not durably recorded.

The engine should not be the agent brain. It should not own coding prompts,
review prompts, tool selection, repository strategy, or what completion means
in a domain.

The engine's own boundary work is partly done. `TaskSpec` is already split:

```rust
pub struct TaskSpec {
    pub runtime: RuntimeTaskSpec,   // goal, kind, continuation, limits
    pub coding: CodingTaskSpec,     // repository, permission mode, sandbox,
                                    // verification plan, base commit
}
```

The crate's own comment calls this "the migration seam toward a domain-neutral
engine". W3 completed that migration: the edge is reversed and the engine
names no harness crate — see §14 and §18.1.

---

## 10. Storage and durable truth

`leveler-storage` is the durable truth boundary: SQLite, embedded migrations,
the connection pool, and one repository per concern. Business logic never
issues SQL directly.

```text
A persistent fact
    → has one authoritative owner
    → has one canonical durable representation
```

This applies with no exceptions to task status, turn status, evidence,
ownership, usage, artifacts and completion state.

`leveler-storage` depends only on `leveler-core` and `leveler-lifecycle`. That
is what lets the low-level persistence crate speak the lifecycle vocabulary
without a back edge to a high-level crate.

---

## 11. Verification and evidence

`leveler-verifier` runs the project's declared checks — format, build, test —
captures evidence, checks scope, and classifies failures.

```text
The verifier is the authority for VERIFICATION VERDICTS.
The verifier is NOT the authority for semantic task completion.
```

It can prove that the configured checks passed, failed, or were blocked. It
cannot, alone, prove that the user's intent was satisfied.

A verification command a user declared explicitly is authority, not a
heuristic input.

This is why `TaskOutcome` and `VerificationStatus` are separate axes in
`leveler-lifecycle`. `TaskOutcome::Completed` means the model declared the goal
complete; `VerificationStatus` says what the project's own checks reported.
The runtime reports both and never folds them into one word.

The crate-level doc comment in `leveler-verifier/src/lib.rs` said otherwise
until it was rewritten to match the axes the code already separates. See
§18.8.

---

## 12. Harness layer

`leveler-agent` is the **Coding Harness**. The crate name has not changed and
this document does not propose changing it; the concept is what matters.

It owns Coding domain semantics:

```text
coding prompt                     compaction strategy
repository context strategy       goal semantics
coding tool selection             coding verification policy
tool admission (the ToolHost)     delegation policy and sub-agent profiles
write workflow                    coding completion contract
ownership of paths across agents  the harness control tool surface
```

### 12.1 What the harness tells the model, and what it does not

The system prompt is a contract, not an operating manual. It carries four
things, and the test for each is whether the model could know it any other way:

```text
identity and the authority boundary   who outranks what, what an edit tool
                                      guarantees that a shell does not, what a
                                      denied approval means
runtime state                         cwd, permission mode, network, project
                                      rules, the workspace listing
harness protocol                      how a decision reaches the user, how a
                                      goal ends, how delegation and ownership
                                      work, how memory consent works
product constraints                   the user's language, message length, no
                                      duplicated narration, no process closeout
```

It does not carry a method. Removed in W1: when to make a plan and how to keep
it synchronized; a required verification step before declaring completion; a
progress-narration template (`current step k/n · evidence → next`); which tool
to prefer for which shape of work; how to investigate a question; how to
diagnose a failure; when to persist and when to stop retrying. Each was the
harness reasoning on the model's behalf.

The same line applies to what the loop injects mid-turn. Protocol repair stays
— an unresolved goal, a malformed tool call, a truncated response, a settled
child, a discovered rules file, a budget position — because each states a
mechanical fact and names the operation that resolves it. Advisories that read
the model's reasoning are gone: the plan nudge, the plan-freshness reminder,
and the identical-call loop guard, whose payload was "do something different".

What bounds a runaway loop is unchanged and unconditional: the round ceiling,
the token, cost and duration budgets, the wall clock, cancellation, and the
no-progress stop when every call in consecutive rounds is refused.

**Plan capability, not plan enforcement.** `update_plan` is available every
turn, its state is persisted, and the UI renders it. Nothing classifies the
task, nothing counts rounds without a plan, and nothing asks for one.

It reaches the kernel through one seam: `Drive` in
`src/executor/drive.rs` implements `leveler_agent_core::AgentHarness`, filling
`tool_definitions`, `on_round_start`, `on_round_admitted`, `on_response`,
`on_model_error`, `on_quiet`, `execute_calls`, `on_stop` and `on_event`. That
is the entire kernel contract, already exercised by a real harness.

A future Review harness is a **sibling**:

```text
              leveler-agent-core
                /            \
               ▼              ▼
        leveler-agent    leveler-review
        Coding Harness   Review Harness
        own ToolHost     own ToolHost
        own tool surface own tool surface
```

The edge `leveler-review → leveler-agent` is forbidden, and reusing the Coding
tool plumbing is not required — Review implements `ToolRuntime` its own way.

Review would own its own vocabulary — `ReviewTarget`, `ReviewScope`,
`ReviewPolicy`, `Finding`, `FindingSeverity`, `FindingEvidence`,
`FindingLifecycle`, deduplication, suppression, `ReviewVerdict`,
`ReviewReport` — and none of those types may enter the agent kernel.

None of this is a commitment to build a Review harness. It is a constraint on
what the foundation is allowed to assume.

---

## 13. Product layer

| Crate | Role |
| --- | --- |
| `leveler-app` | The composition root: configuration, provider registry, database, event projection into client events. |
| `leveler-cli` | Command-line surface. |
| `leveler-tui` | Terminal client. Depends only on the client protocol, core, model and skills. |
| `leveler-web` | Web client surface. |
| `leveler-client-protocol` | The stable UI↔runtime contract: `ClientCommand` in, `RuntimeEvent` out, versioned envelope. |
| `leveler-local-transport` / `leveler-remote-protocol` / `leveler-remote-agent` / `leveler-relay` | Local and remote transports and the pairing/relay path. |
| `leveler-eval` | Capability evaluation harness. No internal dependencies. |
| `leveler-test-support` | Shared test fixtures. Dev-dependency only. |

These layers project authoritative runtime state. They do not derive new
runtime truth. A tool call returning `Ok` does not let a client conclude that
a task is complete.

---

## 14. Dependency direction

Target rule:

```text
Foundation
    ↑
Capabilities / Runtime
    ↑
Harnesses
    ↑
Products
```

Current graph, by topological level (normal dependencies only):

| Level | Crates | Internal dependencies |
| --- | --- | --- |
| 0 | `leveler-core`, `leveler-lifecycle`, `leveler-memory`, `leveler-media`, `leveler-eval` | none |
| 1 | `leveler-model`, `leveler-project`, `leveler-skills`, `leveler-browser`, `leveler-execution` | `core` |
| 1 | `leveler-storage` | `core`, `lifecycle` |
| 2 | `leveler-agent-core` | `model` |
| 2 | `leveler-protocol`, `leveler-client-protocol` | `core`, `model` |
| 2 | `leveler-context` | `core`, `project`, `skills` |
| 2 | `leveler-lsp` | `core`, `project` |
| 2 | `leveler-vcs` | `core`, `execution` |
| 2 | `leveler-verifier` | `core`, `execution`, `lifecycle`, `project` |
| 2 | `leveler-test-support` | `core`, `model` |
| 3 | `leveler-engine` | `context`, `core`, `execution`, `lifecycle`, `model`, `storage` |
| 3 | `leveler-provider` | `core`, `model`, `protocol` |
| 3 | `leveler-tools` | `browser`, `context`, `core`, `execution`, `lsp`, `media`, `memory`, `model`, `project`, `skills`, `vcs` |
| 3 | `leveler-local-transport`, `leveler-remote-protocol`, `leveler-tui` | client protocol and below |
| 4 | `leveler-agent` | `agent-core`, `context`, `core`, `engine`, `execution`, `lifecycle`, `memory`, `model`, `skills`, `storage`, `tools`, `verifier` |
| 4 | `leveler-relay`, `leveler-remote-agent`, `leveler-web` | remote / client protocol and below |
| 5 | `leveler-app` | 18 internal crates |
| 6 | `leveler-cli` | 21 internal crates |

Findings:

- **No reverse dependency exists.** Nothing depends on `leveler-app`,
  `leveler-cli`, `leveler-tui` or `leveler-web`.
- `leveler-agent-core` depends on exactly one internal crate.
- `leveler-agent → leveler-execution` is a **vocabulary** edge: the harness
  uses `PermissionProfile`, `RiskLevel`, `WriteScope` and `HookRunner` as
  types. Its direct side-effect count is zero.
- **The engine sits below the harness.** `leveler-agent → leveler-engine` is
  the only edge between them, and `leveler-engine` names no harness crate at
  all. `crates/leveler-engine/tests/ownership_direction.rs` reads the engine's
  own manifest and every engine source and fails the build if the edge
  returns — by manifest, by rename, or by one convenient import.

---

## 15. Runtime turn flow

```text
client command
    │
    ▼
leveler-app                  composition; config to a running Application
    │
    ▼
leveler-engine               turns row, turn-id stamping, the
    │                        persist-before-forward EventLog, approver and
    │                        clarifier wrapped as recorders
    ├─ ExecutorFactory       one derivation of the execution configuration
    ▼
leveler-agent (Drive)        Coding harness: prompt, context, tool surface,
    │                        delegation, compaction, goal semantics
    ▼
leveler-agent-core           the loop: admit round → assemble model round →
    │                        stream → parse → ToolRuntime::execute → repeat
    ▼
leveler-agent (ToolHost)     barrier → hooks → rules → policy → approval →
    │                        barrier → AdmittedCall
    ▼
leveler-tools                the adapter runs
    │
    ▼
leveler-execution            workspace resolution, permission, approval,
    │                        risk, sandbox, process execution
    ▼
operating system
```

And back out:

```text
events → EventLog (persisted first) → engine events → leveler-app
       → client events → leveler-client-protocol → TUI / web / remote
```

Facts flow out only after they are durable.

---

## 16. Truth and authority model

```text
Mechanical Truth  ≠  Semantic Satisfaction  ≠  User Acceptance
```

**The runtime authoritatively proves:** a command executed, its exit code, a
file was mutated, a test result, a build result, an artifact exists, an event
was persisted, a tool returned this result, observed runtime state.

**The verifier authoritatively decides:** the configured verification verdict.

**Neither implies the next step.**

```text
"the tool succeeded"  does not prove  "the goal is semantically satisfied"
"the tests passed"    does not prove  "the user's request is fulfilled"
```

```text
Runtime owns mechanical truth.
Model owns semantic interpretation.
User owns acceptance.
```

Do not reintroduce a mechanical shortcut that stands in for semantic
completion. No such predicate exists in the codebase today, and `TaskOutcome`
/ `VerificationStatus` being orthogonal axes is what keeps it out.

A corollary for tools: what the model reads and what the runtime records are
different things, and neither may silently become the other. A tool that
rewrites bytes into different text before showing them to the model has
changed the fact, not just the presentation (§18.3.A).

---

## 17. The Second Harness Test

> Can a semantically different agent product — Review, for instance — be built
> on this foundation **without modifying the agent kernel**?

Target answers:

| Question | Required answer |
| --- | --- |
| Modify `leveler-agent-core` | NO |
| Reuse `ToolRuntime` | YES |
| Reuse the model contracts | YES |
| Reuse the persistent runtime | YES |
| Reuse selected capabilities | YES |
| Reuse the Coding `ToolRegistry` | NOT REQUIRED |
| Reuse the Coding `Tool` trait | NOT REQUIRED |
| Depend on `leveler-tools` | NOT REQUIRED |
| Depend on `leveler-agent` | NO |

The core requirement:

```text
Review must be able to build its own harness tool host and its own tool
surface over the same agent kernel.
```

The foundation does not require every harness to use the same tool plumbing.

### Current verdict: ENFORCED

The kernel and the tool boundary passed already. `leveler-agent-core` depends
only on `leveler-model`, carries no product vocabulary, and `ToolRuntime` is
two methods a Review harness can implement its own way. `leveler-model` knows
no Coding tool name (§18.6), and composing a surface requires none of the
Coding tool plumbing: a harness registers its own tools, and
`register_harness_controls` shows the shape.

What closed the last of it:

- W3 put the harness above the engine. `leveler-engine` depends on no harness
  crate, and its public API names no type defined by one: a turn is a closure
  over `TurnPorts` that returns `TurnFacts<T>`, and `T` is whatever the harness
  returns. The engine records the mechanical facts beside it and reads none of
  it (§18.1).
- The engine's own `git` spawns went with it. Repository facts arrive through
  the `WorkspaceFacts` port, which the Coding harness implements as
  `GitWorkspace` (§18.9).

The proof is mechanical rather than architectural.
`crates/leveler-engine/tests/minimal_harness.rs` drives a whole session from a
harness that is not this product: create, turn, persist, crash, reap, resume,
terminal. Its payload is a string, it owns no repository, it runs no tool, and
its test binary cannot link `leveler-agent` — the engine's dev-dependencies do
not reach it. Standing it up required no new engine API, no new trait, no new
crate and no new abstraction of any kind, which is the part that carries the
weight: an engine that needed a seam invented for its second consumer would
not have been general, only accommodating.

**A second harness PRODUCT still does not exist.** The minimal harness is a
test. It proves the boundary holds; it claims nothing about a Review product
nobody has built, and building a demo harness product to validate the design
would still be a fake consumer (§5.3).

**Do not modify code to convert this verdict as part of a documentation
change.**

---

## 18. Known boundary debt

Recorded, not hidden. Each item states the current behaviour, the desired
boundary, why it violates the constitution, the minimal correction, and the
risk.

### 18.1 The engine depended on the Coding harness (closed)

**Was.** `leveler-engine → leveler-agent`. The engine's public API exported
`CodingTaskSpec`, `ExecutorFactory` constructed a `leveler_agent::Executor`
directly, and `recorders.rs`, `recovery.rs`, `turn.rs` and
`policy_resolver.rs` all named `leveler_agent` types. A second harness
inherited the Coding harness through the engine (rules 5 and 2).

**Now.** The edge is reversed: `leveler-agent → leveler-engine`, and the
engine's manifest names no harness crate. What runs inside a turn arrives as
a closure over `TurnPorts` and reports a `TurnFacts<T>` the engine carries
without reading. `ExecutorFactory` moved up into the harness
(`leveler-agent/src/coding/factory.rs`), which is where the single derivation
of execution configuration belongs.

**Closed by.** W3 (engine ↔ harness decoupling), accepted in W4.

**Tripwires.** `crates/leveler-engine/tests/ownership_direction.rs` fails the
build if the engine's manifest or any engine source reaches for
`leveler-agent`, `leveler-tools` or `leveler-verifier`.
`crates/leveler-engine/tests/minimal_harness.rs` runs a full session from a
harness that is not this product.

### 18.2 ToolContext was a universal service locator (closed)

**Was.** Every tool received `ExecutionResources + ToolPolicy + ToolServices +
session_scope`, with `ToolServices` naming `lsp_sessions`, `lsp_start_locks`,
`artifact_store`, `memory_root`, `background_tasks` and `browser` as fields.

**Now.** `ToolServices` is deleted. Every tool is constructed with the handles
it uses; `ToolContext` carries the execution substrate, the per-call authority
and the session identity, and nothing else. See §5.5 for the shape and the
per-tool table, and `crates/leveler-tools/tests/ownership_boundaries.rs` for
the tripwire.

**Still open.** `ExecutionResources` remains a shared facet — workspace,
runner, environment, checkpoint, file fingerprints, command gate. Those are
the substrate a call is anchored to rather than services a tool discovers, and
the write scope resolves against the workspace root, so they were left where
they are. Whether the runner and the command gate should reach only the
command tools is a real question and a smaller one; it is not a service
locator either way.

### 18.3 Concrete tool implementation debts

Each of these is a tool implementing capability behavior it should be calling.
A–C and H were closed by the Core Primitive Foundation work; D, E and F are
closed now; G is open by decision, not by omission. Each is kept with what was
actually done and with what is still open inside it.

#### A. `read_file` still carries the stale-write observation (mostly closed)

`crates/leveler-tools/src/tools/read_file.rs` is now a model-facing adapter:
schema, rendering and paging copy. Reading itself belongs to
`leveler-tools::workspace::WorkspaceReader`, which returns bounded content plus
a `ReadObservation`.

Closed:

- **Size no longer refuses a read.** The 10 MB cap is gone, and the tool no
  longer points the model at `sed`/`head`/`tail` — a bounded window of a 100 MB
  file is an ordinary call, and paging is stated in the tool's own contract.
- **Invalid UTF-8 is reported, not rewritten.** Every line of the file is
  decoded; the first invalid line is an explicit error naming the line number,
  and NUL bytes are reported as "not a text file". Nothing reaches the model as
  `U+FFFD` pretending to be the file's text.
- **Repeated-read policy is gone.** `RepeatedReadGuard` and its config plumbing
  are deleted. How often a model re-reads a range is a judgement about its
  reasoning, and the harness does not make it (§1.1).
- **Result-budget policy is explicit, not implicit.** The reader is told how
  many bytes the caller can render, including the caller's per-line
  decoration, so the rendered result really does fit the budget.

Still open:

- **A narrow window read still streams the whole file.** The fingerprint that
  guards a later edit covers the whole file, and there is no equally strong
  cheaper version of it. Memory is O(longest line + window); time is O(file).
  Correctness outranks the optimization, so this stays until a replacement
  proves itself.

**Target ownership, remaining.** Stale-write protection is already an explicit
mechanism between `ReadObservation` and `WorkspaceEditor` rather than a hidden
side effect, but the fingerprint is still recorded into the shared
`FileStateTracker` by the read tool rather than travelling with the
observation.

#### B. Workspace search is deterministic (closed for the four primitives)

`list_files`, `find_files` and `grep` are adapters over
`leveler-tools::workspace::WorkspaceSearch`: one traversal, one ignore rule
set, one glob dialect, one regex dialect, entirely in process.

- **No subprocess.** `git ls-files` and `rg` are gone. All four read primitives
  now declare `replay_is_side_effect_free`, which is the mechanical proof that
  no binary is consulted.
- **One meaning per invocation.** `grep`'s pattern is a regex, with `literal`
  and `ignore_case` as explicit parameters; it no longer degrades to a
  substring scan when `rg` is absent. `find_files`'s pattern is a glob, with
  the gitignore/ripgrep anchoring rule (no `/` matches the file name, a `/`
  matches the relative path); the `auto`/`substring`/`glob` mode switch is
  gone.
- **No Git-dependent candidate universe.** `.gitignore` and `.ignore` are
  honoured by the `ignore` crate whether or not this is a repository and
  whether or not Git is installed; the user's global gitignore is deliberately
  not read, because it would make the same repository search differently on two
  machines.
- **`list_files` is directory inspection.** It lists the direct children of one
  directory and hides nothing — `max_depth` is gone and so is the build-output
  filter, because a one-level listing never descends into `target/` anyway and
  concealing it costs the model a true fact.

Still open: `locate_hint` and the symbol fallback scans carry their own
traversal. They are not read primitives and were left alone.

#### C. Workspace edit has one owner (closed)

`leveler-tools::workspace::WorkspaceEditor` is the single guarded path from a
tool to a file on disk: the advisory cross-process lock held across compare and
rename, the compare-and-swap, the unguessable staging name, the
capability/descriptor-relative write, checkpoint capture, and write-scope
revalidation under the lock. `apply_patch` and `write_file` call it, and
`replace` did until it was removed from the surface (§6.5).

The code did not change — it was already the shared commit path, but it lived
inside the `replace` tool, so the shared runtime was named after one of its
callers. Only the owner moved.

#### D. Command execution has one owner (closed)

`run_command` used to carry sandbox setup, environment, network policy,
background processes, snapshots, mutation accounting, write scope, rollback,
the command gate and process lifecycle — and `shell_command` imported
`run_command::execute_program` for its own runtime, so one tool looked like
the owner of the other.

That runtime is now `crates/leveler-tools/src/tools/command_execution.rs`.
Both tools are constructed with it; neither owns it. Each keeps exactly its
own model interface: `run_command` decodes argv and phrases the two refusals
that mean "you wanted the other tool", `shell_command` maps a shell line onto
the platform shell and runs the hang guards.

**Still open.** The module lives in `leveler-tools`, not in
`leveler-execution`, because it reads the per-call authority off `ToolContext`
and returns a `ToolOutput` — types the layer below the tools must not depend
on. Moving it further down would need the authority and the result shape to
move with it, which is a larger question than this one.

**`shell_command` has no `background=true`, deliberately.** Mechanically it
could: a shell line IS `program + args`, and `proven_executed_commands`
already attributes a shell script. What breaks is the lifecycle guarantee.
`run_command(background=true)` registers the long-lived process ITSELF, so
`kill_task` and the session reaper actually kill it. A detached shell can exit
immediately after spawning its own child — `shell_command(cmd="python app.py
&")` — leaving the registry holding a task that reports `Exited` while the
real process runs on, unreapable. The asymmetry is not cosmetic, and the
existing guard already points the model at the tool whose lifecycle is honest.

#### E. Code intelligence has one owner (closed)

`find_symbol`, `read_symbol`, `find_references`, `diagnostics` and
`blast_radius` each contained LSP discovery, session lifecycle and startup —
and `find_symbol` carried a second copy of the locate logic that
`symbols.rs` already had. Three of them also carried three DIFFERENT source
walks: one keyed on `repo_map::is_source`, two on a shorter hardcoded
extension list, so the same repository had two ideas of what counts as source.

`leveler_lsp::LspSessions` now owns the session pool, startup, dead-server
eviction and `locate`; every one of the five tools is constructed with it. The
source walk is one function in `tools/symbols.rs`.

**The dependency-free fallbacks stay, and stay labelled.** `find_symbol` and
`read_symbol` answer from a scan when no language server is installed, and the
result says `(via scan)` where the precise answer says `(via rust-analyzer)`.
They answer a WEAKER question — which files define the name, not where — and
saying so is the point. What would be wrong is a fallback that presented
itself as the server's answer; `LspSessions::locate` returning `None` means
"no language server answer" and nothing more.

#### F. `view_image` duplicated — and weakened — `leveler-media` (closed)

`view_image` used to decide the MIME type from the file EXTENSION, read the
bytes and base64-encode them: no content sniffing, no pixel bound, no EXIF
strip. So a JPEG named `.png` was declared `image/png` to the provider, a
decompression bomb was only bounded by a 5 MB byte cap, and a photo's GPS tags
went out with it. Meanwhile `leveler-media` — whose only consumer was the user
attachment path — did all three.

The pipeline is now one function, `leveler_media::process_image`: real type
from content, byte and pixel bounds before the decoder allocates, downscale to
the longest-edge cap, re-encode to PNG. `MediaStore::import_bytes` calls it
and then hashes and stores; `view_image` calls it and then base64-encodes.
`leveler-tools` gained a dependency on `leveler-media` to do it, which is the
right direction: the tool layer calls the capability.

#### G. `web_search` owns provider configuration (closed)

The tool used to read `LEVELER_SEARCH_API_KEY`, `LEVELER_SEARCH_PROVIDER` and
`LEVELER_SEARCH_CX` from the environment snapshot itself, and carried both the
Bing and the Google Custom Search request and response shapes. It also decided,
at call time, whether it was configured at all — which is the composition
root's answer, not a tool's.

**Closed by deletion, not by abstraction.** The two provider shapes went; one
backend (Tavily) remains, written down once, with no `SearchProvider` in front
of it — a trait with one implementation and one user is the wrapper §5.3 exists
to prevent. Configuration now has a single owner: `leveler-app` reads the key
once, a blank value counts as unset, and that one answer feeds BOTH
`capability_availability` and `WebSearchTool::new`. A host without a key
registers no `web_search`, so the tool has no "not configured" branch left —
that state cannot reach it.

What stayed is not duplication. The `network_denied` check inside `execute` is
the ONLY enforcement point for a tool that dials the network in-process: the
ToolHost freezes `network_allowed` into the resolved policy, but the OS sandbox
that enforces it covers `run_command` children, not a `reqwest` call in this
process. `web_fetch` and `web_search` carry the same check for the same reason. The
browser does NOT: it is an explicitly network-authorised capability, so
exposing it IS the authorisation (§5.3).

Extract a provider seam when a second backend actually has to be supported.

#### H. Background settlement belongs to the runtime (closed)

`WaitTaskTool` used to run `account_background_mutations` at wait-end, which
could restore the whole workspace to a snapshot. Settlement therefore depended
on the model choosing to wait: a turn that ended first, or a model that never
waited, left a write-scope violation standing on disk.

`BackgroundTaskRegistry` now settles a task when its process exits. The reaper
consumes the `MutationBaseline`, diffs the workspace, and — only under an
explicit write allowlist — restores what the task was not allowed to touch,
then stores a `BackgroundSettlement` before publishing the terminal state. The
allowlist travels in the baseline because the authority a background task runs
under is the one it was spawned with; it outlives the round, so there is no
later scope to consult. `wait_task` reads that settlement once and renders it.

Dev-server safety is unchanged: restore still runs only under an explicit
allowlist, so a default background task is accounted and never rolled back
(K17).

`wait_task` stays non-`Safe`. It no longer performs the settlement, but it
consumes the one-time report, and a crash replay would block recovery for up
to two minutes and swallow it.

### 18.4 Sidecar processes bypass the command runner

**Current.** MCP servers (`leveler-tools/src/mcp.rs`), the browser
(`leveler-browser/src/cdp.rs`, `webdriver.rs`) and language servers
(`leveler-lsp/src/client.rs`, `registry.rs`) are spawned with `Command::new`
directly rather than through `leveler_execution::CommandRunner`.

**Desired.** Either these run under the host authority's process-tree
termination and sandbox semantics, or the exemption is an explicit, named
policy — "runtime sidecar" — with a defined lifecycle owner.

**Why it partially violates the constitution.** Rule 4. They are not
model-requested commands, so permission and approval do not apply; they are
still host processes outside the authority that owns host processes.

**Risk.** Low to medium. Sidecar lifetime is entangled with daemon shutdown
reaping.

### 18.5 `update_plan` sat with the capability adapters (closed)

`update_plan` is a harness control: it carries no capability, touches no
capability handle, and exists only because the Coding harness has a plan
protocol. It was registered in `leveler-tools` alongside the capability
adapters while the other seven controls lived in `leveler-agent`.

**Why it did not move in W1.** The injected path bypasses the registry
entirely, so moving it then would have meant hand-reimplementing four registry
services for one tool: `normalize_input` (it repairs a nested-envelope shape
models emit), JSON-Schema validation, the `schemars`-derived schema, and
output capping.

**What moved.** It is now `crates/leveler-agent/src/update_plan.rs`, and
`register_harness_controls` puts it on the surface after the capability packs.
It stayed a registered `Tool` rather than becoming an eighth injected
definition, because the registry is now a mechanical seam and not a policy
engine — so reusing it costs nothing and reimplementing it would have cost
four duplicated services. Validation, the derived schema, dispatch and the
result cap are the registry's; the `metadata.plan` → `PlanUpdated` path is
unchanged.

The `core_surface` no longer registers any control, and the harness registers
no capability. `leveler-tools` composes what the host can do;
`leveler-agent` composes what steers the harness.

### 18.6 `leveler-model` knew the Coding tool names (closed by deletion)

**Was.** `crates/leveler-model/src/tool_catalog.rs` hard-coded `grep`,
`find_files`, `find_symbol`, `read_symbol`, `find_references`, `list_files`,
`read_file`, `git_status`, `git_diff`, `view_image`, `web_search`,
`web_fetch`, `apply_patch`, `replace`, `run_command` and `shell_command`, and
derived from them an execution class, a replay-safety answer, a primary
argument and an observe key. It still listed `replace`, which had already been
deleted — the exact failure mode a second copy of a name list has.

**How it closed.** By deleting it, not by moving it. The audit found the
catalog had almost no live consumers:

| Export | Consumer | Outcome |
| --- | --- | --- |
| `is_safe_replay_tool` | `leveler_client_protocol::recovery_for_tool` | `recovery_for_tool` and its `Recovery` enum were themselves dead — re-exported, never called. Both deleted. The live answer is `Tool::replay_is_side_effect_free`, which the registry asks and which an unknown name answers `false`. |
| `builtin_tool_metadata(…).primary_argument` | `leveler-tui`'s `find_files` label | Inlined as `s("pattern")`, next to the forty other tool labels in the same match. Presentation metadata belongs to the client (§5.2). |
| `is_search_tool`, `builtin_observe_key`, `BuiltinToolClass` | nothing | Deleted. |

So there is no second name table anywhere — not a moved one either.
`crates/leveler-tools/tests/ownership_boundaries.rs` fails if a coding tool
name reappears in `leveler-model`'s production code.

### 18.7 `ToolOutput.metadata` is an untyped internal side channel

**Current.**

```rust
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub metadata: serde_json::Value,
}
```

Producers in `leveler-tools` write string-keyed JSON; consumers in
`leveler-agent/src/executor/dispatch.rs` read it back by key:
`plan`, `image`, `applied_diff`, `executed_commands`, `modified_files`,
`outcome`, plus the tool-expansion request. The producer and consumer are in
different crates, connected only by a string convention.

Four different kinds of thing travel in one field:

```text
Model output   ≠   Runtime facts   ≠   Harness control   ≠   UI payload
```

`modified_files` is a runtime fact that feeds verification obligations.
`plan` is harness control. `image` is model-visible content. `applied_diff`
is UI and evidence. None of them is typed, and a typo in a key fails silently.

**Desired.** These four are distinguished in the type system.

**Do not design the final enum now.** The shape should follow the capability
extraction, not precede it.

**Risk.** Medium. It touches every tool and the dispatch path at once, so it
wants to be the last step, not the first.

### 18.8 The verifier's doc comment claims completion authority (closed)

**Was.** `crates/leveler-verifier/src/lib.rs` opened with "Only the
verifier can mark a task complete".

**Now.** The comment states what the code always did: the verifier is the
authority for verification verdicts, not for completion. A passing verdict is
mechanical evidence a task outcome is judged against, not the runtime deciding
the request was satisfied.

**Closed by.** Rewriting the doc comment; the code already separated the axes.

### 18.9 The engine shelled out to git for the baseline (closed)

**Was.** `leveler-engine/src/baseline.rs` and `engine.rs` called
`Command::new("git")` directly, while `leveler-vcs` existed and performed zero
direct process spawns.

**Now.** `baseline.rs` is gone and the engine spawns no process at all.
Repository facts reach a turn through the `WorkspaceFacts` port, which the
Coding harness implements as `GitWorkspace`
(`leveler-agent/src/coding/workspace.rs`). A harness with no repository — the
minimal harness in §17 — passes `None` and the engine asks for nothing.

**Closed by.** W3, accepted in W4.

---

### 18.10 The browser implementation was superseded (closed)

**Was.** `leveler-browser` drove Chromium through Playwright over a
CodeLeveler-authored Node bridge speaking custom JSON-RPC on stdio, with an
SSRF boundary enforced inside that bridge.

**Now.** The Browser Capability Closure replaced it. Chrome, Edge and Chromium
are driven directly over CDP; Safari is driven over W3C WebDriver through
`safaridriver`. There is no Node, no Playwright, no npm install, no managed
browser download and no custom RPC. Which browser runs is the user's default
browser unless a call or `[browser].default` says otherwise, and a product that
cannot be driven is an error rather than a different product.

**What closed with it.** The parked
`loopback_ws_from_a_granted_dev_page_connects` flake was a finding about the
bridge's JavaScript WebSocket gate. That gate is deleted, along with the whole
page-scoped loopback grant it belonged to: the browser is network-authorised,
so there is no per-request loopback decision left to race. The finding is
closed by deletion, not by a fix.

### 18.11 The parallel parent session has a second lifecycle writer (closed)

**Current.** `TaskEngine::finish_task` says the engine is the one writer of
the session lifecycle and that no app layer stamps a second copy. That is
true of every session the engine runs, and false of one it does not: the
parallel multi-agent PARENT session is created, marked `Running` and given
its terminal row by `leveler-app/src/parallel.rs`, which calls
`SessionStore::update_status_owned` and `TerminalStore::finish_task_owned`
directly.

**Not a two-writer defect.** The two writers own disjoint sessions and the
overlap is not reachable. The parent never enters the engine's run path, and
it cannot be pulled into one: `run --resume` refuses a session with no
transcript, and `parallel.rs` writes the parent no messages and no turns.
Both writers are fenced on the same ownership token besides, so a stale
runtime stamps nothing either way. Verified by resuming a `parallel`-kind
session with no transcript, which the engine refuses.

**What is actually wrong.** The engine's doc comment claims an exclusivity
the code does not have, and the barrier that makes the claim hold is
incidental — an empty transcript — rather than an explicit refusal to run a
`Parallel` session. `CodingRuntime::resume` also carries a `kind` guard that
cannot fire, because both sides of its comparison are read from the same
session row.

**Closed.** The engine's comment now says what is true, and
`CodingRuntime::resume` refuses `ExecutionKind::Parallel` outright instead of
relying on the transcript being empty. A regression test
(`resume_refuses_a_parallel_parent_session`) pins the refusal to the kind.

### 18.12 The engine read Coding semantics to run its own mechanics (closed)

**Was.** `leveler-engine` named no harness crate and no harness type — §18.1
was honestly closed — and still made three Coding judgements, each reached
through `leveler-lifecycle`, which both sides legitimately share. A dependency
direction can be clean while the semantics flow the wrong way.

1. **Fresh-turn inheritance.** `should_seed_task_state` called
   `PlanState::is_fully_completed` and
   `ProgressLedger::is_terminal_for_inheritance` to decide whether a new turn
   inherited the prior plan, ledger and progress. "Is the previous epoch
   finished?" is a question in the Coding vocabulary; the engine answered it.
2. **The outstanding-child record.** `leveler-agent` wrote its outstanding
   children as `id|nickname|role|files`, and the engine pulled them apart with
   `splitn` to reconcile them against durable terminals. That is a private
   harness encoding, decoded in the engine.
3. **What a lost child contributed.** When the engine settled a ghost child it
   called `ChildResultProjection::from_findings` over the child's role and the
   evidence ledger, and wrote the resulting contribution and summary itself.
   Role meaning and findings are Coding semantics.

**Now.** Each judgement sits with its owner and the engine keeps the mechanics.

1. `leveler-agent::coding::run::prior_epoch_open` computes the answer and
   passes it as `SeedRequest::Fresh { prior_epoch_open }`. The engine's rule is
   `continues_active_goal || prior_epoch_open` and nothing else.
2. The engine carries `FinishedChildFact` verbatim in `TurnSeeds`;
   `leveler-agent::coding::turn::reconcile_outstanding_children` decodes,
   prunes and re-delivers. No engine source parses the entry format.
3. The engine detects the ghost, orders its terminal before the turn's own,
   attributes it to the turn the child STARTED in, stamps `ok: false` and
   appends it through the fenced log — all unchanged. What the child
   contributed it asks for, through the `LostChildVoice` port that
   `CodingLostChildVoice` implements. A harness with no answer supplies no
   voice and still gets a truthful terminal; `ok: false` is not delegable, so
   no harness can turn a lost child into a successful one.

**Closed by.** One new port (`LostChildVoice`, with `LostChild` and
`LostChildNote`) and two booleans. No new crate, no framework, no event
renamed. Three tripwires in
`crates/leveler-engine/tests/ownership_direction.rs` hold the line: the engine
source may not name `is_fully_completed`, `is_terminal_for_inheritance` or
`from_findings`. The second harness proves the other half — it has no roles,
no findings and no voice, and the engine still settles its ghosts
(`the_engine_settles_a_ghost_child_for_a_harness_with_no_child_semantics`).

---

## 19. Open design questions

Recorded so they are not silently decided by the first person who needs them.

### 19.1 Should a tool result carry typed parts?

The kernel's tool result is:

```rust
pub struct ToolOutcome { pub content: String, pub is_error: bool }
```

The model vocabulary already supports `ContentPart::Image`. Today an image
reaches the model by travelling through `ToolOutput.metadata` and being
re-attached by the harness (§18.7).

The question: should the kernel's tool result support typed text/image parts?

The constraint: **do not change the agent kernel for elegance.** This changes
only if the provider protocol supports it and a real image or multimodal tool
use case proves the metadata path insufficient.

### 19.2 Closed: the canonical edit tool is `apply_patch` plus `write_file`

`replace` is deleted. It overlapped both, its weak-model rationale was withdrawn
(§1.1), and it went unused through the patch failures it was built to absorb
(§6.5).

### 19.3 Closed: patch context matching is exact

`seek_sequence` used to locate a hunk through five progressively looser passes.
The question was framed as a matter of degree; reading the apply path settled it
as correctness.

A located hunk is applied by `file.splice(start..end, replacement)`, and an
unchanged (` `) context line is part of that splice. So a hunk located by a
loose comparison rewrites the file's real bytes on lines the model never asked
to change: trailing whitespace disappears, typographic quotes become ASCII,
spacing is reformatted — inside a call that reports success and reports nothing
about it. It also put the two edit tools in direct contradiction, since
`replace` refused the typographic near-miss that `apply_patch` silently folded.

The comparison is now byte-exact. The tolerances that remain are
representation, not meaning: BOM strip and restore, CRLF fold and restore, the
trailing-blank retry, and the trailing-newline rule — each preserves or is
explicitly documented to change bytes. An inexact patch fails, the file is
untouched, and the error shows what the file really contains at the anchor.

One consequence is recorded rather than fixed: in a mixed-ending file where LF
dominates, a stray `\r` stays embedded in the line, and such a line no longer
matches a patch written without it. Making that exact would mean per-line
ending preservation on write.

### 19.4 Closed: dynamic tool expansion does not pay for itself

`expand_tools` bought schema tokens with an extra round, dynamic registry state
and inverted surface ownership. It is deleted, and the deciding fact was not the
trade: nothing consumed its output, so it never expanded anything (§6.5).

---

## 20. Architecture change rules

1. **Extend above the foundation before modifying it.**
2. **A foundation change needs evidence**, not elegance: two real
   implementations, a real dependency-inversion boundary, an observed coupling
   or ownership defect, or an independent protocol / security / persistence /
   runtime boundary.
3. **Answer the decision test in `AGENTS.md` before you start**, in the pull
   request, not afterwards.
4. **Do not describe the target as the present.** If a change moves toward a
   boundary without reaching it, update §18 rather than deleting the entry.
5. **Do not add speculative interfaces for products that do not exist.**
6. **Do not simplify by deleting reliability.** Move complexity to its correct
   owner instead. Every safety property named in §18 survives its move.
7. **This document is the only canonical architecture.** Do not create
   `ARCHITECTURE_V2.md`, `FOUNDATION_*.md` or a "final" variant.
8. **The foundation is frozen (§21).** No architecture-driven foundation
   refactor without observed mechanical evidence. "It could be more elegant",
   "we might extend it later" and "a competitor designs it this way" are not
   evidence. Two real implementations that need a boundary might be. A real
   run that exposes an ownership, safety or reliability defect is.

Roadmap items — multi-agent direction, browser direction, a Review product,
cloud, ACP, remote workers, NPC workflows, future providers, future UI — are
not architecture. This document may describe an extension point. It does not
promise a feature.

---

## 21. Documentation status

```text
FOUNDATION_ARCHITECTURE_DEFINED   YES
TOOL_ARCHITECTURE_DEFINED         YES
MODEL_CAPABILITY_POLICY_DEFINED   YES
WEAK_MODEL_COMPENSATION_GOAL      REMOVED
CORE_PRIMITIVE_FOUNDATION_DEFINED YES

TOOL_IMPLEMENTATION_ALIGNED       YES
ENGINE_IMPLEMENTATION_ALIGNED     YES
CORE_PRIMITIVE_FOUNDATION_ALIGNED YES
TOOL_SURFACE_CLOSED               YES
CAPABILITY_MODEL_CLOSED           YES
AVAILABLE_ENABLED_EXPOSED         SEPARATED
PLAN_ENFORCEMENT                  REMOVED
EDIT_MATCHING                     EXACT
TOOLREGISTRY_CLOSED               YES
TOOLCONTEXT_CLOSED                YES
FOUNDATION_TOOL_NAME_LEAKAGE      NONE
WORK_PROFILE_AUTHORITY            SESSION_ROW
ENGINE_LIFECYCLE_ONLY             YES
HARNESS_OWNS_CODING_SEMANTICS     YES

BROWSER_IMPLEMENTATION            CLOSED_BY_REPLACEMENT

SECOND_HARNESS_TEST               ENFORCED
SECOND_HARNESS_WRITTEN            TEST_ONLY_PROOF
SECOND_HARNESS_PRODUCT            NO
FOUNDATION_FROZEN                 YES
```

The architecture, the tool boundary and the model-visible surface are decided;
the seven core primitives, the capability ownership and the composition are
implemented against them. The engine was the last of it, and W3 closed it:
`leveler-engine` names no harness crate and no Coding type (§18.1), so
`SECOND_HARNESS_TEST` is enforced by a test rather than argued for in prose
(§17).

`FOUNDATION_FROZEN` is baselined at `70e63900`. What that baseline means is in
§20 rule 8: the foundation is not sealed against change, it is sealed against
change argued from architecture alone. A real run that exposes an ownership,
safety or reliability defect reopens it; an aesthetic reading of the crate
graph does not.

The freeze has survived one such reopening. A re-audit found that the engine
still made three Coding judgements through the shared `leveler-lifecycle`
types even though it named no harness crate — a real ownership defect, so the
rule applied and the boundary work was done. §18.12 records all three and how
each closed. `ENGINE_LIFECYCLE_ONLY` and `HARNESS_OWNS_CODING_SEMANTICS` are
the two lines that reopening added, and each is held by a source tripwire
rather than by this paragraph.

The freeze rests on its own acceptance, run at that commit. A harness that is
not this product ran a full session — create, turn, persist, crash, reap,
resume, terminal — over the engine and needed no new engine API, trait, crate
or abstraction. The real Coding harness then ran three live tasks end to end:
a small edit verified green, an interrupted run killed with `kill -9` and
resumed to completion with the lost turn recorded as `interrupted`, and a
front-end change driven through the browser. `cargo fmt --all --check` and
`cargo clippy --workspace --all-targets` were clean, and the workspace suite
ran three consecutive times at 3599 passing and none failing.

Four lines went from NO to YES because the code changed, not the prose:
`ToolServices` is deleted, the registry decides no authorization,
`leveler-model` holds no tool name, and a turn's work profile comes from the
session row. Each has a tripwire named in its section. Nothing here was
changed to improve a line of this table.

`BROWSER_IMPLEMENTATION` is closed by replacement. The Node bridge,
Playwright, the custom RPC and the JavaScript network gates are gone;
`leveler-browser` speaks CDP and WebDriver directly, and §18.10 records what
that closed.

The line rests on the replacement's own acceptance, not on the old flake
having stopped reproducing. Chrome and Safari each passed eleven live
end-to-end checks three consecutive times, and the workspace suite ran three
consecutive times at 3591 passing and none failing. The old
`loopback_ws_from_a_granted_dev_page_connects` finding keeps its honest
epitaph: root cause UNPROVEN, fix NONE, code path REMOVED. Nothing was proved
about a race in an implementation that no longer exists.
