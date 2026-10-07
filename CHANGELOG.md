# Changelog

Chinese version: [`CHANGELOG.zh-CN.md`](CHANGELOG.zh-CN.md)

Release notes: [`docs/RELEASE.md`](docs/RELEASE.md).

## [1.0.12] - 2026-10-07

Makes the terminal's execution presentation a frozen contract with one
authority, and gives a session one axis on every transport. The terminal, Web,
Desktop and App derive the same semantic tree from the same runtime facts, and
the work / bookkeeping classification behind the final answer lives once on the
wire instead of in four renderers. Auto permission now matches ordinary
development, and an interactive session is a Chat session on both transports.

### Added

- The execution presentation contract, frozen as a language-neutral fixture corpus (`testdata/execution_presentation/v1/`, C1..C14) that every surface checks itself against: the terminal as the reference implementation, plus Web, Desktop and App
- Tool rows grouped by the real execution round, with the observed concurrent burst kept as a batch inside it and the stage named while it runs
- A truthful round head: "all ok" only when every visible call succeeded, and `no_final_answer` for a prose-free turn instead of a green completion
- One answer lifecycle: a committed answer survives `update_plan` / `update_goal(complete)` bookkeeping, real work after it demotes it, and the classification reaches every surface as the stated `answer_effect` fact (an unrecognized tool counts as work)
- `/btw` side questions get their own observe-only surface and never join the main transcript, answer or plan
- Auto permission covers ordinary development: temporary files, process and system inspection, relocated read-only Git (`-C`, `--git-dir`, `--work-tree`) and ordinary network, while destructive operations still ask

### Changed

- `--permission` has no default value: omitted, a new session uses the project/default profile and a resume keeps the profile it persisted; supplied, it overrides it
- An interactive session is a Chat session on every transport (`leveler` / `leveler tui` over the daemon socket and `--in-process`); `leveler run` keeps the Goal default and resume keeps the persisted axis
- The client protocol is at minor 14 (major 1, unchanged); every added field is optional
- The narrow composer chip reserves collaboration and permission before shortening the model, and interim narration is visually subordinate to the answer
- Web, Desktop and App no longer render raw model reasoning as transcript content

### Fixed

- A reconnect lost the running tool's execution round, so a reconnected client re-guessed it from tool kinds and timing
- A command's failure reason could be a runtime note (`[execution policy] …`, `exit: N`, a timeout) instead of the command's own output, and the note was counted as output
- A busy row wider than the status strip collapsed to a bare spinner and dropped its elapsed, tool and token parts
- Prefixed text (`※ 回顾:`) wrapped by character count, clipping the wide glyph on a narrow terminal
- The narrow status chip dropped collaboration and permission before the model
- A protocol repair could reappear as a user-authored message in replayed history and in the session snapshot
- Pending approvals were not superseded when the permission profile changed, and the selected mode did not survive a resume
- A completed user shell's output tail was dropped when live delivery dropped
- Closeout folding could fold a message belonging to an earlier model round
- A runtime-host revival race could spawn a second runtime instead of adopting the one serving
- `leveler update` retried only one transient exec failure while validating a download

## [1.0.11] - 2026-10-02

Reworks how a model request is assembled, folded, and paid for: the control
context is separated from the transcript, every prompt segment has one source and
one authority, and every model attempt is recorded in the request ledger.

### Added

- A single request projection that wire encoding and context statistics both read, so the reported input size and the bytes sent cannot disagree
- Per-attempt request accounting: failures, retries, compaction summaries, and child task calls all land in the request ledger and the resource budget
- **CodeLeveler Desktop**, an Electron client in `apps/leveler-desktop/`. The renderer reaches the Electron main process only through a sandboxed preload with a fixed IPC surface, and the main process reaches the runtime only through the internal `leveler desktop-bridge` JSONL adapter, so runtime discovery, spawn, adopt, revive and handoff stay owned by the runtime host. It is built from the repository; the release archives still contain the `leveler` binary only
- **One Thinking Level for every model.** `/thinking` and `/thinking <level>` set `auto`, `off`, `minimal`, `low`, `medium`, `high` or `max` for the session, and `~/.leveler/config.toml` takes the same words, globally or per model. The default is `high`. `max` means "the strongest level this model declares" rather than a fixed provider value, so the setting tracks a model change instead of pinning a parameter one route happens to read

### Changed

- Every delivered prompt segment carries one source, authority, and lifecycle. A provider may change how a segment is represented on the wire but not its order, source, or authority
- Context folding pressure follows the actual projected request; directory-scoped project rules survive a fold verbatim
- Consumption after recovery is rebuilt from a scope's persisted request facts, so a background child cannot keep spending a stale balance snapshot
- Rule delivery is budgeted against the request, so every rule stays authoritative even when only part of a rule file fits verbatim
- An attachment import result carries the delivery identity of the command that produced it, so a client can match a stored upload, or its failure, to its own request instead of guessing by attachment name; the field is optional on the wire
- A background task started with the `runtime` lifetime is owned by the Execution Host rather than by the runtime generation that launched it, so a dev server or watcher survives an update and the replacement generation re-attaches to it
- `auto` means "no override" rather than CodeLeveler's own tuning: it no longer becomes the model profile's declared default, which is reserved for the calls the harness makes by itself. A level a model cannot express exactly is never rounded into a neighbouring one — it is unavailable, the request carries no override, and `leveler doctor` names the levels the model does have
- The session's Thinking Level reaches the main request and `/btw` side questions inherit it, while internal compaction keeps its own policy. `off` becomes a route-level disable instead of being lost between the executor policy and the request

### Fixed

- A reduced-context route whose `max_output_tokens` exceeds its own `context_window` reserved the full completion against the window, saturating the usable input capacity to `0` and aborting the run with `context management failure: ... capacity 0` after the first tool batch. A bound of zero is not a bound the model declared, so folding now follows the quality boundary alone
- The context snapshot produced before a request reaches the caller's observer instead of being dropped
- A model-spend scope change no longer discards the task epoch's remaining command budget
- The spend of a completed command is flushed when a batch is cancelled mid-round
- A child task's rendered spawn brief is persisted and reused on resume
- `/btw` read-only calls pass through the ToolHost admission pipeline
- Background task writes are constrained to the OS execution boundary, task mutation settles only after the whole workload ends, and live stdout/stderr channels are capped with an explicit truncation marker
- A memory listing reports the store failure that produced it instead of presenting a failed read as an empty list
- The status line and the web client show the canonical level (`high`, `max`) instead of a provider's parameter; `xhigh`, `reasoning_effort`, `output_config` and `budget_tokens` no longer have a path to what a user reads
- `SetThinkingLevel` on a deleted session answers `SessionNotFound` instead of updating zero rows and reporting success
- A repository model that declares no level resolves the global default, so the built-in `high` reaches the YAML/model load path too

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
