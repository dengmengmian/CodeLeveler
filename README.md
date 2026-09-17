<p align="center">
  <img src="assets/brand/codeleveler-app-icon.svg" width="88" alt="CodeLeveler logo">
</p>

<h1 align="center">CodeLeveler</h1>

<p align="center">
  <strong>A local-first coding agent that works in your repository and leaves reviewable changes when code needs to change.</strong>
</p>

<p align="center">
  <a href="README.zh-CN.md">中文</a> ·
  <a href="LICENSE-APACHE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="Apache 2.0 License"></a>
</p>

Give CodeLeveler a task. It can read the repository, edit files, run builds and tests, and use Git. Conversations, approvals, events, and changed files remain available when a session stops. Session state is stored on your machine, and model requests go to the provider you configure.

The terminal UI (`leveler`), Web UI (`leveler web`), and headless CLI (`leveler run`) use the same runtime. CodeLeveler currently targets macOS, Linux, and Windows.

```sh
cd your-project
leveler
# or
leveler run "find the failing tests and fix them"
```

## What it does

- Produces repository changes you can inspect instead of treating an agent's completion claim as proof.
- Persists sessions so you can close the UI and continue with `leveler resume`.
- Supports DeepSeek, Moonshot/Kimi, OpenAI, Anthropic, and other OpenAI-compatible endpoints.
- Provides an explicit `/develop` workflow for Analyze → Coding → Verify → Review.
- Offers optional browser automation, web search, custom agents, and parallel candidate worktrees.

## Install from source

Until release artifacts are published from the new repository, building from source is the supported installation path. After cloning this repository, install Rust 1.90+ and Git, then run:

```sh
cargo install --path crates/leveler-cli --locked
leveler --version
```

Do not use release assets, the Homebrew formula, or upgrade URLs from the previous repository. They refer to older releases and are not the distribution channel for this repository.

On Linux, install `bubblewrap` before running agent commands:

```sh
sudo apt install bubblewrap
```

CodeLeveler can start without it, but commands that require Linux isolation fail closed instead of running unsandboxed. Run `leveler doctor` to inspect the capabilities available on the current machine.

## First run

```sh
leveler login
leveler doctor
cd your-project
leveler
```

`leveler login` supports DeepSeek, Moonshot/Kimi, OpenAI, and Anthropic. When the provider supports model discovery, it lists the models exposed to the supplied key. The command writes `~/.leveler/config.toml`; on Unix it restricts that file to mode `0600`. Skip the menu with `leveler login deepseek` or `leveler login moonshot`.

For another OpenAI-compatible endpoint, run `leveler init` and edit `~/.leveler/config.toml`. [configs/example.yaml](configs/example.yaml) is an annotated schema reference only; that YAML file is not loaded as configuration.

A clean Git worktree is recommended so changes remain easy to inspect or discard.

## Commands

| Goal | Command |
| --- | --- |
| Open the terminal UI | `leveler` or `leveler tui` |
| Run the full development workflow | In the TUI: `/develop <goal>` |
| Open the browser UI | `leveler web` |
| Run one headless task | `leveler run "…"` |
| Continue until the goal reaches a terminal result | `leveler run "…" --collaboration goal` |
| Run parallel candidates in isolated worktrees | `leveler run "…" --parallel 3` |
| Reopen a session in the TUI | `leveler resume [session-id]` |
| Inspect a session's event log | `leveler trace [session-id]` |

`/develop` runs Analyze → Coding → Verify → Review in one session. Without `[develop].model`, its reading stages use the session model. A configured model must be written as `provider/model`; an invalid value is an error, not a fallback.

`--parallel` is a separate workflow, not runtime sub-agent delegation. It requires a clean, committed Git tree, creates isolated worktrees and branches, commits verified candidates, and integrates successful candidates into the current branch.

On macOS and Linux, `leveler serve` keeps the runtime alive behind a local Unix socket after a UI closes. Windows has no local Unix-socket daemon; persisted sessions and `resume` still work.

## Permissions and isolation

The default permission profile is `assisted`. Ordinary repository writes, builds, tests, network actions, and commands such as `git push` or package publishing may run automatically inside the available OS sandbox. Irreversible deletion, privilege escalation, and host-escape operations require approval. Use `request-approval` when you want a stricter approval boundary.

| Platform | Isolation for restricted commands |
| --- | --- |
| macOS | Seatbelt |
| Linux | bubblewrap; install it before using restricted commands |
| Windows | Low integrity. Commands that require network denial are refused because per-command network isolation is not available. |

`leveler doctor` reports the effective host capabilities. If a restricted mode needs an isolation backend that is missing, execution fails instead of pretending to be sandboxed.

## Browser and web search

These tools are optional. They are exposed only when the selected work profile enables them and the current machine can provide them.

- **Chrome, Edge, or Chromium:** CodeLeveler starts a dedicated automation session. `browser_tab`, `browser_act`, and `browser_inspect` can navigate, interact, and inspect console, page-error, and network records.
- **Safari:** opt in with `default = "safari"` under `[browser]` in `~/.leveler/config.toml`, then enable Safari Remote Automation. Safari supports tabs and interaction, but its WebDriver backend cannot provide console, page-error, or network inspection.
- **Web search:** set `LEVELER_SEARCH_API_KEY` to a Tavily key. `web_search` performs one HTTP request with a 10-second timeout and no retry; it does not fall back to browser automation.

TUI `/web` and links opened from the UI still use the operating system's default browser.

## Custom agents

A custom agent is a directory containing `agent.yaml` (declared capabilities) and `instructions.md` (working instructions). Create one through CodeLeveler, through Settings → Agents in the Web UI, or by committing `.leveler/agents/<name>/`.

The coding harness resolves the definition, while capability admission and host authority enforce its boundaries. Instructions cannot grant extra tools or write access. See [Custom agents](docs/AGENT_EXTENSIBILITY.md).

## Experimental mobile client

`apps/leveler-mobile` is a frozen source preview, not a distributed product. The current evidence covers an iOS simulator pairing flow. Android builds, physical-device and cellular testing, TestFlight, and Play distribution have not been completed.

Remote control sends session traffic through the configured relay. Messages are signed, but end-to-end AEAD encryption is not implemented yet; the TLS terminator can read session traffic. Self-host the relay if you evaluate this feature. See [the mobile status](apps/leveler-mobile/README.md) before using it.

## Documentation

- [Architecture](docs/ARCHITECTURE.md)
- [Custom agents](docs/AGENT_EXTENSIBILITY.md)
- [Release notes](docs/RELEASE.md)
- [Changelog](CHANGELOG.md)
- [Documentation index](docs/README.md)
- [Security policy](SECURITY.md)

Run `leveler --help` for the complete command list. The project is pre-1.0: `eval`, `remote`, and other experimental interfaces may change.

Security vulnerabilities belong in the private process documented in [SECURITY.md](SECURITY.md), not in a public issue.

Apache License 2.0. See [LICENSE-APACHE](LICENSE-APACHE).
