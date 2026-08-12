# CodeLeveler Architecture

This document describes the stable boundaries of CodeLeveler against the
**current codebase** (baseline `main@04a015b` and later, unless a section is
explicitly marked otherwise). It intentionally avoids source-line references and
release-specific counts so it remains useful as the workspace evolves.

## How to read this document

Every architectural claim is one of four kinds. Do not mix them:

| Marker | Meaning |
| --- | --- |
| **CURRENT** | Implemented and present in the tree today. |
| **TARGET** | Intended next ownership shape; not fully done. |
| **KNOWN DEBT** | Confirmed structural gap between CURRENT and TARGET. |
| **FUTURE** | Product direction without a full implementation. |

Gaps must not be marketed as shipped capabilities. Shipped capabilities must not
be described as “planned only.”

Companion docs:

- [`TUI_ARCHITECTURE.md`](TUI_ARCHITECTURE.md) — CURRENT TUI ownership contract
  (geometry / conversation / presentation) after the post-`eceb271` hardening.
- [`TUI_ARCHITECTURE_AUDIT.md`](TUI_ARCHITECTURE_AUDIT.md) — pre-hardening audit
  at `eceb271` (historical; geometry ownership has since landed).

---

## Design goals

CodeLeveler is not optimizing for the fewest crates or lines of code. It is a
**local-first coding-agent runtime** meant to stay dependable as long-lived
workflow infrastructure:

1. **Single ownership.** Every state transition, completion decision, and side
   effect has one clear owner instead of being reimplemented by the engine,
   agent, and tool layers.
2. **Predictable behavior.** Safety, budgets, cancellation, retry, recovery,
   and termination are host semantics rather than model discretion.
3. **Long-running reliability.** After a crash, disconnect, cancellation,
   timeout, or restart, the runtime can determine what happened and whether it
   is safe to continue.
4. **Composable capabilities.** Planning, evidence, verification, delegation,
   and (FUTURE) NPC behavior are policies layered above the direct loop, not
   hard-coded into it.
5. **Model-independent runtime.** Provider and wire-protocol differences do not
   leak into orchestration, tools, or user interfaces.
6. **Unbypassable safety boundary.** Paths, permissions, sandboxing, limits,
   cancellation, and dangerous-command policy are enforced by host code.
7. **Recoverable and auditable state.** Sessions, tool side-effect boundaries,
   and runtime events can be persisted and resumed independently of a process
   lifetime or remote control plane.
8. **Evolvable interfaces.** Internals remain free to simplify while public
   CLI, configuration, storage, and extension surfaces follow explicit
   compatibility and migration rules.
9. **Single-direction dependencies and typed errors.** Applications compose
   lower-level libraries; foundational crates never depend back on applications,
   and library boundaries preserve distinguishable failure types.
10. **Consistent multi-client behavior.** Terminal, browser, and (FUTURE)
    desktop/mobile clients share one runtime contract: commands, events,
    snapshots, approvals, and cancellation.

---

## Vocabulary

| Term | Meaning (authoritative) |
| --- | --- |
| **Engine** | Long-running kernel / supervisor (`leveler-engine`): task/turn lifecycle, EventLog, persist-before-forward, recovery, resume, explicit termination, ownership fencing. |
| **Agent Loop** | Model ↔ tool executor (`leveler-agent`). Not session durability, transport, or UI. |
| **Tool** | Model-facing action registered in `leveler-tools` (`Tool` trait + `ToolRegistry`). |
| **Host Execution** | Workspace safety, permissions, process execution, sandbox, checkpoint, artifacts (`leveler-execution`). |
| **Task** | Engine-owned unit of work (`TaskId`). |
| **Turn** | One bounded execution slice inside a task. |
| **Session** | Conversation / client aggregate; today 1:1 with a primary task. |
| **EngineEvent** | Canonical domain fact produced by the engine (persisted unless transient). |
| **RuntimeEvent** | Client-facing projection of runtime facts (`leveler-client-protocol`). |
| **ClientCommand** | Client intent into the runtime (submit, cancel, approve, …). |
| **ClientOrigin** | Command provenance: `Local` \| `Remote` \| `RemoteTimeout`. **Not** User/Agent/System. |
| **ExecutionKind** | Task/session execution strategy: `Direct` \| `Parallel`. **Not** Tool/Shell/MCP/Capability. |
| **Workflow / Policy** | Replaceable planning/check/retry composition above the engine. |
| **NPC** | Long-running runtime / workflow / policy domain (FUTURE productization). **Not** a UI client. |
| **MCP** | CURRENT tool integration: discovered tools adapted to `Tool`, exposed as `mcp__<server>__<tool>`. |
| **User Shell Execution** | CURRENT `!command`: user-initiated direct command, **no** LLM/agent loop (hard-gated by tests), reuses host execution safety via `CommandRunner::run_streaming`. |
| **Capability / Extension** | FUTURE extension surface (may provide model tools, user capabilities, workflows, hooks). Not shipped. |

Do **not** invent a second name such as `ExecutionOrigin` that merges
`ClientOrigin` with action initiator (User / Agent / Policy) unless an ADR
lands in code.

For Tool / Shell / MCP / Capability distinctions in prose, prefer neutral
phrases: **invocation type**, **operation type**, **execution surface** — not a
new Rust enum name that collides with `ExecutionKind`.

---

## Core architecture

The product core is one job: **within host-enforced boundaries, let the model
use tools to finish work in a repository, with recoverable and auditable
state.** Everything else (TUI, Web, slash commands, skills, remote pairing) is
an entry or extension surface on top of that.

Crate count does not define a boundary; responsibility and data ownership do:

| Piece | Sole responsibility | Must not own | Where it lives (CURRENT) |
| --- | --- | --- | --- |
| **1a. Engine** | Task/turn lifecycle, EventLog, recovery, resume, explicit stop, durable runtime facts. | Concrete tool behavior; UI; provider wire formats. | `leveler-engine` |
| **1b. Agent Loop** | Model → tool call → host runs tool → tool result → model. | Session durability; transport; UI; product planning policy. | `leveler-agent` |
| **2. ToolHost / execution** | Schema validation, risk/approval, path constraints, durable tool start/finish, process/FS execution, cancellation. | Conversation orchestration; task completion policy; UI state. | `leveler-tools` + `leveler-execution` |
| **3. Session state** | Persist messages, canonical events, snapshots, migrations as ordered auditable facts. | Agent policy or implicit product decisions. | `leveler-storage` + engine EventLog |
| **4. Model adaptation** | Provider-neutral request, streaming, tool-call semantics. | Session state, tool permissions, workflow decisions. | `leveler-model` + provider/protocol adapters |

**Do not merge Engine and Agent Loop in prose.** The engine supervises; the
agent executes one model–tool feedback loop.

### Target layering (all clients)

```text
                         CLIENTS (CURRENT + FUTURE)

               TUI       Web       APP (FUTURE)
                │         │         │
                └─────────┼─────────┘
                          │
                leveler-client-protocol
                ClientCommand / RuntimeEvent / Snapshot
                          │
                          ▼
                 APPLICATION LAYER  (leveler-app)
                          │
        ┌─────────────────┼──────────────────┐
        │                 │                  │
 Interactive coding   Projection        Interaction
        │                 │                  │
        └─────────────────┼──────────────────┘
                          │
                          ▼
                     ENGINE KERNEL
                    leveler-engine
         Lifecycle / EventLog / Recovery / Persistence
                          │
              ┌───────────┴───────────┐
              │                       │
              ▼                       ▼
      Interactive Coding     NPC Runtime (FUTURE)
                             Workflow / Policy
              │                       │
              └───────────┬───────────┘
                          │
                          ▼
                      AGENT LOOP
                    leveler-agent
                          │
                          ▼
                    MODEL TOOLS
                    leveler-tools  (+ MCP adapters)
                          │
                          ▼
             HOST EXECUTION BOUNDARY
                leveler-execution
          Filesystem / Process / External I/O
```

**CURRENT today:** TUI, Web, remote host bridge (`leveler-remote-agent`),
Engine, Agent Loop, tools + MCP, execution, client protocol, persistence.

**FUTURE:** full desktop/mobile APP experience, User Shell (`!command`), NPC
runtime productization, Capability / Extension framework.

Do not invent crates that do not exist (`leveler-npc`, `leveler-capability`,
…). Logical layers may later be extracted *if* ownership requires it.

---

## Runtime kernel (Engine)

**CURRENT.** `leveler-engine` is the long-running kernel / supervisor:

- Task lifecycle and turn lifecycle
- Append-only **EventLog** with **persist-before-forward**
- Recovery, resume, reaping after restart
- Explicit termination with typed outcomes
- Ownership fencing and durable runtime facts
- `ExecutionKind`: `Direct` | `Parallel` (task/session strategy)

It must be able to answer: what is durable, which tool may already have caused
a side effect, whether retry is safe, where recovery resumes, and why work
stopped.

Canonical tool-event persistence is part of the side-effect protocol: tool
start is durably recorded before an external side effect; completion is
durably recorded afterward. Display-only stream deltas may use a lossy path.

Lifecycle vocabulary is shared via `leveler-lifecycle` (generic runtime states
plus Coding workflow breadcrumbs that refine — never redefine — those states).

---

## Agent Loop

**CURRENT.** `leveler-agent` is the **Agent Executor / Agent Loop**:

```text
Model → Tool Call → Host executes tool → Tool Result → Model
```

It is **not**:

- Session durability owner
- Transport owner
- UI owner
- Provider-specific protocol owner

Top-level product execution uses the **direct agent tool loop**. Long tasks
stay on that path via goal mode (`update_goal` until complete or blocked) and
optional `spawn_agent` fan-out. Legacy log kinds named `orchestrate` are
accepted and run as direct.

**A goal spans bounded work windows, not one giant turn.** The per-turn
100-round ceiling ends a *work window*, not the goal: the supervisor
(`DefaultSupervisorPolicy::after_turn`) opens the next window
(`DriveGoalAgain`, a fresh objective-restated turn) instead of failing. Windows
are bounded twice — an in-memory cross-window no-progress counter
(`MAX_NO_PROGRESS_WINDOWS`, reset by material workspace change) under the
absolute `MAX_SUPERVISED_TURNS` ceiling — so a stuck goal converges rather than
spins. A window that exhausts the round budget is `BudgetLimited` (incomplete,
resumable), **not** `Failed`. Goal identity is the session; the window budget is
in-memory (daemon-crash window recovery is future work, not current).

---

## Host execution boundary

**CURRENT.** `leveler-execution` owns:

- Workspace safety and path resolution
- `PermissionProfile`, `RiskLevel`, approval policy
- Process execution (`CommandRunner`), sandbox backends, cancellation
- Checkpoints, background processes, artifacts

**Execution hosts in the daemon, not the client.** Approval is a per-session
policy (`ApprovalPolicy`, carried on the trusted-local `CreateSessionRequest`):
`--auto-approve` selects an unattended session on the daemon rather than forcing
an in-process runtime, so a long-running goal survives the client disconnecting
and a reconnect resumes the still-running turn. A remote/web client cannot
elevate its own session to auto-approve — the boundary force-resets it to
interactive.

Platform controls (capability-detected, never over-claimed):

- Windows: Job Objects; AppContainer / ACL where available
- macOS: Seatbelt
- Linux: Bubblewrap; audited `PR_SET_PDEATHSIG` orphan cleanup

**Invariant:** every repository mutation and process execution crosses this
boundary — including FUTURE User Shell (`!command`), Capability, and Extension
tools. User-initiated work does **not** skip host safety.

Distinguish:

- **Model authorization** (what the model is allowed to *request*)
- **Host authorization** (what the host actually permits)

The model never elevates its own privileges.

---

## Client runtime contract

**CURRENT.** `leveler-client-protocol` is the stable cross-client contract — not
a future design.

| Surface | Role |
| --- | --- |
| `ClientCommand` | Client intent into the runtime |
| `RuntimeEvent` | Normalized client-facing fact |
| `UiSessionSnapshot` | Reconnect / resync truth (+ watermark) |
| `InteractiveRuntimeClient` | `send` / `subscribe` / `snapshot` |
| Protocol version / envelope | Major compatibility; additive minors |

Clients **must not** reimplement task lifecycle. Closing a client does not
cancel work already accepted by the runtime. View state (scroll, expand,
selection) may be client-local; task facts, permissions, and execution state
come only from engine facts and snapshots.

Transports may vary; the contract does not:

- In-process (`InProcessRuntimeClient`)
- Local daemon / Unix socket (`leveler-local-transport`)
- Session wire / WebSocket (`leveler-session-wire`, Web)
- Remote bridge (`leveler-remote-agent` + `leveler-remote-protocol` / relay)

Do **not** invent a parallel “Presentation Protocol”, “UI Protocol”, or
“Frontend Event Bus” unless a concrete requirement forces it.

`ClientOrigin` (`Local` | `Remote` | `RemoteTimeout`) is **command provenance**
for audit and remote-approval timeout pressure — not authorization and not
action initiator (User / Agent / System).

---

## Application layer

**CURRENT.** `leveler-app` is the composition root: config, storage open,
provider/tool wiring, engine attachment, interactive session use-cases, and
projection from engine facts toward clients.

`InProcessRuntimeClient` currently concentrates many responsibilities (session
runtime config, event streams, approvals, clarifications, media, checkpoints,
live view, active turns, steering, command dispatch). That concentration is
documented under [Current architecture debt](#current-architecture-debt--evolution-seams).

Environment access is concentrated in configuration and application setup;
downstream libraries receive resolved values.

---

## Clients

### TUI (**CURRENT**)

`leveler-tui` is a first-class terminal client. It speaks only
`leveler-client-protocol`. It does **not** own agent logic, tool execution,
permission decisions, or persistence truth.

On Unix, default `leveler tui` is discover-or-start against the repository
daemon socket; closing the TUI does not cancel accepted work. `--in-process`
is explicit embedded mode. Windows keeps the embedded runtime (no socket
transport yet).

TUI interaction correctness (historical disclosures, mouse, duration truth,
PTY validation) is closed. Conversation geometry ownership has been hardened
(see [TUI architecture](#tui-architecture)); residual maintainability seams
remain in the debt section.

### Web (**CURRENT**)

`leveler-web` is **already** a runtime client — not a future concept. An axum
server bridges a SPA to `LocalRuntimeService` via token-authenticated REST +
WebSocket carrying `ClientCommand` / `RuntimeEvent` / snapshots. Loopback-only
bind; 256-bit bearer token; multi-project via per-repo daemons and
`RouterService`. See `crates/leveler-web/README.md`.

### Remote / APP (**CURRENT bridge, FUTURE full product**)

`leveler-remote-agent` is the **host-side remote bridge**. It deliberately does
**not** depend on `leveler-web`; it works through `leveler-session-wire` and the
runtime/client protocol boundary. That infrastructure already exists for a
future desktop/mobile APP experience.

Do **not** write “add Remote APP architecture from zero.” Full Desktop APP and
mobile product UX remain **FUTURE**.

---

## Workflow and NPC runtime

Complex work does not justify growing the direct loop. The core supplies
durable lifecycle, safe execution, and explicit termination. Replaceable
**workflow / policy** composes planning, evidence, stage checks, repair, and
delegation.

```text
Complex task / NPC (FUTURE productization)
    ↓
Workflow / Policy       planning, decomposition, checks, retry
    ↓
Engine                  lifecycle, supervision, persistence, recovery, stop
    ↓
Agent Loop              one model–tool–result loop
    ├── Model Runtime
    └── ToolHost / execution
```

### NPC is not a UI client

**Wrong:** TUI | Web | APP | NPC as peer “interfaces.”

**Right:**

- **Clients / interfaces:** TUI, Web, APP
- **NPC:** long-running runtime / workflow / policy domain above the same Engine

NPC **must** reuse Engine, ToolHost, permission, persistence, recovery, and the
event model. It must **not** grow a second agent loop, session model, permission
model, or recovery model.

NPC productization is **FUTURE**. Policy hooks and lifecycle primitives that
such a runtime would sit on are **CURRENT** building blocks.

---

## Event and projection model

| Layer | Role |
| --- | --- |
| **EngineEvent** | Canonical domain fact (engine-owned). |
| **RuntimeEvent** | Client-facing projection (protocol-owned). |

**TARGET** dependency direction:

```text
Engine → EngineEvent → single application projection → RuntimeEvent → TUI / Web / APP
```

UI must not depend on the engine’s internal event model.

### CURRENT transitional path (**KNOWN DEBT**)

```text
EngineEvent
  → engine_event_to_agent()   (leveler-app)
  → AgentEvent
  → EventBridge
  → RuntimeEvent
```

This shim is real code today. It is **not** the ideal architecture and must not
be described as the end state. Collapsing to a single application projection is
**TARGET** core hardening — not a shipped cleanup.

---

## Persistence and recovery

**CURRENT.** Canonical task facts, execution facts, messages, event log, and
snapshots belong to runtime/persistence (`leveler-storage` + engine EventLog).

Task and session are distinct identities: a **task** is the engine-owned unit of
work; a **session** is the conversation/client aggregate. Today every task has
exactly one primary session. The engine depends on narrow storage ports
(`EventStore`, `TaskStore`, `SessionStore`, …) bundled as `EngineStores`.

UI transient state (scroll, expanded, selection, focus, viewport, drag) is
**never** canonical task truth. Client-local view preferences may be saved as
client-local state only.

Secrets may come from env or local `api_key`, but resolved credentials and
Authorization headers must not enter session messages, runtime events, logs, or
artifacts.

---

## Tool and MCP system

### Tools (**CURRENT**)

`leveler-tools`:

- `Tool` trait, `ToolRegistry`
- Schema validation before dispatch
- Model-facing registration for built-ins and adapters

### MCP (**CURRENT**, not future-only)

MCP tools are dynamically discovered, adapted to `Tool`, registered, and
exposed as:

```text
mcp__<server>__<tool>
```

Future enhancement (lifecycle, reconnect, extension/capability integration) is
optional product work — MCP **already exists** as tool integration.

### Execution surface (**CURRENT**)

All mutations and process execution still go through `leveler-execution`. Write
and command tools serialize where needed to avoid conflicting mutations.

**Multi-agent:** parent may advertise `spawn_agent` (depth 0, delegation on);
concurrent children with attributed activity events; child transcripts stay out
of the parent message list. See `docs/multi-agent.md`.

---

## TUI architecture

Full CURRENT ownership map: [`TUI_ARCHITECTURE.md`](TUI_ARCHITECTURE.md).

```text
TUI
  → Client Protocol
  → local presentation state
  → Components
  → Ratatui
```

TUI must not own: agent logic, tool execution, permission decisions, persistence
truth.

### Conversation subsystem (**CURRENT** after geometry hardening)

Conversation owns authoritative geometry and related view concerns:

- View state, geometry, viewport, scroll, auto-follow
- Hit test, selection mapping, line/hit cache, bottom alignment

Renderer and reducer must **not** each recompute viewport geometry (that
duplication historically caused “click A expands B”).

### Workbench

Owns top-level layout and component composition. Does **not** own Conversation
internal scroll math, duplicated screen/content coordinates, tool semantic
classification, or runtime execution logic.

### Disclosure

Tool disclosure presentation exists today (`presentation::disclosure` as a
domain-free visual; `activity_stream` as the Agent Tool adapter).

**TARGET:** the same disclosure visual language can present Agent Tool, User
Shell, and future Capability rows via adapters. User Shell / Capability are
**not** wired yet.

---

## Extension direction (**FUTURE**)

Extension / Capability is **not** fully implemented. Do not document it as
supported.

Intended semantic shape only:

```text
Extension
   └── may provide
         ├── Model Tool
         ├── User Capability
         ├── Workflow
         └── Hook
              └── Host execution / safety boundary
```

Future Capability must not bypass permission, workspace, cancellation, audit,
or execution.

This document does **not** freeze: `ExtensionHost`, `CapabilityRegistry`,
manifest schema, WASM ABI, JSON-RPC plugin protocol, or a marketplace. Those
need real implementation before they become architecture facts.

Shipped extension points today remain:

- Providers / protocol adapters
- Registered tools + MCP servers
- Verification commands
- Skills
- Hooks (pre/post tool external commands)

---

## Current architecture debt / evolution seams

Only structural gaps confirmed in code. Not a wishlist.

### 1. TUI geometry ownership — largely addressed

| | |
| --- | --- |
| **Was (pre-hardening at `eceb271`)** | workbench / reducer / AppState all knew conversation rect, viewport, scroll, auto-follow, hit-test, screen→content mapping, selection, cache — risk of “click A expands B.” |
| **CURRENT (`04a015b`+)** | `conversation::{geometry,view,build,viewport,interaction}` is the single owner; workbench composes; reducer asks interaction for hit meaning. Documented in `TUI_ARCHITECTURE.md`. |
| **Remaining** | Dual expand semantics (`tools_expanded` vs per-group `expanded`); first-frame geometry fallback; User Shell presentation adapter not present. |
| **Status** | Geometry single-owner: **DONE**. Residual maintainability items: **KNOWN SEAM**. |

### 2. EngineEvent projection shim — addressed

| | |
| --- | --- |
| **CURRENT** | `EngineEvent` → `EventBridge` (leveler-app) → `RuntimeEvent`, with an EXHAUSTIVE match: a new `EngineEvent` variant fails to compile until it gets a projection decision. Sixteen table-driven equivalence tests pin client-visible shapes. |
| **Legacy** | `engine_event_to_agent` remains as a marked one-way adapter for the headless CLI renderer (`run_in_session`/`resume_session`) and eval collectors (`run_in_session_bounded`, `eval_signals`) only. Never a UI path. |
| **Status** | **DONE** (core hardening). |

### 3. Dynamic tool metadata — addressed

| | |
| --- | --- |
| **CURRENT** | `Tool::name/description` return `&str` borrowed from the instance; the registry owns its keys (`BTreeMap<String, _>`); `McpTool` owns its discovered strings. Zero `Box::leak` in production code. Built-in tools unchanged. |
| **Status** | **DONE** (core hardening). Reconnect/reload can rebuild registries without accumulating. |

### 4. ToolContext growth — addressed

| | |
| --- | --- |
| **CURRENT** | Three facets by lifecycle: `execution` (process-wide execution + write-safety infra), `policy` (gates/budgets; the scope-varying part — the two security-loosening switches are private behind `grant_network()` / `grant_unrestricted_fs()`), `services` (LSP/artifacts/memory/background). Anti-growth rule documented on the type: a new field must name its lifecycle and join a facet. |
| **Placement guide** | Extension-provided service / secret provider → `services`; remote executor → `execution`. |
| **Status** | **DONE** (core hardening). Enforcement semantics unchanged. |

### 5. InProcessRuntimeClient responsibilities — partially addressed

| | |
| --- | --- |
| **CURRENT** | `CheckpointStore` owns both checkpoint maps and their joint invariant; `LiveViews` owns reconnect state with a pure fold; `stage_turn` is the single turn-launch preamble. The facade routes commands and delegates (2739 → 2501 lines, 12 state fields). |
| **Remaining** | Delivery middleware, session directory CRUD, runtime-config store, media/memory arms stay inline (stateless or single-path; extraction not yet justified by the rule "own state + own invariant + multiple paths"). Per-session map eviction on delete remains a KNOWN SEAM. |
| **Status** | **CORE CLUSTERS DONE**; remainder tracked. A new client use case = a small handler + facade route, not another inline block. |

### 6. Structured client events vs preformatted copy — policy set, migration started

**The rule** (binding for new work): a stable product fact crosses the wire as
a typed `RuntimeEvent`; clients own wording, layout, and locale. Free-form
diagnostics (unexpected errors, transport failures, model/tool output) stay
`Notification`/`String`. A domain fact is never a preformatted Chinese UI
string.

| | |
| --- | --- |
| **Migrated** | `ContextCompacted { from, to }` and `ContextExpanded { from_tokens, to_tokens, reason }` (protocol 1.4, additive; schemas + envelope golden regenerated; TUI localizes ZH/EN; Web mirror updated). Also fixed: the TUI reason-localizer matched `budget exhausted` (space) while the executor emits `budget_exhausted` — users saw the raw machine token. |
| **Remaining** | AgentActivity advisory labels, turn-incomplete reason defaults, and assorted `interactive.rs` notices remain preformatted — tracked, migrate opportunistically under the rule above. |
| **Status** | **POLICY IN FORCE; HIGH-CONFIDENCE FACTS DONE** |

---

## Non-negotiable invariants

- Every repository mutation and process execution crosses ToolHost/execution.
- Each lifecycle state has one writer; projections and UIs consume canonical events.
- Accepted work does not disappear because one UI disconnects.
- Recovery never blindly replays a tool that may already have caused an external side effect.
- Every stop has a typed reason: completed, blocked, cancelled, budget exhausted, or failed.
- Policies are replaceable; safety, persistence, and cancellation boundaries are not.
- Old configuration, databases, and events use explicit compatibility windows and migrations.
- Disconnecting TUI, Web, or a remote client does not alter task facts; reconnect uses snapshot/resync only.
- FUTURE User Shell / Capability / Extension still pass host execution safety.

### Unsafe Rust

Most crates use `#![forbid(unsafe_code)]`. **`leveler-execution` is different:**
`#![deny(unsafe_code)]` with a single audited, scoped exception — the Linux
`PR_SET_PDEATHSIG` pre-exec hook for orphan-process cleanup. Application and CLI
may use `anyhow` for top-level context; library crates expose typed `thiserror`
errors.

---

## Component map

```text
User
  │
  ├── leveler-cli ────────────────┐
  ├── leveler-tui                 │
  └── leveler-web (browser UI)    │
          │                       │
          ▼                       ▼
  leveler-client-protocol    leveler-app  ◀── composition / config / projection
          │                       │
  leveler-local-transport         ▼
  leveler-session-wire     leveler-engine
  leveler-remote-agent            │
  leveler-remote-protocol         │
  services/leveler-relay          │
                 ┌────────────────┼─────────────────┐
                 ▼                ▼                 ▼
          leveler-agent                    leveler-verifier
                 │                                  │
                 ├────────▶ leveler-context         │
                 └────────▶ leveler-tools ◀─────────┘
                                  │
                                  ▼
                         leveler-execution

  leveler-provider ─▶ leveler-protocol ─▶ leveler-model
         │                                      ▲
         └──────────── used by the engine ──────┘

  Supporting: leveler-core, leveler-lifecycle, leveler-storage,
  leveler-project, leveler-vcs, leveler-lsp, leveler-skills,
  leveler-memory, leveler-media, leveler-eval, leveler-test-support
```

Arrows are conceptual dependency/call direction. Some edges are traits so
runtimes can be tested with deterministic fakes.

---

## Runtime flow

### 1. Composition

`leveler-app` resolves global and project configuration, opens storage, builds
provider and tool registries, selects execution policy, and wires the engine to
CLI, in-process client, or local transport.

### 2. Model request and streaming

The agent produces a provider-neutral `ModelRequest`. `leveler-provider` selects
provider/model; `leveler-protocol` converts to wire format.

```text
HTTP byte stream
  → SSE frame decoder
  → protocol chunk decoder
  → fragmented tool-call assembler
  → ModelEvent stream
  → engine and clients
```

Invalid or truncated tool-call JSON errors out; it is never “repaired” into an
executable call.

### 3. Turns and tool loop

Engine owns task/turn lifecycle; agent runs the direct tool loop; host code owns
transitions, budgets, cancellation, permissions, and completion rules.

### 4. Tools and command execution

Schema validation in `leveler-tools`; host enforcement in `leveler-execution`.

### 5. Verification and completion

`leveler-verifier` discovers or receives format/build/test commands, records
evidence, and classifies failures. Repair attempts are bounded.

- `format`: best-effort, does **not** gate completion
- `build` / `test`: **gate** completion
- Any field under project `verify` **replaces** the whole auto-discovered plan

### 6. Persistence and reconnect

SQLite-backed storage; clients resync via snapshot + event watermark.

Each runtime state directory owns a durable `RuntimeId`. Exactly one daemon
serves a state directory (socket bind + lock).

---

## Important boundaries

### Provider boundary

Upper layers consume `ModelRequest` / `ModelResponse` / `ModelEvent` /
`ModelError`. Vendor JSON, SSE quirks, and auth headers stay below protocol.

### Execution boundary

No direct filesystem or process access from the agent loop that would bypass
approvals, checkpoints, redaction, or cancellation.

### Persistence boundary

Redact credentials before any durable write.

### UI boundary

Clients render protocol events and send commands. They do not own agent
execution. Multi-project Web behavior (daemon probe, spawn, `RouterService`,
registry at `~/.leveler/web-projects.json`) is documented under Clients → Web.

---

## User Shell Execution (**CURRENT**)

`!command` is implemented (TUI entry point):

```text
!git status
  → TUI submit routing (raw first char `!`; never trimmed prose)
  → ClientCommand::RunUserShell (protocol 1.5, additive)
  → leveler-app use case (ActiveTurns foreground mutex; hang guard)
  → CommandRunner::run_streaming — the SAME host boundary as agent shell
      (permission-profile write confinement / network sandbox / env scrub /
       process-tree termination / workspace-root cwd)
  → canonical EngineEvent facts (Started/Output*/Finished; Output transient,
      others persisted LocalOnly) → session EventLog (turn_id = None)
  → exhaustive EventBridge projection → RuntimeEvent UserShell*
  → TUI UserShell block (reuses presentation::disclosure) + Shell Details
```

Hard-gated by tests: ZERO model requests per execution; neither the command
nor its output ever enters model context. Cancellation is per-execution by
`UserShellId` (`CancelUserShell`), never `CancelCurrentTurn`. Reconnect
restores active + bounded history via `UiSessionSnapshot.user_shells`.
MVP is non-interactive shell execution (`sh -c` / `cmd /C`, stdin closed) —
not a PTY/terminal emulator; TTY programs (vim/top/interactive ssh) are out
of scope. Remote policy denies both commands. Runtime restart follows the
existing process-cleanup policy (no cross-process adoption).

---

## Next architecture phases (ordering)

1. Architecture documentation alignment *(this document)*
2. Residual TUI maintainability seams (expand dual-state, shell presentation)
3. Core architecture hardening (EngineEvent → RuntimeEvent projection; related)
4. Full regression
5. User Shell Execution (`!command`)
6. Real-project dogfooding
7. Capability / Extension only from observed needs

Do not schedule Capability ahead of User Shell without new evidence.

---

## Configuration layers

| Layer | Path | Role |
| --- | --- | --- |
| Global | `~/.leveler/config.toml` | Default model, providers, MCP |
| Bundle | `configs/providers/`, `configs/models/` | Checked-in provider/model profiles |
| Project | `<repo>/.leveler/config.yaml` | Model override, permission profile, verify, ignore, readonly roots, limits |
| Permissions | `~/.leveler/permissions.yaml`, project file | Durable allow/ask/deny |
| Hooks | `~/.leveler/hooks.yaml`, project file | Pre/post tool external commands |

Examples: `*.example.yaml`, `leveler-config-example.yaml`,
[`configs/example.yaml`](../configs/example.yaml).

---

## Repository guide

- `crates/` — Rust workspace
- `configs/` — provider/model profiles
- `docs/` — architecture and examples
- `evals/` — evaluation harness
- `migrations/` — SQLite migrations
- `services/leveler-relay` — remote relay service
- `.github/workflows/` — CI

中文版：[`ARCHITECTURE.zh-CN.md`](ARCHITECTURE.zh-CN.md)。入口：[`README.md`](../README.md)。
