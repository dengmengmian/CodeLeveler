<p align="center">
  <img src="assets/brand/codeleveler-app-icon.svg" width="88" alt="CodeLeveler logo">
</p>

<h1 align="center">CodeLeveler</h1>

<p align="center">A coding agent that works in your local repository</p>
<p align="center"><a href="README.zh-CN.md">中文</a> · <a href="LICENSE-APACHE">Apache 2.0</a></p>

CodeLeveler reads code, edits files, and runs tests in your local workspace. Inspect tool output and diffs in the terminal, reopen a session later, or run a task directly from the CLI. Choose BigModel, DeepSeek, Moonshot/Kimi, OpenAI, Anthropic, or another OpenAI-compatible provider.

## Install

The installer supports macOS (Apple Silicon and Intel) and Linux x86_64. It verifies the archive’s SHA-256 checksum.

```sh
curl -fsSL https://raw.githubusercontent.com/dengmengmian/CodeLeveler/main/install.sh | sh
```

The default install directory is `~/.local/bin`; make sure it is on your `PATH`.

Or use Homebrew (macOS and Linux x86_64):

```sh
brew install dengmengmian/tap/leveler
```

On Windows x86_64, download the matching archive from [Releases](https://github.com/dengmengmian/CodeLeveler/releases), verify its supplied SHA-256 checksum, extract it, and add the directory to `PATH`. Keep `leveler.exe` and `leveler-confine.exe` together.

On Linux, install `bubblewrap` before using restricted commands, for example with `sudo apt install bubblewrap`.

To build from source, see the [contributing guide](CONTRIBUTING.md#build-locally).

## Quickstart

After installing, configure a model provider and open the terminal UI from your project directory.

```sh
leveler login
cd /path/to/your/project
leveler
```

Try asking “Find the test entry points for this project and run the smallest relevant set of tests.” Enter `/help` in the terminal UI for commands, or run `leveler doctor` to diagnose setup issues.

| Task | Entry point |
| --- | --- |
| Inspect and edit a project interactively | `leveler` |
| Run a task directly from a shell or script | `leveler run "find and fix the failing tests"` |
| Reopen a session | `leveler resume` |
| Run a goal with acceptance criteria in the terminal UI | `/goal <task>` |

## Permissions and privacy

The default Auto mode allows ordinary development actions and asks for approval for risky operations. Restricted applies tighter resource and approval boundaries. Full permits unrestricted filesystem and network access at the CodeLeveler layer.

Session records stay on your machine. Model requests may include workspace content and go to your configured provider. See the [security policy](SECURITY.md) for permissions, data flow, and platform limits.

## Update

For Homebrew installs, run `brew upgrade leveler`. For other installs, run `leveler update` to get the stable release.

## Documentation, help, and contributing

- [Documentation index](docs/README.md) · [Everyday use and troubleshooting](docs/USAGE.md) · [Version history](CHANGELOG.md)
- [Contributing guide](CONTRIBUTING.md) · [Coding agent instructions](AGENTS.md)
- [Report a bug or request a feature](https://github.com/dengmengmian/CodeLeveler/issues/new/choose) · [Report a vulnerability privately](SECURITY.md#reporting-a-vulnerability)

## License

[Apache License 2.0](LICENSE-APACHE).
