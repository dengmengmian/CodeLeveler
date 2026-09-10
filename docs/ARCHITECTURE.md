# CodeLeveler Architecture

The canonical architecture document. `AGENTS.md` states the constitution in
short form and points here; this file is where the boundaries are defined and
where the gap between the current implementation and the target foundation is
recorded honestly.

Chinese version: [`ARCHITECTURE.zh-CN.md`](ARCHITECTURE.zh-CN.md).

Everything below was verified against the workspace at commit `6724268`
(31 crates) using `cargo metadata` and the crate sources. Where the code does
not yet match the target boundary, it says so rather than describing the
target as if it were already true.

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

Two places where the running code does not match this picture yet:

- **The engine sits above the harness, not beside it.** `leveler-engine`
  depends on `leveler-agent` and its public API names Coding concepts. See
  §18.1.
- **Tool adapters and capability implementations are one crate.**
  `leveler-tools` holds both, and several tools implement capability behavior
  themselves. See §5 and §18.

Everything else in the diagram is the real dependency shape.

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
web, CLI or any other product concept. `leveler-model` currently does — see
§18.6.

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
FindSymbolTool    → CodeIntelligence
BrowserClickTool  → BrowserRuntime
ViewImageTool     → Media
WebSearchTool     → Search provider
```

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
a third permission or approval path. Today `ToolRegistry` does — see §5.6.

### 5.5 ToolContext: current state and target

Current shape:

```text
ToolContext = ExecutionResources + ToolPolicy + ToolServices + session_scope
```

`ToolServices` names `lsp_sessions`, `lsp_start_locks`, `artifact_store`,
`memory_root`, `background_tasks` and `browser` as struct fields. Every tool
receives all of them, whether or not it uses any.

That makes `ToolContext` a service locator, a policy container, an execution
container and a session capability container at once. It is recorded as debt
in §18.2.

Target:

```text
Tool dependencies are explicitly injected.
A tool receives only the capability it needs.
Per-call context carries only genuinely dynamic invocation state, or
authority the host minted for this call.
```

**Do not design a replacement now.** No `BetterToolContext`, no
`ToolExecutionContextV2`, no `CapabilityContext`. The target is that
`ToolContext` shrinks or disappears as a *consequence* of capability
extraction, not that a new container is invented ahead of it.

### 5.6 ToolRegistry: current state and target

`ToolRegistry::execute` today performs, in order: read-only enforcement,
zero-write-authority enforcement, permission-mode enforcement, input
normalization, JSON-schema validation, dispatch, and a central output-budget
cap. The module also owns the observe-class name list, the read-only subset,
MCP filtering, the `core`/`full` compositions and `expand_tool_category`.

That is a policy engine wearing a registry's name.

Target:

```text
ToolRegistry
    register
    lookup
    definitions
    schema validation
    adapter dispatch
```

Everything else moves to its owner:

| Concern | Target owner |
| --- | --- |
| tool selection, work profile, read-only set, dynamic capability choice | Harness |
| permission, approval | ToolHost |
| ownership, write scope | ToolHost / runtime |
| result budget | Harness / runtime result handling |

The registry must not become a policy engine again.

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
│ Browser Runtime         VCS      Memory          │
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

The harness-control column is **already separate in the code**: those tools
live in `crates/leveler-agent/src/injected_tools.rs`, not in the registry.
`update_plan` is the exception — it sits in `leveler-tools` with the capability
adapters. See §18.5.

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

Measured from `crates/leveler-tools/src/registry.rs` at this commit:

| Set | Count |
| --- | --- |
| `core_registry()` | 14 |
| `full_registry()` = core + 17 + 12 browser | 43 |
| Harness control tools injected by the executor | 7 |
| `default_registry()` | `full_registry()` |

So the default surface is on the order of fifty tools. `core_registry()` is
selected by the economy work profile, and `expand_tools` lets the model ask
for more mid-session.

Two facts worth naming, because they cut against the obvious reading:

- **`find_files` is not in `core_registry()`.** The economy surface has
  `grep` and `list_files` but reaches `find_files` only through
  `expand_tools("search")`. If `find_files` is a core primitive — and the
  distinct model intent argues it is — that is a surface-composition bug, not
  a capability gap.
- **`replace`, `shell_command`, `update_plan`, `load_skill`, `expand_tools`
  and `memory` *are* in core.** The economy surface is not the primitive set;
  it is a historical selection.

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

Explicitly:

```text
core_registry()  !=  Core Primitive Foundation
```

`core_registry()` is an implementation and history artifact — the set the
economy work profile happens to select — until a separate Tool Surface Closure
aligns it. Do not read one as a definition of the other.

### 6.3.1 The four categories

**Core capability tools** — the Core Primitive Foundation above.

Whether both `run_command` and `shell_command` stay on the model surface is a
semantic question about distinct intent and analysability (§6.5), not a
question about model strength.

**Optional capability packs** — real capabilities that need not be visible
every round:

```text
Code Intelligence   find_symbol, read_symbol, find_references,
                    diagnostics, blast_radius
VCS                 git_status, git_diff
Browser / Web       browser_* (12), web_fetch, web_search
Media               view_image
Memory              memory, remember
Skills              load_skill
Background process  get_task, wait_task, kill_task
```

An implementation existing is not a reason to expose it by default.

**Harness control tools** — the Coding harness's control protocol, not
reusable capability:

```text
request_user_input (alias ask_user)    update_goal
update_plan                            request_permissions
spawn_agent                            claim_write_scope
report_finding
```

A Review harness would have a different set here. That is the point.

**Extension tools** — MCP-discovered tools. The MCP protocol and runtime stay
conceptually separate from the `McpTool` adapter.

### 6.4 Who decides the surface

```text
The Harness decides what tools exist for the product.
```

It decides from the work profile, the task type, the capabilities configured
and the model profile. The kernel knows none of this.

This is why `expand_tools` is architecturally suspect: it inverts the
ownership, letting the model ask the runtime to reveal more tools. That costs
an extra tool call, an extra round, dynamic registry state, more replay and
control semantics. Unless evaluation shows the schema-token saving beats the
extra round on both success and cost, harness-side selection is the pattern
and `expand_tools` is not.

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

Candidates for evaluation, with the reason each is on the list:

| Tool | Why it is a candidate |
| --- | --- |
| `expand_tools` | Inverts surface ownership; costs a round to buy schema tokens. Strong removal candidate. |
| `create_checkpoint` / `restore_checkpoint` | The runtime already maintains checkpoints and recovery. Asking the model to know when to checkpoint adds cognitive load and turns a runtime facility into tool semantics. Should leave the default model surface absent a specific product requirement. |
| `consolidate_memory` | Memory subsystem maintenance, not a coding capability. Can run in the background, at session close, periodically, or on an explicit user command. |
| `forget` | Destructive mutation of durable state on a semantic judgement the model is not positioned to make. Deleting durable memory is closer to a user action. |
| `create_skill` | System customization / metaprogramming. `load_skill` can stay optional; creation should not be a default coding-task affordance. |
| `blast_radius` | An advanced Code Intelligence operation (references → enclosing symbols → BFS), not a primitive. Move to the optional pack, then evaluate whether it reduces rounds or improves refactor recall. |
| `read_symbol` | `find_symbol` + `read_file` reproduces it. It survives only if it measurably cuts tokens or rounds. Pure evaluation question. |
| `replace` | **REMOVE / SURFACE-EVAL CANDIDATE.** Its original rationale — exact find/replace for weaker models that fail on patch context matching — is withdrawn (§1.1) and is no longer a reason to keep it. The remaining question is the ordinary one: does `replace` express a distinct, generally useful coding primitive that `edit` + `write` do not already cover? Judge the overlap; a weak-model A/B is not a precondition for removing it. |
| `shell_command` vs `run_command` | `run_command` is program+args: cross-platform, no shell quoting, simple to analyse. `shell_command` is the string form models know best. `shell_command` already reuses `run_command`'s `execute_program`, so two adapters over one capability is architecturally fine. Whether both stay on the model surface is an evaluation question. |
| `git_status` / `git_diff` | Reproducible via `run_command`, but git inspection is high-frequency, needs no shell, has stable output and replays cleanly. Keep the interface for now; move the implementation to `leveler-vcs`. |

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
task success. `replace`, `read_symbol`, `blast_radius`, `expand_tools` and the
checkpoint tools are the priority list for that measurement.

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
| `leveler-tools` | 13 | `replace.rs` writes through `context.execution.workspace` (root fd, symlink-safe). `mcp.rs` spawns configured MCP servers directly. |
| `leveler-browser` | 15 | Driver install writes under the Leveler home; the driver process is spawned directly. |
| `leveler-memory` | 11 | Writes the memory store under the Leveler home. |
| `leveler-lsp` | 4 | Spawns language servers directly. |
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
| `leveler-vcs` | Git operations, performed through the execution authority. |
| `leveler-lsp` | Language-server sessions, reused across tool calls. |
| `leveler-browser` | Browser runtime, driver install, isolated per-project profile. |
| `leveler-media` | Content-typed image import: real MIME from content, decode and pixel limits, EXIF stripping, downscaling, content-addressed storage. |

A Coding harness selects context, project, VCS, LSP, browser, memory,
filesystem mutation and process execution. A Review harness would plausibly
select context, project, VCS, LSP, read-only filesystem and memory, and skip
the rest.

`leveler-media` currently has exactly one consumer, `leveler-app`. The
`view_image` tool does not use it. See §18.3.F.

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
engine". It does not yet remove the Coding dependency — see §18.1.

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

The crate-level doc comment in `leveler-verifier/src/lib.rs` still says
otherwise. See §18.7.

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
| 3 | `leveler-provider` | `core`, `model`, `protocol` |
| 3 | `leveler-tools` | `browser`, `context`, `core`, `execution`, `lsp`, `memory`, `model`, `project`, `skills` |
| 3 | `leveler-local-transport`, `leveler-remote-protocol`, `leveler-tui`, `leveler-web` | client protocol and below |
| 4 | `leveler-agent` | `agent-core`, `context`, `core`, `execution`, `lifecycle`, `memory`, `model`, `skills`, `tools` |
| 4 | `leveler-relay`, `leveler-remote-agent` | remote protocol and below |
| 5 | `leveler-engine` | `agent`, `context`, `core`, `execution`, `lifecycle`, `model`, `storage`, `tools`, `verifier` |
| 6 | `leveler-app` | 18 internal crates |
| 7 | `leveler-cli` | 21 internal crates |

Findings:

- **No reverse dependency exists.** Nothing depends on `leveler-app`,
  `leveler-cli`, `leveler-tui` or `leveler-web`.
- `leveler-agent-core` depends on exactly one internal crate.
- `leveler-agent → leveler-execution` is a **vocabulary** edge: the harness
  uses `PermissionProfile`, `RiskLevel`, `WriteScope` and `HookRunner` as
  types. Its direct side-effect count is zero.
- `leveler-tools` does **not** depend on `leveler-media`, which is why
  `view_image` reimplements image handling (§18.3.F).
- `leveler-engine → leveler-agent` is the one edge that contradicts the layer
  model (§18.1).

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

### Current verdict: NOT YET ENFORCED

The kernel and the tool boundary pass. `leveler-agent-core` depends only on
`leveler-model`, carries no product vocabulary, and `ToolRuntime` is two
methods a Review harness can implement its own way. `AgentHarness` is already
implemented by a real harness.

What does not pass yet:

- A Review harness that wants persistence, resume, event ordering and recovery
  goes through `leveler-engine`, which depends on `leveler-agent` and whose
  public API names `CodingTaskSpec` (§18.1).
- `leveler-model` knows the names and execution classes of Coding built-in
  tools, so a Review tool set inherits a vocabulary written for Coding
  (§18.6).

Neither forces a kernel change, which is why this is "not yet enforced" rather
than "fail". They are the inputs to Foundation Hardening.

**Do not modify code to convert this verdict to PASS as part of a
documentation change.**

---

## 18. Known boundary debt

Recorded, not hidden. Each item states the current behaviour, the desired
boundary, why it violates the constitution, the minimal correction, and the
risk.

### 18.1 The engine depends on the Coding harness

**Current.** `leveler-engine → leveler-agent`. The engine's public API exports
`CodingTaskSpec`, and `ExecutorFactory` constructs a `leveler_agent::Executor`
directly. `recorders.rs`, `recovery.rs`, `turn.rs` and `policy_resolver.rs`
all name `leveler_agent` types.

**Desired.** The engine runs a harness executor behind an abstraction; it does
not name a domain.

**Why it violates the constitution.** Rules 5 and 2: a second harness inherits
the Coding harness through the engine.

**Minimal correction.** The `RuntimeTaskSpec` / `CodingTaskSpec` split already
exists as the migration seam. Next is an executor abstraction the engine can
drive without naming `leveler_agent`, with `ExecutorFactory` moving above it.

**Risk.** Medium. `ExecutorFactory` is deliberately the single derivation of
execution configuration; splitting it badly reintroduces the multiple-
derivation bug it was built to remove.

### 18.2 ToolContext is a universal service locator

**Current.** Every tool receives `ExecutionResources + ToolPolicy +
ToolServices + session_scope`, with `ToolServices` naming `lsp_sessions`,
`lsp_start_locks`, `artifact_store`, `memory_root`, `background_tasks` and
`browser` as fields.

**Desired.** Explicit dependency injection; a tool receives only the
capability it needs; per-call context carries only dynamic invocation state or
host-minted authority.

**Why it violates the constitution.** Tools own service discovery they should
not have, and a Review-specific tool needing none of it still takes the shape.

**Minimal correction.** Let it shrink as a consequence of capability
extraction. Do not design a replacement container first.

**Risk.** Low if it follows extraction; high if a `V2` container is invented
ahead of the extraction it is supposed to serve.

### 18.3 Concrete tool implementation debts

Each of these is a tool implementing capability behavior it should be calling.

#### A. `read_file` does too much, and two of its policies are wrong

`crates/leveler-tools/src/tools/read_file.rs` currently owns: file reading,
paging, binary detection, full-file fingerprinting, repeated-read policy,
stale-write state preparation, output budgeting and model guidance.

Verified consequences:

- **A narrow range read still scans the whole file.** The source says so:
  "Stream the complete file once … while still producing the full-file
  fingerprint needed by stale-write protection and the total line count used
  in paging copy." Memory stays bounded; time is O(file).
- **Files over 10 MB are refused outright**, before any range is considered,
  and the model is told to "use `grep` … or `run_command` with sed/head/tail".
  A bounded 50-line read of a 100 MB file is a reasonable request that the
  tool cannot serve, and the suggested workaround is not cross-platform — a
  problem the project's Windows support makes concrete.
- **Invalid UTF-8 is silently rewritten.** Lines are rendered with
  `String::from_utf8_lossy`, so invalid bytes reach the model as `U+FFFD`.
  The binary guard only scans the first 8 KB for NUL, so a file that is
  non-UTF-8 but NUL-free in its opening bytes takes the lossy path. The bytes
  on disk and the text shown to the model differ, and nothing says so.
- **Repeated-read policy lives in the read tool.** Whether the model is
  wasting rounds re-reading the same range is harness behavior policy, not
  filesystem read semantics.
- **Stale-write tracking lives in the read tool.** `read_file` records a
  fingerprint into `FileStateTracker` so `apply_patch` can later refuse a
  stale write. That is why a narrow read must scan everything: reading carries
  the precondition state for a future edit.

**Target contract** (behavior, not an API):

```text
Read is bounded.
Read semantics are deterministic.
A narrow range read does not require unrelated full-file work unless
  mechanical correctness proves it necessary.
Reading does not own edit policy.
Reading does not own repeated-read policy.
Reading does not own result-budget policy.
Invalid text or binary data is reported honestly rather than silently
  rewritten into different text.
Large files are inspectable through bounded reads rather than requiring
  a shell command merely because of file size.
```

**Target ownership.** `ReadFileTool → WorkspaceReader`, returning a bounded
structured read result. Stale-write protection becomes an explicit mechanism
between workspace read observation and `WorkspaceEditor`, not a side effect
hidden in a read tool. Repeated-read nudging moves to the harness, which
already sees tool history.

**Risk of correcting.** Medium. Stale-write protection is a real safety
property; it must survive the move intact. It must not be weakened to make a
read faster.

#### B. Workspace search has environment-dependent semantics

`list_files`, `find_files`, `grep` and the symbol fallback scans each carry
their own directory traversal, ignore rules and result caps.

Worse, `grep` changes query semantics with the machine: with `rg` installed
the pattern is a regex; without it, the built-in fallback matches it as a
literal substring. The tool does append a `[note] ripgrep unavailable …` line
when the pattern looks regex-shaped, so it is not silent — but the same tool
call still means different things on different machines.

**Target.** One deterministic workspace-search semantics across macOS, Linux
and Windows, with one traversal and one ignore rule set beneath `list_files`,
`find_files` and `grep`.

**Risk.** Medium. A bundled regex engine changes match results for existing
users; the change needs to be deliberate and announced.

#### C. Workspace edit logic is duplicated across two tools

`apply_patch` and `replace` each carry stale protection, compare-and-swap,
atomic mutation, rollback and diff/evidence.

**Target.** `ApplyPatchTool` and `ReplaceTool` both call one
`WorkspaceEditor`. Compare-and-swap, stale protection, rollback and
all-or-nothing behavior are preserved exactly; only the owner changes.

**Risk.** Medium-high. This is the code path where a bug corrupts a user's
file. It moves only under the existing tests, with no behavior change in the
same step.

#### D. `run_command` owns most of command execution

`run_command` currently carries sandbox setup, environment, network policy,
background processes, snapshots, mutation accounting, write scope, rollback,
the command gate, process lifecycle, and — since `6724268` — subtracting the
paths a HEAD move explains from what the run reports as authored.

**Target.** `RunCommandTool` and `ShellCommandTool` are adapters over a
`CommandExecution` capability, which calls `leveler-execution`.

**Risk.** Medium. Cancellation and process-tree termination semantics must not
change.

#### E. Code intelligence lifecycle lives in the tools

`find_symbol`, `read_symbol`, `find_references`, `diagnostics` and
`blast_radius` each contain LSP discovery, session lifecycle, startup and
fallback scanning.

**Target.** One `CodeIntelligence` capability owning LSP lifecycle, symbol
queries, references, diagnostics and a deterministic fallback. Tools do
model-facing invocation only.

**Risk.** Low-medium.

#### F. `view_image` duplicates — and weakens — `leveler-media`

`view_image` decides the MIME type from the file extension, reads the bytes
and base64-encodes them. `leveler-media` detects the real MIME from content,
bounds decode allocation and pixel count against decompression bombs, strips
EXIF by re-encoding, downscales oversized images and stores them
content-addressed.

`leveler-tools` does not depend on `leveler-media` at all, and
`leveler-media`'s only consumer is `leveler-app`. So the user-attachment path
is hardened and the model-facing tool path is not.

**Target.** `ViewImageTool → Media capability`.

**Risk.** Low. This one is close to a straight defect.

#### G. `web_search` owns provider configuration

The tool reads `LEVELER_SEARCH_API_KEY`, `LEVELER_SEARCH_PROVIDER` and
`LEVELER_SEARCH_CX` from the environment, builds its own HTTP client, and
implements both the Bing and Google Custom Search request and response
shapes.

**Target.** `WebSearchTool → Search capability / provider`. The tool does not
know provider credentials or configuration.

**Risk.** Low.

#### H. `wait_task` owns workspace settlement

`WaitTaskTool` is deliberately not `RiskLevel::Safe`, because at wait-end it
runs `account_background_mutations`, which can restore the whole workspace to
a snapshot. Its own source comment says so.

A model-facing tool named "wait" should not own workspace rollback semantics.

**Target.** A `BackgroundTaskRuntime` owns settlement. `get_task` and
`wait_task` read or await state.

**Risk.** Medium. Background-task settlement interacts with dev-server safety
rules that exist for a reason.

### 18.4 Sidecar processes bypass the command runner

**Current.** MCP servers (`leveler-tools/src/mcp.rs`), the browser driver
(`leveler-browser/src/driver.rs`) and language servers
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

### 18.5 `update_plan` sits with the capability adapters

**Current.** Six of the seven harness control tools live in
`crates/leveler-agent/src/injected_tools.rs`. `update_plan` is registered in
`leveler-tools`' `core_registry()` alongside `read_file` and `grep`.

**Desired.** Harness control tools are the harness's control protocol and
belong with the harness.

**Why it violates the constitution.** A future Review harness taking the
Coding capability adapters would inherit the Coding plan protocol with them.

**Minimal correction.** Move it next to the other injected tools.

**Risk.** Low.

### 18.6 `leveler-model` knows the Coding tool names

**Current.** `crates/leveler-model/src/tool_catalog.rs` hard-codes `grep`,
`find_files`, `find_symbol`, `read_symbol`, `find_references`, `list_files`,
`read_file`, `git_status`, `git_diff`, `view_image`, `web_search`,
`web_fetch`, `apply_patch`, `replace`, `run_command` and `shell_command`, and
derives from them an execution class (`Search` / `Read` / `Write`), a
replay-safety answer, a primary argument and an observe key.

Its real consumers are `leveler-agent` (observe key),
`leveler-client-protocol` (safe-replay decision) and `leveler-tui` (display).
The crate comment states the motive plainly: "execution policy must not
duplicate name lists and argument-field guesses across crates". The motive is
sound; the location is not.

**Desired.** `leveler-model` knows only `ToolDefinition`, `ToolCall`,
`ToolResult` and `ToolChoice`. Built-in Coding tool metadata belongs to the
harness or to the tool composition that owns those tools.

**Why it violates the constitution.** Rule 1, directly. A foundation primitive
enumerates product tools, and every harness built on `leveler-model` inherits
a Coding vocabulary.

**Minimal correction.** Move the catalog to the layer that owns the tool set,
and give the three consumers a way to reach it that does not run through a
foundation primitive. Note that one exported function, `is_search_tool`, has
no callers outside the crate.

**Risk.** Medium. Three consumers across three layers currently share this;
the replacement must not become three copies of the same list.

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

### 18.8 The verifier's doc comment claims completion authority

**Current.** `crates/leveler-verifier/src/lib.rs` opens with "Only the
verifier can mark a task complete".

**Desired.** The verifier is the authority for verification verdicts.

**Minimal correction.** Rewrite the doc comment. The code already separates
the axes; the comment predates the split.

**Risk.** None. It is a comment.

### 18.9 The engine shells out to git for the baseline

**Current.** `leveler-engine/src/baseline.rs` and `engine.rs` call
`Command::new("git")` directly, while `leveler-vcs` exists and performs zero
direct process spawns.

**Desired.** The engine asks the VCS capability, which asks the host
authority.

**Risk.** Low.

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

### 19.2 Is the canonical edit tool one tool or two?

`apply_patch` is the canonical structured edit and `write_file` is the
canonical whole-file write. `replace` overlaps both. The rationale it was built
on — an exact find/replace path for models that fail repeatedly at patch
context matching — is withdrawn (§1.1). What is left is a plain surface
question: does `replace` express a distinct model intent that `edit` and
`write` do not already cover? The default answer is now no.

### 19.3 Does dynamic model-controlled tool expansion pay for itself?

`expand_tools` buys schema tokens with an extra round and dynamic registry
state. Harness-side selection buys the same tokens with none of that, at the
cost of not adapting mid-session. Only evaluation settles which is worth more,
and the burden of proof is on the dynamic option because it is the one that
inverts ownership.

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

TOOL_IMPLEMENTATION_ALIGNED       NO
ENGINE_IMPLEMENTATION_ALIGNED     NO
CORE_PRIMITIVE_FOUNDATION_ALIGNED NO

SECOND_HARNESS_TEST               NOT_YET_ENFORCED
FOUNDATION_FROZEN                 NO
```

The architecture and the tool boundary are decided. The implementation is not
aligned to them, and this document says where. No source was changed to
improve any line of this table.
