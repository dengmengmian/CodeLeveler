# Changelog

[中文](CHANGELOG.zh-CN.md) · [Install and use](README.md)

## [Unreleased]

- Completion verification now has an explicit evidence policy per requirement kind. A `named` deliverable is model judgement: a delivered `done` account is recorded as `reported` and completes normally, with no "no mechanical check" warning. A passing recorded run of a declared `tests`/`command` satisfies it only when that run is the exact declared command and is the newest matching execution; a different or compound line is not its record. For `tests`, `verified` is the declared command contract — the line ran and exited 0 after the last change — not a claim that the command executed tests. Requirements that need evidence the runtime cannot obtain stay `unverified` and keep their warning.
- Background task display: starting a task no longer posts a second notice next to the tool row, and a failed task is a record in the conversation and the jobs page instead of a footer reminder that needs acknowledging. The running-count cell counts only tasks that are running now.

## [1.0.27] - 2026-11-14

- Cross-session memory is available as an opt-in capability. `[memory].rollout` is `off` (the default), `record_only`, `shadow` or `active`; only `active` exposes memory to the model. Everything lives under `<LevelerHome>/memory/`; a workspace is identified by its git remote, and a temporary working directory has no workspace scope.
- Capture runs as durable background work after each settled turn. The extraction call uses a forced tool call and writes structured observations as Markdown files plus a SQLite ledger row; the turn itself is never blocked and committing an outcome is idempotent. A normal quit releases an in-flight job as retryable without charging the bounded retry budget, and startup scans for claimable jobs so a cancelled capture resumes without a new turn. Paused sessions and a global `off` rollout record skipped turns instead of backfilling them on re-enable.
- Dream consolidates pending observations into Markdown topics. A durable six-hour lease serializes it across processes and an owner whose PID is gone is treated as expired. Automatic Dream runs after capture settles; `/dream` runs one pass; `D` on the memory page runs a bounded batch.
- Each turn's system context can carry a bounded `## Cross-session memory` index of topic paths and one-line descriptions. The agent reads a topic with the ordinary `read_file` tool; there is no retrieval API and no vector database.
- `/memory` opens the management page (both scopes, read, filter, refresh, pause and Dream keys), and `leveler memory status|show|remember|forget` covers the CLI. Forget binds to the BLAKE3 hash printed by `memory show`: a tombstone is written before the file is removed, the path is excluded from the index, capture and consolidation, and the retained archive is invisible to the agent's file tools.
- A `blocked` goal resolution is refused once while the model's own latest explicit plan still lists steps it never marked completed. The refusal names the open steps and structured exits; a confirmed block is accepted with the conflict kept in its receipt. The check is bounded once per drive, never rewrites a block into a completion, and never completes pending steps on the model's behalf.
- `shell_command` names input mistakes instead of ignoring them: `program`, `args` or `command` used beside or instead of `cmd` and a wrong-typed `cmd` are refused by the schema, and a refused call never executes. `reason` and the documented `workdir` alias keep working.
- Plan progress has a dedicated TUI status row and updates in place.
- Sub-agent run futures are boxed, so nesting no longer adds a full stack frame per level.
- A completion claim the runtime refused for a named reason is recorded with that detail and is not booked as a runtime gate failure in evaluation.
- Dependencies: `quinn-udp` 0.5.16 closes RUSTSEC-2026-0538. Added GitHub issue forms.

## [1.0.26] - 2026-11-12

- Continuation, pause, amendment, cancellation and questions are decided by the model in the running turn, not by a wording pattern. The model declares the meaning with `declare_intent`, quoting the newest user message verbatim; the runtime checks that the quote really came from that message, which goal it targets (and refuses to guess when several are running and none is named), the legal ordered operations, and the plan step a boundary names by stable id. Any language, phrasing or compound instruction ("继续执行，但第三阶段完成后暂停") uses the same path, and no extra classification call is made.
- A pause is released only by a validated `continue_task`. Clients submit the user's text unchanged and project the runtime's goal state; "继续" no longer turns into a special client-side resume intent. A supplement or a status question neither releases a pause nor starts work, and a declaration cannot create authority the user did not state.
- Accepted declarations are stored in order with the turn that carried the quoted user message (migrations 0053, 0054), so a restart can still answer who authorized what, with which words. A turn that continues the same goal reuses its plan instead of re-planning the completed stages from nothing.
- A clarification is durable. Each request has an id, structured questions and an answered/skipped/cancelled terminal in the event log; the live waiter is only a delivery channel. A client disconnect, a daemon restart or a task cancellation no longer discards the question or lets a late answer start cancelled work, the first terminal wins, duplicate or stale answers are refused, and the TUI restores the pending card from durable state.
- A checkpoint carries the last window's answered clarifications, amendments, verbatim user messages, runtime-observed command outcomes and accepted outcome. The plan is projected from the newest declaration, so a yielded window does not repeat work its predecessor finished, and a yielded turn is shown as a neutral boundary rather than a failure.
- `docs/INTENT_CONTRACT.md` records the ownership boundaries for the declaration, validation, persistence and client projection.
- Goal scope is a structured, model-declared requirement set. `commit_task.requirements` seeds it for a Chat-committed task and `declare_scope` adds, amends, replaces, waives or restores requirements on an already bound goal; every operation is anchored to a verbatim quote from the newest user message. The runtime validates the quote's existence, requirement identity, legal transitions and event order, and never re-reads the objective's prose. The keyword, negation-word, section-heading, quantifier and test-name heuristics are gone, so "特别优化启动速度", "更新 specification 文档", "能不能加一个 status 字段？" and `## Abnahmekriterien` are ordinary user text that only the model interprets.
- The completion gate holds a model to its recorded set: every active deliverable is accounted for by stable id, artifacts and absent artifacts are checked against the workspace, content literals against the target file (`content` requirements put the exact literal line in `text`), commands and test runs against the recorded execution history after the last change. A resumed or compacted context cannot shrink the set, and a requirement reported done without a mechanical check is surfaced as an explicit unverified boundary rather than silently accepted as proof.
- Requirement changes are durable ordered events (migration 0055) carrying the authorizing turn and the verbatim quote; the current set is a fold of that history. `get_goal_scope` projects the set and its change history, and Goal mode states the declaration obligation in its contract.
- `docs/GOAL_SCOPE.md` records the requirement ownership, declaration, validation, history and completion-gate boundaries.
- Declaration and evidence are separate. `update_goal(verification)` stays the model's declaration; the gate resolves its citation against the runtime's own execution record by call id, exact command line, or command plus a trailing note. A failed run, a run that predates the last change, or one a later run of the same command contradicts is refused for that exact reason. No command is classified by name or output, so an unfamiliar runner counts exactly like a familiar one when the record proves it passed.
- An accepted completion persists a structured `VerificationReport` on the task terminal: an aggregate Verified/Unverified verdict plus one record per owed requirement carrying its id, kind, state, the execution's call id, command, exit code, workspace version and model step. Verified, CompletedUnverified, Incomplete and Blocked are projections of the task outcome, this report and the stop reason; an unverified completion is delivered with a warning instead of being held for user confirmation.
- A terminal recorded before the structured report still replays: the removed host-owned verdict string in the same field reads as "no report", while a malformed report still fails replay instead of silently becoming unverified.
- The remaining runner-name and output-marker vocabulary (`is_verification_command`, `names_a_test_suite`, `reports_no_tests`) is gone: the runtime no longer reads a command's name or sniffs a suite's output to decide what its result means.
- A completion is never `Verified` without a requirement-level record. With no owed requirement a cited command can still account for the completion, but the terminal is unverified: a passing command proves an execution, not a delivered requirement. The earlier empty `Verified` report claimed coverage the runtime never established.
- `tests` requirements declare their exact command in `target`. The runtime matches recorded runs of that command — custom commands work, no runner vocabulary is involved — and refuses a failed, stale or never-run declaration. Because no structured test result exists, a passing command is reported as unverified test coverage with its call id, command and exit code, so `echo`/`true` can never satisfy a tests requirement as verified coverage. Legacy `tests` requirements recorded without a target stay finishable at the explicit unverified boundary.
- A pause boundary is reached only by an explicit `completed` status on its step. A step that was deleted, renamed without its stable id, replaced, split or merged is still outstanding; the model may adjust the plan freely and re-declare the boundary when the stage really moved.
- A goal-lifetime background task whose runtime generation was killed is no longer adopted by its replacement. The Execution Host records the admitting boot at spawn and accepts an idempotent owner-loss fact (protocol minor 2, optional `process.owner-loss`); the replacement records it only after the boot lease proves that generation dead. The task is projected as Lost while the host keeps the process and its output, its elapsed time stops at the proven loss, and `leveler background list` names the runtime as its owner. The host never signals the process, repeated restarts keep one record and one timestamp, and graceful shutdown kills only the current generation's goal work.
- A detached command whose owning runtime is gone now settles its TUI row as state unknown instead of keeping a running clock; the same applies to the runtime's unknown background-task terminal. The Desktop jobs list settles a vanished row from its durable record (Lost, Unknown, Conflict, Completed, Failed or Cancelled) and never keeps it as running without proof. A newly opened Desktop window restores durable Lost/Unknown rows, and its labels keep an unprovable outcome distinct from a failure.
- A failed Execution Host startup reports the host's own stderr tail, so the readiness error names the cause instead of only an exit status.
- Transient provider failures no longer end the whole request. `[retry]` is a route fact (`max_retries`, `recovery_seconds`) defaulting to 12 retries inside a 600 s window; backoff climbs to a 60 s steady rate, every wait is checked against the remaining window, and `Retry-After` wins. A retry after a tool result does not repeat the side effect, and the TUI says "model service unavailable · retry" instead of reporting a lost connection.
- Project rules are read once per turn and delivered with a typed audit. Unreadable, invalid-UTF-8, non-file and symlink-escaping sources are reported instead of silently dropped; empty, absent and loaded sources stay distinguishable; a mutating call pauses when an applicable scoped rule cannot be read; a directory's own `AGENTS.md` is scoped to it.
- Tabs and control bytes are normalized at the composer and Markdown boundaries before width measurement, so pasted input, history and model Markdown cannot ghost later cells. The splash deletes its Kitty image data by id before leaving the alternate screen.
- The standalone Flutter mobile client, its relay, and the host-side remote protocol/agent crates that only served it are removed. The CLI and TUI no longer have paired-device commands or screens; the local transport, runtime host and client command/event/snapshot protocol remain the native session interface for other clients.
- Dependencies: `rmcp` 3.9.0 closes RUSTSEC-2026-0462; `cap-primitives` 4.0.3 closes the sandbox escape in RUSTSEC-2026-0482; `rustix` 1.1.6 closes RUSTSEC-2026-0486. RUSTSEC-2026-0459 (bundled SQLite in `libsqlite3-sys`) has no fixed `sqlx` 0.8 release, so it is recorded with an expiry and a local-file reachability boundary instead of being silently ignored.

## [1.0.25] - 2026-11-09

- Input integrity has one boundary. Secrets are classified by value instead of by field name, so PEM blocks, `DB_PASS`, JSON/YAML credentials and `Authorization` values are redacted while code identifiers, prose and JSON keys stay byte-identical; a placeholder can no longer be written back to disk through `write_file`, an added file or a new patch line.
- A turn's durable facts and its transcript can no longer disagree. A durable delivery precedes every turn terminal, a crash between an executed tool and its transcript recovers as an explicit unknown instead of a silent replay, and repeated terminal submissions are idempotent by turn identity.
- `※ 回顾` is durable: one review is stored per settled turn (migration 0052), and replay or reconnect shows the same text without another model call.
- Background work is supervised. Every background task has a durable identity and result; a runtime crash keeps the original task id and its process start identity, and a recovered task is observed rather than spawned again.
- MCP outcomes distinguish success, failure, timeout, cancellation and transport loss. An unknown outcome is reported as unknown and is never retried blindly.
- Steering and compaction keep order and identity: a mid-turn instruction survives compaction and resume, a status question adds no goal scope, and `run --resume` sends its instruction through the production amendment path.
- Long-running daemons reclaim request and session state: a completed connection task is joined instead of retained (about 7 KiB per request), and idle session event streams are bounded. A resident runtime serving scheduled automation no longer grows memory without bound.
- The release page records the 24-hour soak status and any conditional-release reason, so an unfinished soak is never shown as passed.

## [1.0.24] - 2026-11-07

- Durable scheduled automation. `leveler schedule add` stores a definition in `~/.leveler/state/schedules.db` with one workspace, one action, one trigger and its policies. Actions are a shell command, a typed native action, or an agent goal. Triggers are a one-off RFC 3339 instant, a fixed interval, a daily time, selected weekdays, or a five-field cron expression in an IANA timezone. Session policy is `continue` or `fresh`; overlap is `skip-if-running`, `queue-one` or `allow-concurrent` (`allow-concurrent` requires `fresh`); a missed slot is `skip` or `run-once` within a grace window. Runs are claimed under a renewable lease, and an expired lease settles `unknown` instead of retrying. `list`, `show`, `update`, `grant`, `revoke-grant`, `runs`, `pause`, `resume`, `run-now`, `cancel` and `remove` manage the definition; `dispatch` runs one pass and `serve` is the resident dispatcher. The TUI `/schedules` page, the Web UI and the desktop Automation page use the same runtime facts, and the model-facing `schedule_inspect` / `schedule_manage` tools manage the same definitions without creating a second authority.
- Unattended grants. A native or agent action (and a shell run) needs a persisted grant covering its action, workspace, trigger, permission mode and external-side-effect choice. Editing a covered field invalidates the grant; Full permission does not substitute for it; a missing grant refuses the run before it starts.
- Operating-system wakeups. `leveler schedule enable` registers a launchd agent on macOS or a per-user Task Scheduler task on Windows; `disable` and `uninstall` remove it; `status`, `wakeups` and `doctor` report the registration, wake history and checks. Linux reports the wakeup backend as unsupported; schedules remain runnable through `serve` or `dispatch`.
- Deterministic native actions. `leveler native capabilities` probes Mail on macOS or Outlook on Windows without launching the application or requesting consent. `leveler native action` runs `mail.draft.create`, `mail.send.open_ui`, `mail.send.submit` and `mail.sent.check` with no model, daemon or session; input is validated before planning, user text travels through private `0600` files, and success requires a recognized result marker. A timeout or unrecognized outcome is `unknown` and is never retried. The `native_capabilities`, `native_action` and `native_verify` tools expose the same contract to the model.
- Computer use. `computer_observe` and `computer_act` implement desktop observation and control over macOS Accessibility/CoreGraphics and Windows UI Automation/Win32. Observation references are per session and revalidated against the live UI before each action. The tools reuse the existing approval policy, capability packs and image channel; there is no separate execution path. macOS reports Accessibility and Screen Recording permission at call time.
- Foreign session continuation. `/resume-claude` and `/resume-codex` discover local Claude Code and Codex sessions, enforce a workspace match, and import one into a native session seeded with a bounded briefing. The source store stays read-only and authoritative; tool calls are not replayed, a completion claim from the source remains unverified, and `foreign_session_history` reads the original transcript on demand.
- Durable input and delivery. Session input journal migrations 0049–0051 give each submission an identity before admission, so a reconnect or recovery delivers it once instead of losing or duplicating it. A final answer is bound to the model's explicit `deliver_answer` declaration and persisted as a durable event before the turn terminal; a crash after the answer is recovered without another model call.
- Unified TUI syntax highlighting and diff rendering: one highlighter for code blocks and diffs, and an applied diff remains one copyable document. `diff_visual_acceptance.py` renders the diff corpus for visual acceptance.
- A client that loses the runtime startup race adopts the live runtime instead of failing permanently.
- Natural recurring schedules keep the rule's own grid while a shell run is active; `allow-concurrent` shell runs use one execution context per run; a one-off keeps its pending occurrence when its trigger is rescheduled; `schedule show` only projects a pending occurrence.
- The headless CLI prints the committed delivery rather than text from a bookkeeping round, and continuation observers replay the accepted answer without persisting a second delivery fact.
- Regenerated the protocol schemas and Web UI types, silenced vendored crossterm lints, fixed platform-gated clippy errors, and kept reconnect test fixtures isolated on Unix.

## [1.0.23] - 2026-11-04

- `→` accepts the inline next-step suggestion, exactly like Tab: the ghost becomes editable input with the caret at its end and nothing is sent until Enter. With no suggestion showing — an empty popup, Shell Mode, a completion menu, a non-empty draft — it stays an ordinary caret move.
- Reconnect state is evidence-based. The runtime reports the build serving the session and whether its process changed across a reconnect; a version mismatch stays visible as an issue, a turn that was running is described as possibly interrupted or unknown rather than stopped without proof, and input or approval choices are held when the connection is down.
- A turn whose tool calls were interrupted with no recorded result now pauses instead of retrying. `/recover` lists each call with its state and, after confirmation, records it as "result unknown, not replayed" — nothing is replayed and no call is reported as successful.
- A submitted message reports whether it was delivered, rejected with its content restored, or unconfirmed; a message whose owning process died is never retried automatically.
- Background task status distinguishes a task lost with its background process, an unconfirmable state and an identifier conflict from a task that is running or finished.
- Goal accounting recognizes explicit single-letter field labels while still rejecting incidental prose and evidence-only mentions.

## [1.0.22] - 2026-11-04

- Streamable HTTP MCP. `[[mcp_servers]]` accepts `url` (and optional `transport = "streamable_http"`, `headers`, `bearer_token_env`) in addition to the existing stdio `command` / `args` / `env` form. Header and bearer values are environment-variable names. The client negotiates MCP `2026-07-28` and falls back through the Streamable HTTP versions still used by older servers. A tool call whose result is unknown after a timeout or a dropped session is not sent again. OAuth 2.1 is not implemented. Existing stdio servers keep working without a config change.

- An empty conversation opens on a welcome card: the brand mark, the tagline, a rotating tip that names `/goal`, `/diff`, `/context` and `/theme`, and clickable entries for session history, help and model selection. It disappears as soon as the conversation has real content, folds to a narrow layout on small terminals, and paints the original mascot where the terminal supports a native image protocol.

- Retired Memory 2.0: removed background extraction, storage/retrieval, context injection, tools, commands and management screens. Session history, resume, compaction, goals, plans and project rules remain available. Existing databases remove retired queues and policy columns on upgrade.

- A user-stated pause is now an execution boundary, not an unresolved plan. "Finish stage 1, then wait for my confirmation" ends the turn normally with stage 2 still `pending`; the harness does not buy extra rounds to pressure past it. The boundary is read from the user's own words and the model's plan, requires the named stage to be completed, survives resume/compaction, and is released by a later user instruction. A model's own claim that it needs confirmation is still not a pause: without an explicit user boundary, the open plan keeps its normal continuation.
- Plan progress counts only the current plan. A step the model replaced is recorded as `abandoned` history (shown separately, e.g. `计划 1/2 · 1 项历史步骤已弃用`) instead of inflating the denominator; it is never counted as completed. An explicit user pause is shown as `等待用户确认` rather than an unexplained open plan. That label now comes from the goal the runtime recorded the pause on, so an earlier goal's pause never marks a different goal as waiting, and a paused Chat-committed goal keeps its own badge while an ordinary follow-up question is answered.
- A request keeps its authority in question form. "能不能顺便把 X 也补上？" and "Can you also add X?" now add `X` to the goal's scope instead of being discarded with every other question; status queries ("测试通过了吗？"), consultations ("如果加 X 会有什么影响？") and authorization changes ("能不能先不要执行阶段 2？") still change no requirement. Asking a question never releases a pause the user set, and changing scope never releases it either.
- `commit_task` and `spawn_agent` are no longer described with the same word. Committing a goal is not delegation: the row says `Goal committed`, `Goal not committed` (a refused acceptance list, with the reason one fold away) or `Goal commit failed` (a real tool failure), while only `spawn_agent` says `Delegation`. Both the conversation and the Tools screen read one status mark, so a refused declaration is a warning everywhere instead of a red failure in one view.
- A chat turn that ends while the plan it published is still open no longer just stops. The runtime states the open steps and the three honest ways out — continue the work, rewrite the plan because a step is no longer needed, or say what blocks it — bounded by the same no-progress guard as a Goal continuation, and never as a success claim. A completed plan, a host-seeded plan, an ordinary question and a denied permission all buy no extra round; a Goal turn keeps its own continuation. The plan stays the model's declaration: the runtime does not mark steps done, does not require 100%, and does not treat a full plan as proof.
- Removed the `/develop` command and its fixed Analyze → Coding → Review workflow. `/goal` remains the explicit-goal entry point; model planning, review, delegation and goal verification are unchanged.
- Configuration is live state: editing `~/.leveler/config.toml` (or the repository bundle) applies to the next turn without restarting, and `/reload` re-reads it immediately. A running turn keeps the model/provider/generation it started with; a session created without naming a model follows the new default, while an explicit `/model` choice stays pinned. Invalid or partially written files keep the last working configuration; a failed reload names why. Settings that only apply at start-up (UI language, browser product) are reported as requiring a restart instead of being reported as applied. MCP servers are reconciled per server, so unchanged connections survive a reload.
- `/context` and the new `/usage` are now one "Context & Usage" page: `/context` positions it at the context section, `/usage` at the usage section. Usage comes from the runtime's durable model-call ledger and covers this turn, this session, today, the last 7 days, the last 30 days and all time, with per-model token and metered-cost detail.
- Usage keeps "unmeasured" apart from "zero": a provider that reported no usage, a model with no configured price, and a partially priced scope are shown as unknown or as a known part — never as `$0.00`.
- Fixed read-only diagnostics during a running turn: `/trace`, `/context` and `/usage` open immediately while the agent works, read the same runtime state, and coalesce event-driven refreshes into a single query so answers are no longer discarded.
- The context page no longer prints a separate `Free` detail row; remaining capacity stays in the occupancy map and the header ratio.
- Fixed a buffer overrun that crashed the `/` command menu on narrow terminals (40 columns and below).
- A background task that reaches its terminal state while the user steers still delivers both the exit and the message. Undelivered task facts drain on the next admission, a failed provider attempt retries the request that already holds the fact, compaction keeps the delivered task identities, and concurrent terminal writes stay gapless.
- Fixed a background task started with the default `goal` lifetime inside a plain chat session outliving its turn. The turn's terminal is the boundary that lifetime promises, so the task is stopped there; a goal that still owes work keeps its services.
- A runtime of a different build is never attached while it is busy: a client that finds the endpoint busy defers its upgrade instead of silently joining the previous generation. An active background task now states its own lifetime, so quitting the TUI stops goal-scoped work while a shell the user sent to the background keeps running.

## [1.0.21] - 2026-10-29

- Improved reasoning history when switching models, keeping private reasoning separate between models. Historical reasoning is sent back only to the route that produced it, and only when that route declares the mode.
- Fixed follow-up questions being treated as new goals after task completion, and improved resuming paused work. Ending a goal returns the session to ordinary chat.
- Goal completion follows the request and its evidence. A path that was only permitted is not a required deliverable. A refused check can be recovered without starting the goal over.
- Added a `/memory` management page to search, read, and manage records; `/memory status` shows processing activity.
- User facts and experimental experience records have separate settings; experience recording is off by default. Recalled facts keep their source and authority.
- Improved retrieval for large memory collections and protected historical sensitive records in UI and logs.
- Improved project rule loading, session recovery, and completion checks to reduce missed requirements and repeated work.
- A local terminal keeps its output, its owner, and its file descriptors.

## [1.0.20] - 2026-10-19

- Added Shell Mode: enter `!` for command mode and use Tab to complete files, directories, and commands.
- Read complete local command output with `/output` and copy it with `/copy output`.
- `Ctrl+B` moves a running local command to the background; `Ctrl+C` stops the current execution.
- Improved user message backgrounds, long output folding, and plain-text copying.

## [1.0.19] - 2026-10-19

- Web search can use DeepSeek or Tavily independently of the conversation model.
- Search requires `[web.search]`; an API key alone no longer enables it.
- Homebrew installations now use `brew upgrade leveler` for updates.
- Simplified the terminal footer with direct access to background tasks, failures, and plans.

## [1.0.15] - 2026-10-13

- Tool activity is grouped and collapsed by default, with individual details and full file diffs available.
- Improved diagnostics and recovery for model connection failures and interrupted responses.
- Fixed sessions failing to start after a background execution was stopped.

## [1.0.14] - 2026-10-12

- Added input undo, redo, history search, and searchable `/help`.
- Added terminal and clipboard diagnostics, distinguishing sent copies from verified copies.
- Improved background task mouse controls, scrolling, and narrow layouts.

## [1.0.13] - 2026-10-10

- Improved long code blocks, streaming highlighting, and keyboard access to tool output.
- Improved cancellation and session recovery, and fixed Windows permission checks.
- Starting the TUI without an interactive terminal now reports an explicit error.

## [1.0.12] - 2026-10-07

- Aligned execution status across interfaces and fixed failures or missing answers being shown as complete.
- Added `/btw` for read-only side questions without interrupting the main conversation.
- Improved ordinary development in Auto mode and fixed permission state after switching modes or resuming.
- Fixed execution history and command output after reconnecting, plus narrow terminal display issues.

## [1.0.11] - 2026-10-02

- Added shared reasoning intensity settings through `/thinking`, defaulting to `high`.
- Added a desktop client runnable from source; release packages still contain the CLI only.
- Improved context usage reporting, long-session compaction, and background task recovery.
- Fixed tasks unable to continue because some model configurations produced zero context capacity.

## [1.0.0] - 2026-09-18

- First stable release with startup auto-update, `leveler update`, and terminal `/update`.
- Release archives include SHA-256 checksums; fixed tool-call pairing errors in long sessions.

## [0.1.0-beta.1] - 2026-09-17

- Public beta with terminal, browser, CLI, session recovery, and custom agents.
- Supports macOS, Linux, and Windows, with experimental mobile and remote connection sources.
- Packages lack Apple or Microsoft signing; Windows does not support a local background service or per-command network isolation.
