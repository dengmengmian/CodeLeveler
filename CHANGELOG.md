# Changelog

Chinese version: [`CHANGELOG.zh-CN.md`](CHANGELOG.zh-CN.md)

Release notes: [`docs/RELEASE.md`](docs/RELEASE.md).

## [1.0.11] - 2026-10-02

Reworks how a model request is assembled, folded, and paid for: the control
context is separated from the transcript, every prompt segment has one source and
one authority, and every model attempt is recorded in the request ledger.

### Added

- A single request projection that wire encoding and context statistics both read, so the reported input size and the bytes sent cannot disagree
- Per-attempt request accounting: failures, retries, compaction summaries, and child task calls all land in the request ledger and the resource budget
- **CodeLeveler Desktop**, an Electron client in `apps/leveler-desktop/`. The renderer reaches the Electron main process only through a sandboxed preload with a fixed IPC surface, and the main process reaches the runtime only through the internal `leveler desktop-bridge` JSONL adapter, so runtime discovery, spawn, adopt, revive and handoff stay owned by the runtime host. It is built from the repository; the release archives still contain the `leveler` binary only

### Changed

- Every delivered prompt segment carries one source, authority, and lifecycle. A provider may change how a segment is represented on the wire but not its order, source, or authority
- Context folding pressure follows the actual projected request; directory-scoped project rules survive a fold verbatim
- Consumption after recovery is rebuilt from a scope's persisted request facts, so a background child cannot keep spending a stale balance snapshot
- Rule delivery is budgeted against the request, so every rule stays authoritative even when only part of a rule file fits verbatim
- An attachment import result carries the delivery identity of the command that produced it, so a client can match a stored upload, or its failure, to its own request instead of guessing by attachment name; the field is optional on the wire
- A background task started with the `runtime` lifetime is owned by the Execution Host rather than by the runtime generation that launched it, so a dev server or watcher survives an update and the replacement generation re-attaches to it

### Fixed

- A reduced-context route whose `max_output_tokens` exceeds its own `context_window` reserved the full completion against the window, saturating the usable input capacity to `0` and aborting the run with `context management failure: ... capacity 0` after the first tool batch. A bound of zero is not a bound the model declared, so folding now follows the quality boundary alone
- The context snapshot produced before a request reaches the caller's observer instead of being dropped
- A model-spend scope change no longer discards the task epoch's remaining command budget
- The spend of a completed command is flushed when a batch is cancelled mid-round
- A child task's rendered spawn brief is persisted and reused on resume
- `/btw` read-only calls pass through the ToolHost admission pipeline
- Background task writes are constrained to the OS execution boundary, task mutation settles only after the whole workload ends, and live stdout/stderr channels are capped with an explicit truncation marker
- A memory listing reports the store failure that produced it instead of presenting a failed read as an empty list

## [1.0.0] - 2026-09-18

First stable release. From here CodeLeveler follows Semantic Versioning.

### Added

- Self-update: CodeLeveler checks the latest **stable** GitHub release at start-up, verifies its SHA-256, replaces the running binary, and restarts. Failures never block start-up
- `leveler update` (alias `upgrade`) to check and install manually, with `--check`, `--force`, and `--version <tag>`
- `/update` in the TUI, refused while a task is running
- `[update]` in `~/.leveler/config.toml`: `auto_update`, `check_interval_hours`

### Changed

- Release assets now follow the frozen `leveler-v<version>-<target>.tar.gz|zip` contract, each with a `.sha256` sibling; the release workflow refuses a tag that disagrees with the workspace version

### Fixed

- A long session with many tool calls could fail every later turn with `HTTP 400 invalid_request` ("Messages with role 'tool' must be a response to a preceding message with 'tool_calls'"). Context assembly now keeps each tool call with its results, a context snapshot that lost that pairing is ignored in favour of the saved transcript, and a request that still breaks it is refused before sending and reported as an internal conversation protocol error

## [0.1.0-beta.1] - 2026-09-17

Public beta for macOS, Linux, and Windows.

### Included

- Terminal UI (`leveler tui`), Web UI (`leveler web`), and CLI (`leveler run`)
- Sessions stored on your machine, so you can resume later
- Custom agents: a directory with `agent.yaml` and `instructions.md`
- Mobile app and host-side remote bridge (`leveler remote`)
- Approval for file writes and commands
- Command isolation: Seatbelt on macOS, bubblewrap on Linux, Low integrity on Windows

### Known limits

- Prebuilt binaries are not Apple- or Microsoft-signed. macOS or Windows may block the first run until you allow it
- Commands and config may still change before 1.0. `run`, `resume`, and `tui` are intended to stay stable; `eval` and `remote` may not
- Windows cannot deny network access per command. A command that requires that isolation is refused rather than run with the network open
- Windows has no local daemon socket. Sessions and `resume` still work
- A confined Windows `!command` prints its output when it finishes, not while it runs

Install and usage: [`README.md`](README.md).
How the system is layered: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).
