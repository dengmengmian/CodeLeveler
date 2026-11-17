# Everyday use

[中文](USAGE.zh-CN.md) · [Documentation index](README.md)

[Model setup](#configure-a-model) · [Sessions](#sessions-and-background-work) · [Command line](#command-line-runs) · [Review](#review) · [Troubleshooting](#troubleshooting)

## Configure a model

Run `leveler login` to choose a provider and model. Settings live in `~/.leveler/config.toml`. For another OpenAI-compatible service, run `leveler init` and edit that file.

Configuration is read again while the app runs: editing `~/.leveler/config.toml` (or the repository bundle) applies to the next turn, and `/reload` re-reads it immediately without restarting. A running turn keeps the model and provider it started with; a session that never chose a model follows the new default, while an explicit session model choice stays put (`/model default` is the separate action that changes the default). An invalid or partially written file is rejected and the last working configuration stays in force. Settings that can only apply on start-up, such as the UI language, are reported as requiring a restart instead of being reported as applied.

Use `/thinking high` to change reasoning intensity for the session, or `/thinking reset` to return to the configured default. Choices are `auto`, `off`, `minimal`, `low`, `medium`, `high`, and `max`; run `leveler doctor` to check model support. The default is `high`; `max` uses the model's strongest supported level, and `auto` leaves the choice to the provider.

`--deny-network` (`--sandbox` is the legacy alias) is a run-level restriction: it denies the network to `run_command` processes and to the network tools, and Full permission does not undo it. Desktop automation through `computer_act` drives the user's applications outside the process sandbox and is **not** bounded by this flag; approve or avoid those actions when a run must stay offline. A backend that cannot enforce the denial refuses the command instead of running it with the network open.

A transient provider failure (connection refused or reset, a retryable timeout, 429, a retryable 5xx) is retried automatically with exponential backoff and jitter, without resending the original task. The default budget is 12 retries within 10 minutes; `Ctrl+C` cancels the wait. Change it in `~/.leveler/config.toml`:

```toml
[retry]
max_retries = 12        # retries after the first attempt; 0 disables automatic recovery
recovery_seconds = 600  # ceiling for retries (waits and attempts), from the first attempt
```

A permanent error (authentication, an invalid request, a rejected conversation) is never retried. When the budget runs out the failure is reported as resumable: the task and its completed tool work are kept, and sending a new message continues from there.

### Custom OpenAI-compatible discovery

For a custom gateway such as A2Agent, keep using the existing provider settings in
`~/.leveler/config.toml` (this is a provider entry, not a complete model configuration):

```toml
[providers.a2agent]
base_url = "https://api.a2agent.me/v1"
api_key_env = "A2AGENT_API_KEY"
```

Set `A2AGENT_API_KEY` locally to your own key; never put a real key in shared
configuration or examples. In your manually configured model entry, use
`provider = "a2agent"` and an explicit model ID and limits verified for that
endpoint. A model appearing in a catalog does not establish its capabilities.

Model discovery queries `/models` without running inference. A valid empty
`data` array or a missing `data` field keeps the existing empty-list behavior;
an explicitly non-array `data` value is reported as a malformed listing.
Discovery failure does not remove manually configured models. This example is
configuration guidance, not a claim of live compatibility verification.

## Input and results

- `Ctrl+R` searches input history. Enter restores an entry to the draft without sending it.
- `Ctrl+Z` undoes input; `Ctrl+Shift+Z` or `Alt+Z` redoes it.
- Click a tool group for details, use `Ctrl+O` for the latest execution, or `Ctrl+G` for the final answer.
- Type `!` in an empty input to enter Shell Mode; Tab completes commands and paths.
- `/output` opens full local command output; `/copy output` copies it.
- An edit paints its complete applied diff by default; click its head row or press `Ctrl+O` to fold it to one `+A/-R` row. Dragging over the diff copies the selected source lines (no gutter, no `+`/`-` marker); `/copy diff` copies the complete applied patch instead.
- `Ctrl+B` moves a running local command to the background; `Ctrl+C` stops that execution.
- Windows Terminal's native `Ctrl+V` submits at newlines and replaces non-BMP emoji with `?` in native console programs. On Windows, `Alt+V` reads the system clipboard directly and inserts it as one paste, preserving newlines and emoji; `Ctrl+V` remains available for images or single-line text.

Open background tasks from the footer to inspect output or stop an individual task. Esc only returns to the list. Closing the terminal does not stop background tasks when connected to the background service.

Without `timeout_seconds`, a foreground command still running after 15 seconds moves to the background and returns a `task_id`. With `timeout_seconds`, the call waits for the command: a deadline up to 120 seconds ends it in that call, and a longer deadline is waited for up to 120 seconds before the still-running process moves to the background (the deadline continues to follow it). A move is not a successful exit: observe it with `get_task` / `wait_task` and stop it with `kill_task`. `timeout_seconds` is a process execution deadline counted from process start. Start a long-lived service with `background=true`; its lifetime follows `background_lifetime` (`goal` by default, or `runtime` / `session` / `persistent`), not the wait budget.

Tool summaries show elapsed time from dispatch to completion where reliable timing is available. Reconnecting restores the recorded timing. Approval dialogs start on **Allow once**. Press Esc to deny.

## Sessions and background work

Use `leveler resume` to continue a session. On macOS and Linux, `leveler serve` also keeps work running in the background. A local background service is not supported on Windows.

If the process that owns background services is lost (crash or kill), `leveler tui` keeps starting: affected tasks become `unknown`, and the startup notice says whether their process was still present. CodeLeveler never signals or re-runs them in that state; use `leveler background list` to inspect what was left, `leveler background logs` for the retained output, and `leveler background journal list` for write reservations that remain unresolved.

TUIs in the same project share a runtime. During an upgrade, a client with a compatible protocol and matching configuration can reconnect to a busy runtime. A later startup retries the upgrade once the runtime is idle.

A normal TUI exit cancels the active turn and stops background tasks in that session, waits for cleanup, and leaves other sessions running. Tasks in the shared runtime continue when a connection drops or the client is forcibly stopped.

## Command-line runs

`leveler run "task"` runs a task to its end. Add `--collaboration chat` for a single response. Run `leveler run --help` for options.

```sh
leveler run "add an empty-input regression test for the parser" --parallel 2 --output jsonl
```

Parallel runs need a clean, committed Git working tree. They create candidate branches and worktrees, commit verified candidates, and integrate successful candidates into your current branch. Candidates inherit the parent run's permission profile, network restrictions, model-step limit, and state directory.

Parallel runs reject `--commit`, `--push`, `--pr`, `--branch`, and `--pr-base` before starting. For single-task runs, shipping failures return a nonzero exit code in both output formats. A candidate setup, execution, or commit failure ends the run before integration and preserves the original cause. If worktree cleanup fails, warnings and the persisted terminal record include the remaining paths. Any completed integration stays in place.

## Scheduled automation

`leveler schedule` keeps definitions in `~/.leveler/state/schedules.db`. Definitions are user-global and each runs in the workspace it names (the current repository by default). Create a daily shell run:

```sh
leveler schedule add --name nightly --command "cargo test" --daily 02:00 --timezone Asia/Shanghai --grant-unattended
```

Choose one action (`--command`, `--native-action ID` with `--native-input JSON`, or `--agent-goal TEXT`) and one trigger (`--at RFC3339`, `--every SECONDS`, `--daily HH:MM`, `--weekly mon,wed --local-time HH:MM`, or `--cron "..."` with `--timezone IANA`). `--session-policy continue|fresh`, `--overlap skip-if-running|queue-one|allow-concurrent` (`allow-concurrent` requires `fresh`), `--misfire skip|run-once` with `--misfire-grace SECONDS`, and `--timeout SECONDS` set the rest of the policy.

A run that happens with nobody present needs `--grant-unattended`; add `--allow-external-side-effects` when the action sends or changes something outside the workspace. Changing the action, workspace, trigger, permission mode or that choice invalidates the grant: use `leveler schedule grant <id> [--allow-external-side-effects]` for the new identity. Full permission does not replace a grant.

`leveler schedule list|show|update|runs|pause|resume|run-now|cancel|remove` inspect and manage definitions. `leveler schedule dispatch` runs one pass; `leveler schedule serve` keeps dispatching in the foreground. On macOS `leveler schedule enable` registers a launchd agent, and on Windows a per-user Task Scheduler task; `disable` and `uninstall` remove it, `status` and `wakeups` report it, and `doctor` lists checks. Linux has no wakeup backend and reports that; use `serve` or a system scheduler that calls `dispatch`.

In the TUI, `/schedules` opens the definition list, detail and edit form. The agent has the same operations through the `schedule_inspect` and `schedule_manage` tools.

## Task workbench and diagnostics

`/tasks` opens one read-only workbench over the current session's goal, child agents, background tasks and reviews, the schedule definitions and their last runs, and every durable session in the global store. `Tab` switches Active / Recent / All; Enter opens the existing detail page (Activity, Goal, or the Schedules page); `x` cancels a current-session task through the runtime; `r` re-reads the two snapshot sources. Session rows carry a snapshot time; child and background rows update live. Cross-workspace sessions are read-only here because this runtime cannot open another workspace's store.

`/doctor` opens structured diagnostics: runtime connection, build, session validity and crash window; configuration; provider credentials; model reasoning; MCP configuration; permission profile; sandbox capability; and terminal capability. Each row has a stable id, a typed status (`pass`, `warn`, `fail`, `unknown`, `not checked`, `not applicable`), observed evidence and an optional existing action. `UNKNOWN` and `NOT CHECKED` are never shown as a pass. `R` re-runs the shared checks; the page is safe while a turn runs and never calls a provider. Live MCP connection state is not exposed yet, so that row says so. `leveler doctor --json` prints the same findings as a machine-readable report.

## Native actions and computer use

`leveler native capabilities` reports whether Mail on macOS or Outlook on Windows is installed, running, signed in and allowed to be automated. `leveler native action` runs one deterministic action without a model:

```sh
leveler native action --action mail.draft.create --to you@example.com --subject "Weekly report" --body-file report.txt
```

The actions are `mail.draft.create`, `mail.send.open_ui`, `mail.send.submit` and `mail.sent.check`. A timeout or an unrecognized result is reported as `unknown` and is not retried. macOS requires Automation permission for the target application; an unresolved consent prompt is reported as `unknown` rather than assumed.

The model-facing tools are `native_capabilities`, `native_action`, `native_verify`, `computer_observe` and `computer_act`. The computer-use tools need Accessibility and Screen Recording permission on macOS (UI Automation on Windows), report availability at call time, and revalidate every observation reference against the live UI before acting. They appear only when the platform backend is compiled in; Linux has neither.

## Continuing a Claude Code or Codex session

In the TUI, `/resume-claude` and `/resume-codex` list local sessions for the current workspace; pass a session id to skip the picker. CodeLeveler reads the source store without modifying it, imports the chosen session as a new native session with a bounded briefing, and continues from there. The briefing quotes the task and the state the source ended in instead of copying the transcript. A workspace mismatch is reported before the import, and a claim the other agent made (including "done") stays unverified until it is checked against the current workspace. The model can read the original transcript with the `foreign_session_history` tool.

## Browser and search

Browser automation supports Chrome, Edge, and Chromium. For Safari, enable Remote Automation and set `default = "safari"` under `[browser]` in your config. Safari does not provide console or network inspection.

Web search is configured separately from the conversation model. Add this to `~/.leveler/config.toml`:

```toml
[web.search]
provider = "deepseek" # or tavily
api_key_env = "LEVELER_SEARCH_API_KEY"
max_results = 5
timeout_ms = 15000
```

Set the named environment variable before starting CodeLeveler. A key alone does not enable search without `[web.search]`. UI links use your system browser.

## MCP servers

CodeLeveler can call tools from MCP servers. A stdio server is a local program. A Streamable HTTP server is an `http://` or `https://` endpoint. Both show up as `mcp__<server>__<tool>`. One server failing to connect does not remove the others.

Stdio, unchanged from earlier versions:

```toml
[[mcp_servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[mcp_servers.env]
GITHUB_TOKEN = "GITHUB_TOKEN"
```

`env` values are environment-variable names, not the secrets themselves.

Streamable HTTP:

```toml
[[mcp_servers]]
name = "devorder"
transport = "streamable_http"
url = "http://127.0.0.1:8081/mcp"

[mcp_servers.headers]
X-API-Key = "DEVORDER_MCP_API_KEY"
```

Or `leveler mcp add devorder --url http://127.0.0.1:8081/mcp --header X-API-Key=DEVORDER_MCP_API_KEY`. A bearer token is `--bearer-env SOME_TOKEN` or `bearer_token_env = "SOME_TOKEN"`; CodeLeveler sends `Authorization: Bearer <value>`. Header values are environment-variable names. Do not put a token in the URL. HTTPS verifies the server certificate. OAuth 2.1 is not implemented: a server that only speaks OAuth cannot be used by pasting a bearer token and calling that OAuth.

`leveler mcp list` prints the transport, the URL, and header names. `leveler doctor` warns when a referenced variable is unset. The Tools view shows a call after it runs; connection state is in the log (`MCP server connected`, including the negotiated protocol version and tool count).

## Review

In the terminal UI, enter `/review` to start a manual, read-only review. Each review has its own ID, budget, cancellation, and saved report. This feature was added after v1.0.27. See the [changelog](../CHANGELOG.md) for availability.

### Choose a scope

Enter `/review` on its own to open the scope picker. The default scope is the current task's accepted delivery and the files it changed. To name a scope directly, use one of these forms and add a focus in your own words.

```text
/review Check error handling in the current task
/review @workspace Check uncommitted changes
/review @project Review the project
/review @session <session-id> Examine this run
/review @commit <rev> Review this commit
/review @branch <base> Review this branch against its base
/review @path src/a.rs,src/b.rs Check the boundaries of these files
```

Append `@model provider/model` before the focus to select a model for this review. Enter `/review cancel [review-id]` to stop a running review.

### Review evidence

The reviewer reads the objective, the user's own adjustments, the requested object and, where possible, its diff. For a session or an execution question it can also page through the reviewed session's durable record with the read-only `session_history` tool: turns and their terminal states, events and tool results, user/assistant messages, every model request's tokens and latency, goals and requirement changes, and checkpoints. The tool is bound to the reviewed session — the model cannot name another one — and ordinary agents never see it.

The reviewer's read-only investigation surface also includes the code-intelligence tools (`find_symbol`, `read_symbol`, `find_references`, `diagnostics`) without a `capability` enable round trip, and `git_history` for committed history (`log`, `show`, `file`, `search`, `list`, `blame`) over the Git object database. Code-intelligence calls on a review seat use the in-process scan fallbacks, not a language-server process. A commit or branch review is tied to the resolved commit SHA: tools that read the live worktree are removed from the surface and refused at admission, so current files cannot be presented as that version's evidence. `git_history` is scoped to the reviewed object and its ancestors; a later or sibling branch cannot be used as evidence about that object.

The review's initial context is built only for that review: the source task's objective and recorded requirement changes (with the user's own quotes), project documentation entry points, and — for diff-shaped objects — a bounded relation prefetch of changed symbols and cross-file call sites. When cross-session memory is enabled, it also receives a bounded recall of relevant topics and a read-only `memory_search` tool; memory is historical context, not current fact, and is verified against the source and the user's request. None of this runs in an ordinary turn.

### Investigations and budget

A Lead Reviewer may launch independent read-only investigations with `spawn_agent` when a review is complex enough to benefit. Each investigation starts its own conversation, receives a self-contained objective, and inherits the same reviewed object, read-only tool surface and permission boundary. It cannot write, run commands, change scope, choose a different agent or delegate further. Read tools are confined to the reviewed workspace; host files outside it and credential files are not review evidence. Several investigations can run in parallel, foreground or background; their findings are attributed to the child that reported them, and their structured facts, examined scope, unresolved questions and limitations return to the Lead Reviewer as evidence. The Lead Reviewer remains responsible for verifying and synthesising the single final report — a child's report is not a verdict. A simple review does not need to delegate.

A review honors the project configuration's `limits` for tokens and cost and its 20-minute wall bound. Those caps are **shared** by the Lead and every investigation: a child never receives a fresh full budget. When the budget runs out, the review stops issuing new requests, keeps the facts it obtained, and states the incomplete scope honestly.

### Read the report

Once the review ends, click its conversation card to open the saved report. The card shows severity counts, the total number of findings, and whether the source changed. The card and report remain available when you reopen the session. If the reviewed object changed during the run, the report flags it.

The reviewer records findings with an optional dimension (code, business, security, architecture, test, delivery, session), P0–P3 severity, confidence, evidence, impact, a fix direction and whether it reproduced them. It also declares a structured verdict through `report_review`: a conclusion (issues found / none found / inconclusive), per-dimension coverage (reviewed, partial, not reviewed, not applicable), and limitations attributed to the environment, permissions, tool capability or the requested scope. The runtime records what was declared; it never derives these from the report prose or finding counts. A finding is information: it never blocks a goal or changes a turn's terminal state.

Investigator findings arrive at settlement as **candidates**, never as formal defects. The Lead Reviewer alone decides each one with `adjudicate_finding`: `adopted` (a formal finding, optionally with an adjusted severity/confidence/title), `rejected` (evidence against it; never counted), `duplicate` (same root cause; merged into a named finding), or `unresolved` (still undecided). A candidate the Lead does not mention stays unresolved; the runtime never adopts or rejects one implicitly. The formal defect count is the Lead's own findings plus explicitly adopted candidates; pending candidates are reported separately, and rejected or merged candidates remain in the record for traceability without inflating the count. Several investigators reporting one root cause should be merged into one finding.

The `report` field of `report_review` is the **formal report body**: complete, standalone, and explicitly submitted by the model. The final chat message is stored and shown separately and never replaces the report. Every `report_review` is a full revision bound to the evidence revision at submission. A new investigation result, a finding from the Lead, or a changed adjudication advances that revision. If the report is not resubmitted, the review can still complete and the report is marked as missing newer evidence.

`Completed` means the reviewer reached a clean end, not that no defects exist. A declared coverage of `reviewed` is the reviewer's statement about its own scope, not a runtime proof. `No findings` is not proof of correctness or security. An incomplete or cancelled review keeps whatever it had already established and says what stopped it.

## Troubleshooting

Run `leveler doctor` to check models, terminal support, and system capabilities. Use `leveler trace` to inspect session records. Check for private content before sharing them.
