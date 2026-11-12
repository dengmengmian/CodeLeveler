# Everyday use

[中文](USAGE.zh-CN.md) · [Home](../README.md)

## Configure a model

Run `leveler login` to choose a provider and model. Settings live in `~/.leveler/config.toml`. For another OpenAI-compatible service, run `leveler init` and edit that file.

Configuration is read again while the app runs: editing `~/.leveler/config.toml` (or the repository bundle) applies to the next turn, and `/reload` re-reads it immediately without restarting. A running turn keeps the model and provider it started with; a session that never chose a model follows the new default, while an explicit `/model` choice stays put. An invalid or partially written file is rejected and the last working configuration stays in force. Settings that can only apply on start-up, such as the UI language, are reported as requiring a restart instead of being reported as applied.

Use `/thinking high` to change reasoning intensity for the session, or `/thinking reset` to return to the configured default. Choices are `auto`, `off`, `minimal`, `low`, `medium`, `high`, and `max`; run `leveler doctor` to check model support. The default is `high`; `max` uses the model's strongest supported level, and `auto` leaves the choice to the provider.

A transient provider failure (connection refused or reset, a retryable timeout, 429, a retryable 5xx) is retried automatically with exponential backoff and jitter, without resending the original task. The default budget is 12 retries within 10 minutes; `Ctrl+C` cancels the wait. Change it in `~/.leveler/config.toml`:

```toml
[retry]
max_retries = 12        # retries after the first attempt; 0 disables automatic recovery
recovery_seconds = 600  # wall-clock ceiling for the whole recovery, from the first attempt
```

A permanent error (authentication, an invalid request, a rejected conversation) is never retried. When the budget runs out the failure is reported as resumable: the task and its completed tool work are kept, and sending a new message continues from there.

## Input and results

- `Ctrl+R` searches input history. Enter restores an entry to the draft without sending it.
- `Ctrl+Z` undoes input; `Ctrl+Shift+Z` or `Alt+Z` redoes it.
- Click a tool group for details, use `Ctrl+O` for the latest execution, or `Ctrl+G` for the final answer.
- Type `!` in an empty input to enter Shell Mode; Tab completes commands and paths.
- `/output` opens full local command output; `/copy output` copies it.
- An edit paints its complete applied diff by default; click its head row or press `Ctrl+O` to fold it to one `+A/-R` row. Dragging over the diff copies the selected source lines (no gutter, no `+`/`-` marker); `/copy diff` copies the complete applied patch instead.
- `Ctrl+B` moves a running local command to the background; `Ctrl+C` stops that execution.

Open background tasks from the footer to inspect output or stop an individual task. Esc only returns to the list. Closing the terminal does not stop background tasks when connected to the background service.

Without `timeout_seconds`, a foreground command still running after 15 seconds moves to the background and returns a `task_id`. With `timeout_seconds`, the call waits for the command: a deadline up to 120 seconds ends it in that call, and a longer deadline is waited for up to 120 seconds before the still-running process moves to the background (the deadline continues to follow it). A move is not a successful exit: observe it with `get_task` / `wait_task` and stop it with `kill_task`. `timeout_seconds` is a process execution deadline counted from process start. Start a long-lived service with `background=true`; its lifetime follows `background_lifetime` (`goal` by default, or `runtime` / `session` / `persistent`), not the wait budget.

## Sessions and background work

Use `leveler resume` to continue a session. On macOS and Linux, `leveler serve` also keeps work running in the background. A local background service is not supported on Windows.

If the process that owns background services is lost (crash or kill), `leveler tui` keeps starting: affected tasks become `unknown`, and the startup notice says whether their process was still present. CodeLeveler never signals or re-runs them in that state; use `leveler background list` to inspect what was left, `leveler background logs` for the retained output, and `leveler background journal list` for write reservations that remain unresolved.

`leveler run "task"` runs a task to its end. Add `--collaboration chat` for a single response.

To try multiple candidate implementations, use `leveler run "task" --parallel 3`. This needs a clean, committed Git working tree. It creates branches and worktrees, commits verified candidates, and integrates successful candidates into your current branch.

## Scheduled automation

`leveler schedule` keeps definitions in `~/.leveler/state/schedules.db`. Definitions are user-global and each runs in the workspace it names (the current repository by default). Create a daily shell run:

```sh
leveler schedule add --name nightly --command "cargo test" --daily 02:00 --timezone Asia/Shanghai --grant-unattended
```

Choose one action (`--command`, `--native-action ID` with `--native-input JSON`, or `--agent-goal TEXT`) and one trigger (`--at RFC3339`, `--every SECONDS`, `--daily HH:MM`, `--weekly mon,wed --local-time HH:MM`, or `--cron "..."` with `--timezone IANA`). `--session-policy continue|fresh`, `--overlap skip-if-running|queue-one|allow-concurrent` (`allow-concurrent` requires `fresh`), `--misfire skip|run-once` with `--misfire-grace SECONDS`, and `--timeout SECONDS` set the rest of the policy.

A run that happens with nobody present needs `--grant-unattended`; add `--allow-external-side-effects` when the action sends or changes something outside the workspace. Changing the action, workspace, trigger, permission mode or that choice invalidates the grant: use `leveler schedule grant <id> [--allow-external-side-effects]` for the new identity. Full permission does not replace a grant.

`leveler schedule list|show|update|runs|pause|resume|run-now|cancel|remove` inspect and manage definitions. `leveler schedule dispatch` runs one pass; `leveler schedule serve` keeps dispatching in the foreground. On macOS `leveler schedule enable` registers a launchd agent, and on Windows a per-user Task Scheduler task; `disable` and `uninstall` remove it, `status` and `wakeups` report it, and `doctor` lists checks. Linux has no wakeup backend and reports that; use `serve` or a system scheduler that calls `dispatch`.

In the TUI, `/schedules` opens the definition list, detail and edit form. The agent has the same operations through the `schedule_inspect` and `schedule_manage` tools.

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

Set the named environment variable before starting CodeLeveler. A key alone does not enable search without `[web.search]`. UI links and `/web` use your system browser.

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

## Troubleshooting

Run `leveler doctor` to check models, terminal support, and system capabilities. Use `leveler trace` to inspect session records. Check for private content before sharing them.

The mobile client remains a frozen source preview without an official install package. Remote session traffic passes through a relay without end-to-end encryption; self-host the relay when evaluating it.
