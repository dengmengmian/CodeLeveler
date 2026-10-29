<p align="center">
  <img src="assets/brand/codeleveler-app-icon.svg" width="88" alt="CodeLeveler logo">
</p>

<h1 align="center">CodeLeveler</h1>

<p align="center">A coding agent that works in your local repository</p>
<p align="center"><a href="README.zh-CN.md">中文</a> · <a href="LICENSE-APACHE">Apache 2.0</a></p>

Give CodeLeveler a task to read code, edit files, and run tests. Inspect the changes as it works and resume your session later.

Use the terminal, browser, or command line with BigModel, DeepSeek, Moonshot/Kimi, OpenAI, Anthropic, or other OpenAI-compatible services. Sessions stay on your machine; model requests go to your configured provider.

## Install

macOS and Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/dengmengmian/CodeLeveler/main/install.sh | sh
```

The default install directory is `~/.local/bin`; make sure it is on your `PATH`.

Or use Homebrew (macOS and Linux x86_64):

```sh
brew install dengmengmian/tap/leveler
```

On Windows, download the matching archive from [Releases](https://github.com/dengmengmian/CodeLeveler/releases), verify its supplied SHA-256 checksum, extract it, and add the directory to `PATH`. Keep `leveler.exe` and `leveler-confine.exe` together.

On Linux, install `bubblewrap` before using restricted commands, for example with `sudo apt install bubblewrap`.

## Get started

```sh
leveler login              # choose a provider and configure an API key
cd your-project
leveler
```

Start with a clean Git working tree so changes are easy to review. Run `leveler doctor` to diagnose configuration or environment issues.

| Task | Command |
| --- | --- |
| Open the terminal UI | `leveler` |
| Open the browser UI | `leveler web` |
| Run a task directly | `leveler run "find the failing tests and fix them"` |
| Resume a session | `leveler resume` |
| Show help | `leveler --help` |

`leveler run --parallel 2` preserves the selected permission profile, network restriction and model-step ceiling, and supports `--output jsonl`. Parallel runs reject shipping flags (`--commit`, `--push`, `--pr`, `--branch`, `--pr-base`) before starting; ship the reviewed integration separately. For single-task runs, shipping works with either output format and shipping failures return a non-zero exit code. Candidate setup, execution or commit errors fail the run with their original cause before integration; the run still records candidate completion and cleans up its worktrees.

In the terminal, `/develop <goal>` walks through analysis, coding, verification, and review. Use `/help` for commands and shortcuts, and `/thinking` to adjust reasoning intensity.

## Permissions and privacy

The default Auto mode allows ordinary development actions and asks for approval for dangerous operations. Choose Restricted for stricter approval, or Full to allow all operations in an environment you trust.

The model may read workspace content and send relevant material to your provider. Keep secrets outside workspaces accessible to the agent. See the [security policy](SECURITY.md) for permission and platform limitations.

## Update

For Homebrew installs:

```sh
brew upgrade leveler
```

Other installs check for stable updates automatically. To update manually:

```sh
leveler update
```

## User guides

- [Everyday use](docs/USAGE.md)
- [Custom agents](docs/AGENT_EXTENSIBILITY.md)
- [Durable memory](docs/MEMORY.md)
- [Latest release](docs/RELEASE.md)
- [Version history](CHANGELOG.md)

Report security vulnerabilities privately using the [security policy](SECURITY.md).
