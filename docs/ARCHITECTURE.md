# CodeLeveler architecture

This document describes the stable boundaries of CodeLeveler. It intentionally
avoids source-line references and release-specific implementation counts so it
can remain useful as the workspace evolves.

## Design goals

CodeLeveler is designed around five constraints:

1. **Model-independent runtime.** Provider and wire-protocol differences do not
   leak into orchestration, tools, or the terminal UI.
2. **Single-direction dependencies.** User-facing layers compose lower-level
   libraries; foundational crates never depend back on applications.
3. **Typed failure boundaries.** Library crates preserve provider, protocol,
   tool, execution, storage, and verification errors as distinct types.
4. **Deterministic safety controls.** Path checks, permissions, limits,
   cancellation, and verification are enforced by host code rather than model
   instructions.
5. **Recoverable local state.** Sessions and runtime events can be persisted and
   resumed without requiring a remote control plane.

Every crate forbids unsafe Rust. The application and CLI may use `anyhow` to add
top-level context; reusable library crates expose typed `thiserror` errors.

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
          leveler-agent   leveler-orchestrator  leveler-verifier
                 │                │                 │
                 ├────────▶ leveler-context         │
                 └────────▶ leveler-tools ◀─────────┘
                                  │
                                  ▼
                         leveler-execution

  leveler-provider ─▶ leveler-protocol ─▶ leveler-model
         │                                      ▲
         └──────────── used by the engine ──────┘

  Supporting libraries: leveler-storage, leveler-project, leveler-vcs,
  leveler-lsp, leveler-skills, leveler-memory, leveler-media, leveler-core
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

### 3. Turns and orchestration

`leveler-engine` owns task and turn lifecycle. A direct run drives one agent
loop, while orchestrated runs add requirement extraction, localization, a task
graph, and review. Lifecycle vocabulary is shared through `leveler-lifecycle`.

The model may propose actions, but host code owns the state transition, resource
budget, cancellation, permission decision, and completion rules.

### 4. Tools and command execution

`leveler-tools` defines schemas and dispatch for built-in and MCP tools. Tool
arguments are schema-validated before execution. Write and command tools are
serialized where necessary to prevent conflicting mutations.

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
responses. It does not own agent execution. In daemon mode, closing the TUI does
not cancel accepted work; shutting down the runtime does.

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
