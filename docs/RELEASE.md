# CodeLeveler 1.0.11

Chinese: [`RELEASE.zh-CN.md`](RELEASE.zh-CN.md)

This release rebuilds how a model request is assembled, folded, and paid for.
The control context is now separate from the transcript, every delivered prompt
segment has one source and one authority, and every model attempt is recorded in
the request ledger.
Reasoning also gets one user-facing vocabulary: a Thinking Level names the
intent (`auto`, `off`, `minimal`, `low`, `medium`, `high`, `max`) rather than a
provider's parameter, `/thinking` sets it for a session, the config file takes
the same words globally or per model, and the default is `high`.
Installed stable builds can update automatically, or use `leveler update`
or `/update`.

## Added

- **CodeLeveler Desktop**, an Electron client in `apps/leveler-desktop/`.
  Tasks, conversation, tools, approvals and persistence still come from the Rust
  runtime. The renderer reaches the Electron main process only through a
  sandboxed preload with a fixed IPC surface, and never receives runtime
  credentials or Node access; the main process reaches the runtime only through
  the internal `leveler desktop-bridge` JSONL adapter, so runtime discovery,
  spawn, adopt, revive and handoff stay owned by the runtime host. It is built
  from the repository (`npm ci` and `npm start` in `apps/leveler-desktop`); the
  release archives still contain the `leveler` binary only.
- The desktop client provides task navigation, a conversation view with safe
  Markdown, a closable workbench for plan, changes and manual browser tabs,
  per-session model and permission menus, and bounded attachment upload.
- The desktop bridge checks every command it forwards against its own explicit
  bound: a PNG-only image attachment up to 20 MiB with a 64-hex digest and
  1..=2048 pixel dimensions, a non-empty bounded query id, a bounded
  observability window, a validated agent name, and session-scoped model,
  permission, rename and archive commands. Anything else is refused.
- **A Thinking Level every model shares.** `/thinking` and `/thinking <level>`
  set `auto`, `off`, `minimal`, `low`, `medium`, `high` or `max` for the
  session, and `~/.leveler/config.toml` takes the same words, globally or per
  model. The default is `high`. `max` means "the strongest level this model
  declares" rather than a fixed provider value, so the setting tracks a model
  change instead of pinning a parameter one route happens to read. The picker
  offers one entry per distinct effect, so `high` and `max` are never listed as
  two names for one request, and the status line reads the canonical level.

## Changed

- Prompt construction gives every delivered segment a single source, authority,
  and lifecycle. Providers may change how a segment is represented on the wire
  (a system message or a top-level system field) but not its order, source, or
  authority, and the control context no longer travels as part of the transcript.
- Wire encoding and context statistics read the same projected request, so the
  reported input size and the bytes actually sent cannot disagree. Folding
  pressure follows the projected request, project rules scoped to a directory
  survive a fold verbatim, and historical reasoning follows the route's replay
  contract.
- Every model attempt is recorded in the request ledger and the resource budget,
  including failures, retries, compaction summaries, and child task calls.
  Consumption is rebuilt from a scope's persisted request facts on recovery, so a
  background child can no longer keep spending a stale balance snapshot.
- Rule delivery is budgeted against the request: every rule stays authoritative
  even when only part of a rule file fits verbatim.
- An attachment import result carries the delivery identity of the command that
  produced it, so a client can match a stored upload, or its failure, to its own
  request instead of guessing by attachment name. The field is optional on the
  wire and absent for raw-send imports.
- A background task started with the `runtime` lifetime is owned by the
  Execution Host rather than by the runtime generation that launched it, so a
  dev server or watcher survives an update: the launching generation retires
  without stopping it, and the replacement generation re-attaches to it. An
  Execution Host from an earlier protocol major keeps managing the services it
  already runs; cross-owner control stays refused instead of being downgraded.
- The repository's `./dev` development entry point is documented in the release
  archives, for local verification and release qualification.
- `auto` means "no override" rather than CodeLeveler's own tuning: it no longer
  becomes the model profile's declared default, which is reserved for the calls
  the harness makes by itself (compaction, memory extraction) and for an
  explicit native request from the eval seam or an agent manifest. A level a
  model cannot express exactly is never rounded into a neighbouring one — it is
  unavailable, the request carries no override, and `leveler doctor` names the
  levels the model does have.
- The session's Thinking Level reaches the main request, `/btw` side questions
  inherit it, and internal compaction keeps its own policy. `off` becomes a
  route-level disable instead of being lost between the executor policy and the
  request, and a new session starts from the configured default rather than
  inheriting the previous conversation's override.

## Fixed

- A reduced-context route can declare `max_output_tokens` larger than its own
  `context_window`. Reserving that full completion against the window used to
  saturate the usable input capacity to `0`, so every request looked over the hard
  capacity and the run aborted with `context management failure: ... capacity 0`
  after the first tool batch. A bound of zero is not a bound the model declared,
  so folding now follows the quality boundary alone. Routes whose reservation
  fits their window are unchanged.
- The context snapshot produced before a request reaches the caller's observer
  instead of being dropped.
- A model-spend scope change no longer discards the task epoch's remaining
  command budget.
- The spend of a completed command is flushed when a batch is cancelled
  mid-round, instead of being lost with the cancelled round.
- A child task's rendered spawn brief is persisted on the child spec and reused
  on resume, so a resumed child sees the brief it started with.
- `/btw` read-only calls pass through the ToolHost admission pipeline instead of
  bypassing it.
- Background task writes are constrained to the OS execution boundary, task
  mutation settles only after the whole workload ends, and the live stdout and
  stderr channels are capped with an explicit truncation marker rather than
  growing without bound.
- A memory listing reports the store failure that produced it, instead of
  presenting a failed read as an empty list.
- The status line and the web client show the canonical level (`high`, `max`)
  instead of a provider's parameter; `xhigh`, `reasoning_effort`,
  `output_config` and `budget_tokens` no longer have a path to what a user reads.
- `SetThinkingLevel` on a deleted session answers `SessionNotFound` instead of
  updating zero rows and reporting success.
- A repository model that declares no level resolves the global default, so the
  built-in `high` reaches the YAML/model load path too, and explicit `auto`
  never warns and never becomes `high`.

## Known limits

- Prebuilt binaries are not Apple- or Microsoft-signed. macOS or Windows may
  block the first run until you allow it
- Windows cannot deny network access per command. A command that requires that
  isolation is refused rather than run with the network open
- Windows has no local daemon socket. Sessions and `resume` still work
- A confined Windows `!command` prints its output when it finishes, not while
  it runs

Install and usage: [README](../README.md).
Updates: [README § Updates](../README.md#updates).
How the system is layered: [Architecture](ARCHITECTURE.md).
