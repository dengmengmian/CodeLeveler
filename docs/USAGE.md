# Everyday use

[中文](USAGE.zh-CN.md) · [Home](../README.md)

## Configure a model

Run `leveler login` to choose a provider and model. Settings live in `~/.leveler/config.toml`. For another OpenAI-compatible service, run `leveler init` and edit that file.

Use `/thinking high` to change reasoning intensity for the session, or `/thinking reset` to return to the configured default. Choices are `auto`, `off`, `minimal`, `low`, `medium`, `high`, and `max`; run `leveler doctor` to check model support. The default is `high`; `max` uses the model's strongest supported level, and `auto` leaves the choice to the provider.

## Input and results

- `Ctrl+R` searches input history. Enter restores an entry to the draft without sending it.
- `Ctrl+Z` undoes input; `Ctrl+Shift+Z` or `Alt+Z` redoes it.
- Click a tool group for details, use `Ctrl+O` for the latest execution, or `Ctrl+G` for the final answer.
- Type `!` in an empty input to enter Shell Mode; Tab completes commands and paths.
- `/output` opens full local command output; `/copy output` copies it.
- `Ctrl+B` moves a running local command to the background; `Ctrl+C` stops that execution.

Open background tasks from the footer to inspect output or stop an individual task. Esc only returns to the list. Closing the terminal does not stop background tasks when connected to the background service.

## Sessions and background work

Use `leveler resume` to continue a session. On macOS and Linux, `leveler serve` also keeps work running in the background. A local background service is not supported on Windows.

`leveler run "task"` runs a task to its end. Add `--collaboration chat` for a single response.

To try multiple candidate implementations, use `leveler run "task" --parallel 3`. This needs a clean, committed Git working tree. It creates branches and worktrees, commits verified candidates, and integrates successful candidates into your current branch.

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

## Troubleshooting

Run `leveler doctor` to check models, terminal support, and system capabilities. Use `leveler trace` to inspect session records. Check for private content before sharing them.

The mobile client remains a frozen source preview without an official install package. Remote session traffic passes through a relay without end-to-end encryption; self-host the relay when evaluating it.
