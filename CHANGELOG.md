# Changelog

Chinese version: [`CHANGELOG.zh-CN.md`](CHANGELOG.zh-CN.md)

Release notes: [`docs/RELEASE.md`](docs/RELEASE.md).

## [Unreleased]

Durable memory becomes a first-class, default-on product surface when the
project enables it, and historical credential-shaped records stop crossing
display and archive boundaries.

### Added

- Memory is part of the first request when `memory.enabled` is true: `memory`, `remember` and `forget` plus the guidance, catalog and recall no longer require a `capability(enable)` round trip. Approval rules for memory writes are unchanged
- `/memory status` reports active/pending/archived/sensitive counts, the durable extraction queue, accepted/rejected/skipped totals, the last extraction result, the oldest retry and its sanitized error, and the last recall count
- `leveler memory audit` lists historical credential-shaped records without printing their content; `leveler memory scrub [--id <id>] [--apply]` is a dry-run by default and redacts only when `--apply` is explicit
- A Chat turn can declare an explicitly delegated task with the injected `commit_task` tool. The turn binds to a durable Goal (identity, turn WAL and budget scope) without changing the session's `chat` axis, then uses the existing Goal closeout, verification, cancellation and resume contract. The tool is offered only to top-level authoring Chat turns, never to Plan/read-only surfaces or sub-agents
- The TUI shows the task badge and goal-mode status while a delegated task runs, keeps a paused task visible, and restores it from the session snapshot on reconnect

### Changed

- `remember` now records model-inference authority with `agent_remember_tool` provenance. Human approval authorizes the write; it does not claim the user said the fact, and the entry can never replace a fact the user wrote directly
- `memory.enabled = false` removes the memory pack before capability disclosure, so Full Access cannot expose memory tools, guidance, catalog or recall. Automatic interactive candidate extraction also obeys the switch
- Background extraction tolerates a reasoning model's first-token latency (90 s) instead of converting a healthy call into a retry at 20 s
- Resume treats `turn.payload.goal_id` as sufficient Goal identity, so an interrupted delegated Chat turn resumes under the Goal profile without changing the session axis

### Fixed

- Sensitive-material validation covers title, body, tags and provenance evidence on every write path
- Forget, supersede and expiry redact credential-shaped content before writing an archive; a non-sensitive record is unchanged
- CLI, TUI, Web projections, notifications and logs no longer print a historical sensitive title or body; clients receive a redacted marker and the `sensitive` flag

## [1.0.20] - 2026-10-19

Shell commands become a first-class inline surface, their complete output stays
reachable, and a user's turn reads as one full-width band.

### Added

- Shell Mode: `!` on an empty composer is a mode marker, with offline Tab completion for files, directories and PATH executables, a scrolling candidate menu driven by keyboard, pointer and wheel, and one inline record per command. Each record carries a status rail, `$ command`, stdout/stderr provenance, a live tail and measured duration; stopping is available from both the record and the live activity row, and `Ctrl+C` cancels exactly that execution
- Complete shell output is retained: the exact stdout/stderr and the arrival-order combined stream are written to per-execution capture files, and `/output` or `/copy output` reach the full result. The in-TUI viewer indexes the capture and renders only the visible window, so a 50 MiB result costs the same per frame as a small one; rows can be selected and copied with tabs preserved and soft wraps rejoined
- `Ctrl+B` hands a running command to the existing background registry with the same execution id, process and process group. Output, exit code and duration stay continuous, the transfer is durable, and a cold restore reports an orphan without a terminal fact as unknown rather than running

### Changed

- A user's turn is one full-width background band: the dark theme uses the existing quiet `#171A1E` step above the `#111315` canvas, the soft-blue `❯` opens the first row, and short and long messages share the same width. The band is exactly as tall as the message; there is no identity label, border or divider, and AI prose stays on the canvas
- Local commands render inline in the conversation. The separate Shell Details screen is removed; `Ctrl+O` reaches the latest execution, and a submitted command returns the viewport to the live edge
- Selected transcript text is copied from the authored source rather than the painted glyphs: `❯`, status rails, list markers and code fences never reach the clipboard
- The background-wait row's marker animates with the shared 120 ms spinner while the runtime reports the wait target running; reduced motion keeps it static

### Fixed

- Long shell output no longer keeps only a 1 MiB tail as its terminal fact; the complete capture is retained and remains reachable after folding
- Background user-shell protocol types and their exported schemas stay current, and remote policy classifies `background_user_shell` explicitly

## [1.0.19] - 2026-10-19

Web search becomes a capability independent of the conversation model.

### Added

- `[web.search]` selects a search backend — the DeepSeek official Search API or Tavily — independently of `[providers.*]` and `[models.*]`. Switching the conversation model, including to another vendor, does not change it, and a model provider's `base_url` is never used as the search endpoint
- `web_search` reads its credential only from the configured `api_key_env`; the variable is scrubbed from child processes and its value never reaches logs or tool output

### Changed

- Only `[web.search]` enables search. The legacy `LEVELER_SEARCH_API_KEY` fallback is removed
- A configured search whose credential variable is unset is logged at warn level, naming the variable, instead of a debug line the default filter never wrote
- A Homebrew-managed install no longer replaces itself in the Cellar: the start-up updater and `leveler update` defer to `brew upgrade leveler`, while `--check` stays read-only. Script and manual installs keep self-updating
- The TUI's bottom state strip is four independent cells — runtime status, the running background count (`后台 2 ↗`), active issues (`⚠ 后台失败 1 ↗`, or a failed turn named in full as `✕ …`), and the plan indicator — each built from its own owner's real state, each painted range its own click target (jobs list / issue page / plan page). A background task's raw command no longer sits on the footer and a failed task no longer raises a standing banner: the failure is an active issue while its exit code and output stay on the task's own page, and looking at one marks it read without clearing it. Issue state derives from the runtime's own `Error` status and the background projection; an ordinary tool failure stays in the execution timeline
- The TUI's bottom chrome spends one row on the turn's state: the runtime status keeps the live label, elapsed time and tool/token counts on the left, and the plan indicator sits on the right of the same row — its own violet identity, a breathing dot while the turn runs, a static green ✓ when declared complete, red ✕ on failure, muted ○ for a last record. Plan progress is stated once (the runtime line no longer repeats the current step), and the whole indicator, arrow included, is the click/Enter target for the Plan page

### Fixed

- The TUI shows the first search result instead of the external-data marker
- Dogfood coverage persists this run's layer artifacts before taking the pre-RC verdict, so a passed L1 test is no longer reported as "L1 did not run for this candidate"; evidence that declares another candidate is refused

## [1.0.15] - 2026-10-13

Makes a stalled provider answer diagnosable and recoverable, and settles how the
conversation presents work: one receipt per visible stage, reasoning only in the
status strip, and details on demand.

### Added

- A response body that cannot be read records what the transport had already seen — HTTP status, declared content type and encoding, bytes received, and a redacted tail of them — in `model_requests.error_detail`, and `leveler trace` prints it under the failed request. A `200` that broke mid-body is no longer indistinguishable from an upstream that never answered, and a first attempt at the same finding is not thrown away with the consumed response
- `leveler background journal list` and `leveler background journal abandon <task-id>` inspect and release semantic write reservations, naming the fact that decides each one

### Changed

- Settled TUI tool processes fold across model rounds into one clickable receipt with counts by tool kind and explicit failure, stopped, and unknown outcomes. Opening the receipt restores member headers in order; each member retains its own details. Confirmed edit diffs remain fully visible
- Lone commands no longer repeat a group title; background wait receipts stay inside their drawer, and background completion updates the matching starter without a duplicate transcript notice
- Consecutive ordinary tools accumulate into one receipt per visible stage, including a lone tool. The closed header reports real kinds and counts and marks failures separately; running work states its own state and count, and folding never shows busy or cancelled work as finished
- Reasoning no longer appears in the conversation. The status strip reports the streaming fact and clears it at completion; Ctrl+O opens the latest tool group's details and never substitutes a thought
- Tool and failure details start collapsed and open through their own header, which keeps a running call's real state visible either way
- Progress prose and the Final are shown as written, without a per-paragraph prefix symbol, and model text is no longer restated by the interface
- Model-failure wording follows delivery evidence: "cannot reach the service" only when nothing was sent, "receiving the response failed" once headers arrived, "response interrupted" for a stream cut mid-flight, and "communication failed" when the transport cannot say
- Agent progress updates connect observed work to the user's goal — an opening statement, updates at findings, blockers and direction changes — instead of restating each tool call

### Fixed

- A response body that failed mid-read no longer ends the whole task when the identical transient condition one phase later was recoverable: a reasoning-only interruption with no answer text or tool-call data may use the bounded retry lifecycle. Each failed attempt is settled before the retry and abandoned reasoning is never merged; answer text or a started tool call is still never replayed
- A permission-settings edit no longer treats answer progress since the observed snapshot as a settings conflict
- A stopped execution host no longer blocks the next runtime start: liveness is decided by the host's lifecycle lock rather than the existence of its ready file, so a dead host's residue is cleared and SIGTERM removes it cleanly
- A semantic write reservation the host never admitted no longer blocks every future start; it is released when the archive holds no fact for it and no spawn intent is outstanding, and everything else keeps its reservation

## [1.0.14] - 2026-10-12

Turns the terminal into a primitives-first editor surface — input editing,
history and help search, local terminal diagnostics, and an evidence-bearing
clipboard route — and reworks transcript presentation so it stops repeating what
the conversation already shows.

### Added

- Bounded composer edit history: `Ctrl+Z` undoes, `Ctrl+Shift+Z` / `Alt+Z` redoes, and `Ctrl+Y` restores recently killed text. A mixed paste, its file references and its image attachment bindings are restored together; submitting or changing sessions ends the draft's history (100 entries and 8 MiB per stack)
- `Ctrl+R` searches submitted input history: `Enter` puts the selected entry back in the draft for editing without sending it, `Esc` / `Ctrl+C` closes the search and keeps the draft
- Searchable `/help`: typing filters commands, shortcuts and descriptions, and the selected row stays reachable at narrow widths
- `/doctor`, a local terminal diagnostics screen, and the same observations in `leveler doctor`: terminal, SSH/tmux, color hints, newline keys and clipboard route. Diagnostics neither read nor write the clipboard, and a capability without evidence stays unknown
- Terminal-aware clipboard routing with a delivery receipt. Only a native write followed by matching readback on a local, non-tmux host is verified; SSH and tmux skip the native clipboard, a tmux buffer write and an OSC 52 send are reported as sent with local delivery unverified, and OSC 52 payloads are capped at 64 KiB
- Commands keep their full identity in the conversation, expanding to the complete argument text wrapped to terminal width, and a command waiting without output says so
- The Background Tasks page selects tasks with the mouse, routes the wheel by region, and keeps each task's own scroll and follow state; identity and stop controls stay fixed while only detail content scrolls

### Changed

- The footer context bar is presentation only: percentage, denominator, bar length and color come from the runtime projection, provider usage is only the no-snapshot fallback, and a snapshot with an unknown window has no gauge. Narrow rows drop the bar, then the cache, then the capacity chips; the clock and the permission / collaboration controls retain priority
- `/context` names input capacity, model window and output reservation separately; the compaction policy and thresholds stay on the input-capacity axis while the runtime snapshot's `free_tokens` remains model-window free
- A running command's status reports the phase and the command's own elapsed instead of repeating the command name the conversation row already shows
- Thoughts use bounded previews: a live Thought shows its header and at most the latest three wrapped body rows, a completed standalone Thought occupies no history row until it is asked for, and `Ctrl+O` reveals the header of every such Thought at once while opening the latest command's details (so a 40-read receipt's file content stays reachable); a header's own click opens that one body
- Exploration receipts fold independently at each level: opening a receipt restores its member rows in real chronology, every exploration call with its own body disclosure and a header for each finished Thought that sat between them
- Real transcripts start at the viewport top; only the empty-session splash is centered. Manual folding and a runtime completion or interruption that folds a Thought keep the line at the viewport's top edge in place

### Fixed

- A composer slot shorter than the box dropped its closing border and painted the footer through an open frame; the tail of the box is kept instead
- A background detail pane could collapse or overdraw its rule row in a short pane
- Unpainted filler rows below a short transcript were click targets

## [1.0.13] - 2026-10-10

This patch focuses on CLI/TUI and Agent Core reliability and terminal performance.
Web and Desktop are outside this release's qualification scope.

### Terminal

- Code blocks remain fully accessible through transcript scrolling, including long blocks, resize and resumed history. Tool output is reachable from the keyboard, and Ctrl+O expands the visible exploration receipt rather than only its hidden members.
- Streaming syntax highlighting reuses completed-line Syntect state. An unfinished line is highlighted again from its saved state, preserving cross-line syntax and the complete code text.
- The bounded highlight cache retains 640 entries instead of 512, addressing the measured 574-block history workload. The existing source-byte budget and LRU eviction remain in place.
- Failed turns and durable cancellation are shown as terminal outcomes. Repeated cancellation waits instead of promising a force operation.
- The TUI rejects non-terminal stdin or stdout with an unsupported-terminal error before entering raw mode.

### Runtime and context

- Auto permission classification inspects literal `cmd.exe` redirection destinations using Windows quotation and caret-escape rules. Full remains unrestricted.
- Pre-claim and explicitly scoped filesystem writes no longer receive an implicit shared temporary-directory grant. Ordinary Auto temporary writes remain available, and Full stays unconfined.
- Interactive command acknowledgements follow durable application; cancellation is persisted before acknowledgement and reconciled after interruption.
- Resuming a Goal recovers interrupted lineage from fenced durable turn facts, and ordinary continuation goes through the existing runtime owner.
- Soft compaction uses a percentage of the effective input capacity, with a 95% default. Terminal context utilization follows runtime accounting on resume and after policy changes.

### Validation and limits

Full-code visibility has regression coverage across resize and history replay. Controlled Agent fixtures now verify Goal closeout and distinguish continuity from a completed task. The independent performance comparison includes matching code fixtures, terminal sizes, real PTY runs and a fresh 600-second stress test.

Incremental highlighting reduces repeated work during streaming; it does not remove whole-document Markdown parsing. A cold 1MB syntax highlight still costs approximately one second on the measured machine. Physical terminal compositor flicker is not qualified by the PTY tests.

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

- A permission change could be silently undone: a staged turn wrote its captured permission mode back into the session's live profile on every engine build, so the UI could show `full` while policy read the stale `assisted` and asked for approval on a destructive command
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
