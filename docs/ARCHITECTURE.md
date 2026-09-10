# CodeLeveler Architecture

The canonical architecture document. `AGENTS.md` states the constitution in
short form and points here; this file is where the boundaries are defined and
where the gap between the current implementation and the target foundation is
recorded honestly.

Chinese version: [`ARCHITECTURE.zh-CN.md`](ARCHITECTURE.zh-CN.md).

Everything below was verified against the workspace at commit `494ed1b`
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

Six sentences carry the whole model:

```text
The Kernel does not know the product.
The Harness defines domain semantics.
The Engine owns lifecycle, not agent intelligence.
The Host Authority exclusively owns controlled side effects.
Every durable fact has one authoritative owner.
Mechanical Truth is not Semantic Satisfaction and is not User Acceptance.
```

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
└──────────┬──────────────────────────┬───────────────────────┘
           │                          │
           └────────────┬─────────────┘
                        ▼
┌─────────────────────────────────────────────────────────────┐
│                      AGENT RUNTIME                          │
│   leveler-agent-core          Tool runtime contracts        │
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
  §17.1.
- **Tool contracts and concrete capabilities are one crate.** The box labelled
  "Tool runtime contracts" and most of the "Reusable capabilities" row are both
  reached through `leveler-tools`. See §17.2.

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
web, CLI or any other product concept.

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

## 5. Tool runtime

Two different concerns are worth separating in the reader's mind, because the
code does not separate them yet.

**Tool runtime contracts** — what a tool *is*:

```text
Tool trait          ToolSchema          ToolRegistry
ToolCall            ToolResult          ToolContext contract
ToolHost contract   admission           dispatch contract
```

**Concrete agent capabilities** — what tools there *are*: `read_file`,
`list_files`, `grep`, `apply_patch`, `replace`, `run_command`,
`shell_command`, `find_symbol`, `find_references`, `diagnostics`,
`blast_radius`, `git_status`, `git_diff`, browser, memory, skills, web fetch
and search, image viewing, task control, and MCP-discovered tools.

### Current state

`leveler-tools` holds both. `src/tool.rs` and `src/registry.rs` are the
contract; `src/tools/` is 29 concrete capabilities. The crate depends on
`leveler-browser`, `leveler-context`, `leveler-execution`, `leveler-lsp`,
`leveler-memory`, `leveler-project` and `leveler-skills` — those edges belong
to the concrete tools, not to the contract.

The concrete coupling shows in `ToolContext`: `ToolServices` names
`lsp_sessions`, `artifact_store`, `memory_root`, `background_tasks` and
`browser` as struct fields. Any implementer of the `Tool` trait — including a
future Review-specific tool that needs none of them — takes that whole shape.
The fields are `Option`, so a caller *can* pass `None`; the type still carries
every capability into every tool.

`ToolRegistry` itself composes freely: `ToolRegistry::new()` plus `register`,
with `core_registry()` and `full_registry()` as two prebuilt selections. A
different harness can build a different registry today.

### Target state

```text
leveler-tool-core   → Tool, ToolSchema, ToolRegistry, ToolCall, ToolResult,
                      ToolContext contract, ToolHost contract, admission,
                      dispatch contract
leveler-tools       → the concrete built-in capabilities
```

**This split is a stated target, not scheduled work.** Do not create
`leveler-tool-core` to make this document look finished. The split earns its
place when a second harness actually needs the contract without the
capabilities — see §16.

Note that the kernel already has its own narrower seam: `ToolRuntime` in
`leveler-agent-core` needs only the tool definitions the model may see plus a
way to turn a `ToolCall` into text. `leveler-tools` sits behind that seam, not
inside the kernel.

---

## 6. Host execution authority

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

There must not be a second path from a harness or a tool to the filesystem or
to a process. Do not duplicate security policy between the tool layer and the
execution layer.

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

Two distinct things sit in that table, and conflating them would misstate the
boundary:

1. **Model-requested effects on the user's repository.** These all go through
   `Workspace` and `CommandRunner`. `leveler-agent` reaching zero is the
   meaningful number.
2. **The runtime's own state and sidecars.** Writes under the Leveler home
   (memory, skills, project state, browser driver) and long-lived sidecar
   processes (MCP servers, language servers, the browser driver) do not pass
   through the permission/approval path, because they are not something the
   model asked for.

That second category is a real, deliberate boundary — but the sidecars are
outside `CommandRunner`'s process-tree termination and sandbox semantics. See
§17.4.

---

## 7. Reusable capabilities

These are capabilities a harness may select, not parts of the kernel:

| Crate | Capability |
| --- | --- |
| `leveler-context` | Bounded repository context assembly: map, candidate files, related tests, merged project rules, token estimate, repeated-read guard. |
| `leveler-project` | Project language detection and filesystem layout (config and state locations). |
| `leveler-memory` | Durable project memory store and its promotion pipeline. |
| `leveler-skills` | Skill discovery and loading. |
| `leveler-vcs` | Git operations, performed through the execution authority. |
| `leveler-lsp` | Language-server sessions, reused across tool calls. |
| `leveler-browser` | Browser runtime, driver install, isolated per-project profile. |
| `leveler-media` | Media handling. No internal dependencies. |

A Coding harness selects context, project, VCS, LSP, browser, memory,
filesystem mutation and process execution. A Review harness would plausibly
select context, project, VCS, LSP, read-only filesystem and memory, and skip
the rest. Do not bind the full capability set into the kernel to save a
harness the trouble of choosing.

---

## 8. Persistent runtime

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
engine". The split makes each code path declare which half it reads. It does
not yet remove the Coding dependency — see §17.1.

---

## 9. Storage and durable truth

`leveler-storage` is the durable truth boundary: SQLite, embedded migrations,
the connection pool, and one repository per concern. Business logic never
issues SQL directly.

```text
A persistent fact
    → has one authoritative owner
    → has one canonical durable representation
```

This applies with no exceptions to task status, turn status, evidence,
ownership, usage, artifacts and completion state. There must be no parallel
source of truth.

`leveler-storage` depends only on `leveler-core` and `leveler-lifecycle`. That
is what lets the low-level persistence crate speak the lifecycle vocabulary
without a back edge to a high-level crate — the reason the vocabulary lives in
its own crate at all.

---

## 10. Verification and evidence

`leveler-verifier` runs the project's declared checks — format, build, test —
captures evidence, checks scope, and classifies failures.

The precise statement of its authority:

```text
The verifier is the authority for VERIFICATION VERDICTS.
The verifier is NOT the authority for semantic task completion.
```

It can prove that the configured checks passed, failed, or were blocked. It
cannot, alone, prove that the user's intent was satisfied.

A verification command a user declared explicitly is authority, not a
heuristic input — the discovery layer marks it as such and the harness may not
substitute its own guess for it.

This is why `TaskOutcome` and `VerificationStatus` are separate axes in
`leveler-lifecycle`. `TaskOutcome::Completed` means the model declared the goal
complete; `VerificationStatus` says what the project's own checks reported
about the final tree. The runtime reports both and never folds them into one
word.

The crate-level doc comment in `leveler-verifier/src/lib.rs` still says
otherwise. See §17.3.

---

## 11. Harness layer

`leveler-agent` is the **Coding Harness**. The crate name has not changed and
this document does not propose changing it; the concept is what matters.

It owns Coding domain semantics:

```text
coding prompt                     compaction strategy
repository context strategy       goal semantics
coding tool selection             coding verification policy
write workflow                    delegation policy and sub-agent profiles
ownership of paths across agents  coding completion contract
```

It reaches the kernel through one seam: `Drive` in
`src/executor/drive.rs` implements `leveler_agent_core::AgentHarness`. The
seams it fills are `tool_definitions`, `on_round_start`, `on_round_admitted`,
`on_response`, `on_model_error`, `on_quiet`, `execute_calls`, `on_stop` and
`on_event`. That is the entire kernel contract, and it is already exercised by
a real harness rather than being a hypothetical extension point.

A future Review harness is a **sibling**:

```text
              leveler-agent-core
                /            \
               ▼              ▼
        leveler-agent    leveler-review
        Coding Harness   Review Harness
```

The edge `leveler-review → leveler-agent` is forbidden. Review reuses the
foundation, not the Coding product.

Review would own its own vocabulary — `ReviewTarget`, `ReviewScope`,
`ReviewPolicy`, `Finding`, `FindingSeverity`, `FindingEvidence`,
`FindingLifecycle`, deduplication, suppression, `ReviewVerdict`,
`ReviewReport` — and none of those types may enter the agent kernel.

None of this is a commitment to build a Review harness. It is a constraint on
what the foundation is allowed to assume.

---

## 12. Product layer

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
a task is complete; that word has one owner, and clients read it.

The client protocol is what keeps this honest: UI code depends on
`leveler-client-protocol` and never on the concrete runtime, providers, tools
or storage. `leveler-tui` proves it — its dependencies are the client
protocol, core, model and skills, and nothing else.

---

## 13. Dependency direction

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

No lower layer may depend on a user-facing one.

Current graph, by topological level (normal dependencies only, dev-dependencies
excluded):

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
  `leveler-cli`, `leveler-tui` or `leveler-web`. The direction rule holds.
- `leveler-agent-core` depends on exactly one internal crate. The kernel is as
  narrow as the constitution asks.
- `leveler-agent → leveler-execution` is a **vocabulary** edge, not an
  execution edge: the harness uses `PermissionProfile`, `RiskLevel`,
  `WriteScope` and `HookRunner` as types. Its direct side-effect count is zero.
- `leveler-engine → leveler-agent` is the one edge that contradicts the layer
  model. It is the debt in §17.1.

---

## 14. Runtime turn flow

One turn, end to end:

```text
client command
    │
    ▼
leveler-app                  composition; maps config to a running Application
    │
    ▼
leveler-engine               opens a turns row, stamps messages with the turn
    │                        id, wires the persist-before-forward EventLog,
    │                        wraps the approver and clarifier as recorders
    │
    ├─ ExecutorFactory       one derivation of the execution configuration
    │                        from the resolved policy and turn profile
    ▼
leveler-agent (Drive)        the Coding harness: prompt, context, tool
    │                        selection, delegation, compaction, goal semantics
    ▼
leveler-agent-core           the loop: admit round → assemble model round →
    │                        stream → parse → dispatch tools → next round,
    │                        under budgets, deadline and cancellation
    ▼
leveler-tools                the concrete tool runs
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

Facts flow out only after they are durable. That ordering is the reason a
client can never show a state the runtime cannot reproduce after a restart.

---

## 15. Truth and authority model

```text
Mechanical Truth  ≠  Semantic Satisfaction  ≠  User Acceptance
```

**The runtime authoritatively proves:**

```text
a command executed        a file was mutated
its exit code             a test result
a build result            an artifact exists
an event was persisted    a tool returned this result
observed runtime state
```

**The verifier authoritatively decides:** the configured verification verdict.

**Neither of these implies the next step.**

```text
"the tool succeeded"  does not prove  "the goal is semantically satisfied"
"the tests passed"    does not prove  "the user's request is fulfilled"
```

The model performs semantic judgement. The user holds final acceptance.

```text
Runtime owns mechanical truth.
Model owns semantic interpretation.
User owns acceptance.
```

Do not reintroduce a mechanical shortcut that stands in for semantic
completion — an `observed_the_changed_tree()` style predicate that reads "the
tree changed" as "the work is done". No such predicate exists in the codebase
today, and `TaskOutcome` / `VerificationStatus` being orthogonal axes is what
keeps it out.

---

## 16. The Second Harness Test

The foundation's architecture acceptance test.

> Can a semantically different agent product — Review, for instance — be built
> on this foundation **without modifying the agent kernel**?

Target answers:

| Question | Required answer |
| --- | --- |
| Modify `leveler-agent-core` | NO |
| Reuse the model and runtime vocabulary | YES |
| Reuse the tool contracts | YES |
| Reuse selected capabilities | YES |
| Add Review-specific semantics | YES |
| Add Review-specific tools | ALLOWED |
| Depend on the Coding harness | NO |

If a future Review harness turns out to require a change to
`leveler-agent-core`, that is a **foundation leak**. The response is to
analyse why, not to add the new business concept to the kernel.

### Current verdict: NOT YET ENFORCED

The kernel side passes. `leveler-agent-core` depends only on `leveler-model`,
carries no product vocabulary, and its `AgentHarness` seam is already
implemented by a real harness. A Review harness could implement the same trait
without touching it.

The foundation around it does not pass yet:

- A Review harness that wants persistence, resume, event ordering and recovery
  has to go through `leveler-engine`, which depends on `leveler-agent` and
  whose public API names `CodingTaskSpec` (§17.1).
- A Review harness that wants tool contracts also takes the concrete
  capability set and the `ToolServices` shape (§17.2).

Neither of these forces a kernel change, which is why this is "not yet
enforced" rather than "fail". They are the inputs to Foundation Hardening.

**Do not modify code to convert this verdict to PASS as part of a
documentation change.**

---

## 17. Known boundary debt

Recorded, not hidden. Each item states the current behaviour, the desired
boundary, why it violates the constitution, the minimal correction, and the
risk of making it.

### 17.1 The engine depends on the Coding harness

**Current.** `leveler-engine → leveler-agent`. The engine's public API exports
`CodingTaskSpec`, and `ExecutorFactory` constructs a
`leveler_agent::Executor` directly. `recorders.rs`, `recovery.rs`, `turn.rs`
and `policy_resolver.rs` all name `leveler_agent` types.

**Desired.** The engine runs a harness executor behind an abstraction; it does
not name a domain. `TaskSpec` carries a runtime half and a domain half, and the
engine reads only the runtime half.

**Why it violates the constitution.** Rule 5 (the engine owns runtime
mechanics, not product semantics) and rule 2 (harnesses are siblings): a
second harness inherits the Coding harness through the engine.

**Minimal correction.** The `RuntimeTaskSpec` / `CodingTaskSpec` split already
exists and is described in the source as the migration seam. The next step is
an executor abstraction the engine can drive without naming `leveler_agent`,
with `ExecutorFactory` moving above the engine.

**Risk.** Medium. `ExecutorFactory` is deliberately the single derivation of
execution configuration; splitting it badly reintroduces the multiple-
derivation bug it was built to remove.

### 17.2 Tool contracts and concrete capabilities share a crate

**Current.** `leveler-tools` holds the `Tool` trait, `ToolRegistry` and
dispatch alongside 29 concrete tools, and depends on browser, context,
execution, LSP, memory, project and skills. `ToolServices` names
`lsp_sessions`, `artifact_store`, `memory_root`, `background_tasks` and
`browser` as fields of the context every tool receives.

**Desired.** `leveler-tool-core` holds the contract; `leveler-tools` holds the
capabilities. A harness takes the contract without the capability graph.

**Why it violates the constitution.** Rule 3. A Review-specific tool needing
none of those services still takes the whole shape.

**Minimal correction.** Extract the contract when a second harness needs it —
not before. The registry already composes freely, so the practical cost today
is the `ToolContext` shape rather than the tool set.

**Risk.** Low if deferred, medium if done speculatively: a contract crate
designed against one consumer usually has to be redesigned for the second.

### 17.3 The verifier's doc comment claims completion authority

**Current.** `crates/leveler-verifier/src/lib.rs` opens with "Only the
verifier can mark a task complete".

**Desired.** The verifier is the authority for verification verdicts. Task
outcome and verification status are orthogonal, which is what
`leveler-lifecycle` implements.

**Why it violates the constitution.** Rule 6. This is the one place in the
tree where a document still asserts that a green check is completion.

**Minimal correction.** Rewrite the doc comment. No behaviour change — the
code already separates the axes; the comment predates the split.

**Risk.** None. It is a comment.

### 17.4 Sidecar processes bypass the command runner

**Current.** MCP servers (`leveler-tools/src/mcp.rs`), the browser driver
(`leveler-browser/src/driver.rs`) and language servers
(`leveler-lsp/src/client.rs`, `registry.rs`) are spawned with `Command::new`
directly rather than through `leveler_execution::CommandRunner`.

**Desired.** Either these run under the host authority's process-tree
termination and sandbox semantics, or the exemption is an explicit, named
policy rather than an accident of implementation.

**Why it partially violates the constitution.** Rule 4. These are not
model-requested commands, so the permission and approval path does not apply.
They are still host processes outside the authority that is supposed to own
every host process.

**Minimal correction.** Name the category — "runtime sidecar" — and give it a
defined lifecycle owner, rather than three independent spawn sites.

**Risk.** Low to medium. Sidecar lifetime is already entangled with daemon
shutdown reaping; changing the spawn path touches that.

### 17.5 The engine shells out to git for the baseline

**Current.** `leveler-engine/src/baseline.rs` and `engine.rs` call
`Command::new("git")` directly to stamp the base commit, while
`leveler-vcs` exists and performs zero direct process spawns.

**Desired.** The engine asks the VCS capability, which asks the host
authority.

**Why it violates the constitution.** Rule 4, and it puts a domain operation
(git) in the runtime layer.

**Minimal correction.** Route the baseline read through `leveler-vcs`.

**Risk.** Low. It is a single read-only invocation.

---

## 18. Architecture change rules

1. **Extend above the foundation before modifying it.** A change that can live
   in a harness or a product belongs there.
2. **A foundation change needs evidence**, not elegance: two real
   implementations, a real dependency-inversion boundary, an observed coupling
   or ownership defect, or an independent protocol / security / persistence /
   runtime boundary.
3. **Answer the decision test in §1 of `AGENTS.md` before you start**, in the
   pull request, not afterwards.
4. **Do not describe the target as the present.** If a change moves toward a
   boundary without reaching it, update §17 rather than deleting the entry.
5. **Do not add speculative interfaces for products that do not exist.** The
   architecture must permit a Review harness. It must not pre-build one.
6. **This document is the only canonical architecture.** Do not create
   `ARCHITECTURE_V2.md`, `FOUNDATION_*.md` or a "final" variant. Amend this
   file; the Chinese version tracks it.

Roadmap items — multi-agent direction, browser direction, a Review product,
cloud, ACP, remote workers, NPC workflows, future providers, future UI — are
not architecture. This document may describe an extension point. It does not
promise a feature.
