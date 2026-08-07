# CodeLeveler architecture

This document describes the stable boundaries of CodeLeveler. It intentionally
avoids source-line references and release-specific implementation counts so it
can remain useful as the workspace evolves.

## Design goals

CodeLeveler is not optimizing for the fewest crates or lines of code. It is an
agent core intended to remain dependable as long-lived workflow infrastructure:

1. **Single ownership.** Every state transition, completion decision, and side
   effect has one clear owner instead of being reimplemented by the engine,
   agent, and tool layers.
2. **Predictable behavior.** Safety, budgets, cancellation, retry, recovery,
   and termination are host semantics rather than model discretion.
3. **Long-running reliability.** After a crash, disconnect, cancellation,
   timeout, or restart, the runtime can determine what happened and whether it
   is safe to continue.
4. **Composable capabilities.** Planning, evidence, verification, delegation,
   and NPC behavior are policies layered above the direct loop, not hard-coded
   into it.
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
10. **Consistent multi-client behavior.** TUI, Web, and mobile are first-class
    clients of one runtime and share command, event, snapshot, approval, and
    cancellation semantics.

## Core

The product core is one job: **within host-enforced boundaries, let the model
use tools to finish work in a repository, with recoverable and auditable
state.** Everything else (TUI/Web, slash commands, skills, remote pairing) is
an entry or extension surface on top of that.

The core still consists of four pieces. Crate count does not define a boundary;
responsibility and data ownership do:

| Piece | Sole responsibility | Must not own | Where it lives |
| --- | --- | --- | --- |
| **1. Engine + direct loop** | The engine supervises task/turn lifecycle, recovery, and explicit termination. The agent loop only performs model → tool → result feedback. | Concrete tool behavior, duplicate continuation state machines, or hard-coded product planning policy. | `leveler-engine` + `leveler-agent` |
| **2. ToolHost / execution boundary** | Schema validation, risk and approval, path constraints, durable tool start/finish records, execution, and cancellation. | Conversation orchestration, task completion decisions, or UI state. | `leveler-tools` + `leveler-execution` |
| **3. Session state** | Persist messages, canonical events, snapshots, and migrations as ordered, auditable facts for recovery. | Agent policy or implicit product decisions. | `leveler-storage` + engine event log |
| **4. Model adaptation** | Provider-neutral request, streaming, and tool-call semantics above a thin provider/wire boundary. | Session state, tool permissions, or workflow decisions. | `leveler-model` + provider-internal protocol adapters |

The engine is a supervisor for long-running work, not merely a wrapper around
the loop. It must answer what is durable, which tool may already have produced
a side effect, whether retry is safe, where recovery resumes, and why execution
stopped. Canonical tool-event persistence is part of the side-effect protocol:
tool start is durably recorded before an external side effect, and completion
is durably recorded afterward. Display-only stream deltas may use a lossy path.

Not core implementation (keep thin and evolve independently): UI chrome, command menus, multi-phase
orchestration stacks, plugin marketplaces, and heavy product axes beyond what
the loop and gates already need. **Although TUI, Web, and mobile do not own core
execution logic, they are first-class supported product entry points and release
acceptance surfaces, not optional accessories.**

## Multi-client runtime contract

TUI, Web, and mobile connect through `leveler-client-protocol` and transports
to one engine; they do not implement separate task lifecycles. The client
contract covers:

- creating, continuing, and cancelling work, plus approval and clarification responses;
- subscribing to canonical runtime events and resyncing from a snapshot plus watermark;
- reconnecting and handing a session between clients without duplicate execution or lost progress;
- isolating sessions and projects so no client observes another project's events;
- protocol versioning, capability negotiation, authentication, and explicit incompatibility errors.

Closing or disconnecting any client does not cancel work already accepted by
the runtime. A client may persist view state, but task facts, permission
decisions, and execution state come only from canonical engine events and
snapshots. Mobile additionally enforces pairing, device revocation, and remote
approval restrictions at the host boundary; hiding a UI control is not an
authorization mechanism.

## Policies and complex work

Complex work does not justify growing the direct loop. The core supplies
durable lifecycle, safe execution, and explicit termination. Replaceable
workflow policies compose planning, evidence collection, stage checks, repair,
and delegation. A default policy may preserve today's product behavior, but no
policy may be required for execution safety or correct recovery.

```text
Complex task / NPC
    ↓
Workflow / Policy       planning, decomposition, checks, and retry
    ↓
Engine                  lifecycle, supervision, persistence, recovery, stop
    ↓
Agent Loop              one model-tool-result loop
    ├── Model Runtime    provider-neutral model calls
    └── ToolHost         approval, safe execution, durable side-effect records
```

Long-lived NPCs build on the same engine. Identity, long-term memory, wake-up
schedules, inboxes, world state, and character policy belong to an NPC runtime;
they neither duplicate the direct loop nor bypass ToolHost. Adding an NPC or a
workflow therefore cannot introduce a second session, permission, or recovery
model.

## Non-negotiable core invariants

- Every repository mutation and process execution crosses ToolHost/execution.
- Each lifecycle state has one writer; projections and UIs consume canonical events.
- Accepted work does not disappear because one UI disconnects.
- Recovery never blindly replays a tool that may already have caused an external side effect.
- Every stop has a typed reason: completed, blocked, cancelled, budget exhausted, or failed.
- Policies are replaceable; safety, persistence, and cancellation boundaries are not.
- Old configuration, databases, and events use explicit compatibility windows and migrations, never guessed repairs.
- Disconnecting TUI, Web, or mobile does not alter task facts; reconnect uses canonical snapshot/resync only.

This section is the normative target architecture. Gaps between it and the
current code must not be presented as implemented behavior.

Every crate forbids unsafe Rust except `leveler-execution`, which is
`#![deny(unsafe_code)]` with a single audited exception (the Linux
`PR_SET_PDEATHSIG` prctl call for orphan-process cleanup). The application and
CLI may use `anyhow` to add top-level context; reusable library crates expose
typed `thiserror` errors.

## Component map

```text
User
  │
  ├── leveler-cli ───────────────┐
  ├── leveler-tui                │
  └── leveler-web (browser UI)   │
          │                      │
          ▼                      ▼
  leveler-client-protocol   leveler-app  ◀── composition and configuration
          │                      │
  leveler-local-transport        ▼
          └──────────────▶ leveler-engine
                                  │
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

  Supporting libraries: leveler-storage, leveler-project, leveler-vcs,
  leveler-lsp, leveler-skills, leveler-memory, leveler-media, leveler-core,
  leveler-lifecycle (shared plan/evidence vocabulary across storage, engine,
  and agent), leveler-eval (offline evaluation harness)
```

The arrows represent dependency and call direction at a conceptual level. Some
composition edges are expressed through traits so high-level runtimes can be
tested with deterministic fakes.

## Runtime flow

### 1. Composition

`leveler-app` is the composition root. It resolves global and project
configuration, opens storage, builds provider and tool registries, selects the
execution policy, and wires the engine to either the CLI or local transport.

Environment access is concentrated in configuration and application setup.
Downstream libraries receive resolved values instead of reading process state
ad hoc.

### 2. Model request and streaming

The agent produces a provider-neutral `ModelRequest`. `leveler-provider` selects
the configured provider and model profile, while `leveler-protocol` converts the
request to and from the provider's wire format.

Streaming bytes follow this path:

```text
HTTP byte stream
  → SSE frame decoder
  → protocol chunk decoder
  → fragmented tool-call assembler
  → ModelEvent stream
  → engine and UI
```

The SSE decoder accepts arbitrary byte fragmentation. Tool-call arguments are
joined before JSON parsing; invalid or truncated JSON produces an error and is
never repaired into an executable call.

### 3. Turns and tool loop

`leveler-engine` owns task and turn lifecycle. **All product execution uses a
single path: the direct agent tool loop.** Long tasks stay on that path via
goal mode (`update_goal` until complete or blocked) and optional `spawn_agent`
fan-out. The multi-phase orchestrate stack (crate, CLI `plan`/`discuss`, dual
session mode) has been removed. Legacy log kinds named `orchestrate` are
accepted and run as direct.

Lifecycle vocabulary is shared through `leveler-lifecycle`, split into a
generic `runtime` module (`SessionStatus`, `TaskOutcome`, `TurnOutcome` — the
states any domain understands) and a Coding `workflow` module (`AgentState`
phase breadcrumbs, which refine runtime states but never redefine them). The
model may propose actions, but host code owns the state transition, resource
budget, cancellation, permission decision, and completion rules.

### 4. Tools and command execution

`leveler-tools` defines schemas and dispatch for built-in and MCP tools. Tool
arguments are schema-validated before execution. Write and command tools are
serialized where necessary to prevent conflicting mutations.

**Multi-agent.** The parent executor may advertise `spawn_agent` (depth 0,
delegation enabled). Several calls in one model turn run concurrently (default
caps: 4 concurrent, 6 total per top-level run; depth 1). Child tool start/finish
is re-emitted as attributed activity for clients; full child transcripts stay
out of the parent message list. See `docs/multi-agent.md`.

`leveler-execution` enforces the workspace boundary, sensitive-path rules,
approval policy, checkpoints, process-tree cancellation, and available OS-level
isolation. Filesystem decisions use host-resolved paths and trusted execution
intents; model input cannot select a more privileged backend.

Platform-specific execution controls include:

- Windows Job Objects, with AppContainer and ACL coordination where supported.
- macOS Seatbelt profiles.
- Linux Bubblewrap.

Capability detection is explicit. A platform without a required isolation
backend is not reported as fully sandboxed.

### 5. Verification and completion

`leveler-verifier` discovers or receives scoped format, build, and test commands.
It records evidence and classifies failures before the engine permits a task to
complete. Repair attempts are bounded and remain subject to the same permission
and resource limits as the original turn.

Verification is language-independent. Rust, Go, and TypeScript have deeper
built-in defaults; projects can provide commands for other stacks in
`.leveler/config.yaml`.

### 6. Persistence and reconnect

`leveler-storage` persists sessions and runtime state in SQLite. The local
runtime publishes normalized events through `leveler-client-protocol`; the TUI
can reconnect, request a snapshot, and continue from the current session state.

Task and session are distinct identities: a **task** (`TaskId`, `tasks` table)
is the engine-owned unit of work, a **session** is the conversation/client
aggregate. Today every task has exactly one primary session and the engine
records the association when a session is created or first runs; legacy
databases are backfilled deterministically (a legacy session's id is its task
id). Lifecycle columns stay on the session row while the relationship is 1:1 —
one writer per fact. The engine depends only on narrow storage ports
(`EventStore`, `TaskStore`, `SessionStore`, `TurnStore`, `MessageStore`,
`ModelRequestStore`, `TerminalStore`, bundled as `EngineStores`), each with an
in-memory implementation exercising the same contract tests; SQLite is the
production adapter wired by the composition root. Terminal task/turn facts
commit atomically inside the terminal port's adapter — the transaction
boundary is part of that port's contract, not the engine's concern.

The transport DTOs are separate from internal engine types so the local protocol
can evolve without exposing storage or provider structures.

## Important boundaries

### Provider boundary

Upper layers consume `ModelRequest`, `ModelResponse`, `ModelEvent`, and
`ModelError`. Vendor JSON, SSE chunk types, authorization headers, and endpoint
quirks stay below the protocol/provider boundary.

Adding another OpenAI-compatible endpoint is usually configuration-only. A new
wire format belongs in a protocol adapter, with provider configuration selecting
that adapter.

### Execution boundary

All repository mutation and process execution must pass through registered
tools and the execution layer. Direct filesystem or process access in the agent
loop would bypass approvals, checkpoints, redaction, and cancellation.

### Persistence boundary

Secrets may be sourced from an environment variable or an explicitly configured
local `api_key`, but resolved credentials and authorization headers must not be
written to session messages, runtime events, logs, or artifacts. Persistence
paths apply redaction before writes.

### UI boundary

The TUI renders client-protocol events and sends commands or interaction
responses. It does not own agent execution. On Unix, `leveler tui`'s default
path is discover-or-start: it probes the repository's daemon socket, starts a
detached `leveler serve` when none answers, and connects — so closing the TUI
does not cancel accepted work; shutting down the runtime does. `--in-process`
remains the explicit embedded mode (debug/fallback; also implied by
`--auto-approve` and `--config-dir`, which a running daemon cannot inherit).
Windows has no socket transport and keeps the embedded runtime.

Each runtime state directory owns a durable `RuntimeId` (persisted in
`<state_dir>/runtime-id`): a daemon restart keeps the same identity, and it
changes only when that state is explicitly re-initialized. Clients read it via
the local runtime contract (`RuntimeInfo`) for discovery/reconnect
verification and diagnostics. Exactly one daemon serves a state directory at a
time — the socket bind holds an exclusive lock file for the daemon's lifetime,
so racing starters elect one winner and the losers exit with an error.

`leveler-web` is the browser UI over the same seam: an axum server bridging a
single-page app to a `LocalRuntimeService` (in-process, or a `leveler serve
--tcp` daemon via `leveler web --connect`) through token-authenticated REST plus
one WebSocket. It is **loopback-only** by construction — `bind` refuses
non-loopback addresses — and every endpoint requires a 256-bit bearer token
compared in constant time; the frontend build is embedded at compile time. Off
-machine access (e.g. a phone) is expected to go through a tunnel that terminates
TLS and forwards to loopback, not by binding a public address. See
`crates/leveler-web/README.md`.

**Multi-project.** `leveler web` can aggregate several repositories in one UI.
The current repo keeps its in-process runtime; additional projects are served
by per-repo daemons. Opening a project probes the repo's daemon Unix socket
first (reusing e.g. a running `leveler tui` daemon); otherwise the web process
spawns `leveler --repo <path> serve --ready-json <file>` and connects over the
Unix socket once the readiness file appears — spawned daemons need no token. A
`RouterService` (itself a `LocalRuntimeService`) routes commands, snapshots,
and per-session event subscriptions by session→project mapping, so the REST
and WS layers see one facade; per-session WS subscriptions keep tabs on
different sessions or projects from seeing each other's traffic. The daemon
socket lives at `<home>/sock/<repo-path-hash>.sock` — short and stable, since
`sun_path` (~104 bytes on macOS) cannot fit the hashed state-dir path for deep
repos — and doubles as the ownership lock: `serve --tcp` binds it too, so a
second daemon on the same repo fails fast instead of reaping the first
daemon's active turns. (TCP mode reads its bearer token from
`LEVELER_DAEMON_TOKEN` — never argv.) The project registry at
`~/.leveler/web-projects.json` stores repository paths only; daemons that
outlive a web restart are rediscovered by socket probe, not by trusting pids.

## Extension points

- **Providers and protocols:** implement the model runtime and protocol adapter
  traits or configure a compatible endpoint.
- **Tools:** implement the tool trait, provide a JSON schema, declare risk and
  parallelism properties, and register the tool.
- **MCP:** configure external MCP servers without coupling their schemas to the
  core tool implementations.
- **Verification:** add project commands under `verify.format`, `verify.build`,
  and `verify.test`.
- **Skills:** add project skills under `.leveler/skills/` or user skills under
  the Leveler home directory.

## Configuration layers

| Layer | Path | Role |
| --- | --- | --- |
| Global | `~/.leveler/config.toml` | Default model, providers, MCP servers |
| Bundle | `configs/providers/`, `configs/models/` | Checked-in provider/model profiles |
| Project | `<repo>/.leveler/config.yaml` | Model override, permission profile, verify, ignore, readonly roots, limits |
| Permissions | `~/.leveler/permissions.yaml`, `<repo>/.leveler/permissions.yaml` | Durable allow/ask/deny rules |
| Hooks | `~/.leveler/hooks.yaml`, `<repo>/.leveler/hooks.yaml` | Pre/post tool external commands |

Annotated examples live next to this file (`*.example.yaml`,
`leveler-config-example.yaml`). The full global/bundle schema is
[`configs/example.yaml`](../configs/example.yaml).

## Repository guide

- `crates/` — Rust workspace crates.
- `configs/` — provider and model compatibility examples (bundle schema).
- `docs/` — architecture notes and annotated configuration examples.
- `evals/` — evaluation cases and harness documentation.
- `migrations/` — SQLite schema migrations.
- `.github/workflows/` — cross-platform CI and supply-chain checks.

中文说明见 [`README.zh-CN.md`](../README.zh-CN.md) 与
[`ARCHITECTURE.zh-CN.md`](ARCHITECTURE.zh-CN.md)。
