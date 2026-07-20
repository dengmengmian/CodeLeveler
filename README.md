<p align="center">
  <img src="assets/brand/codeleveler-app-icon.svg" width="88" alt="CodeLeveler logo">
</p>

<h1 align="center">CodeLeveler</h1>

<p align="center">
  <strong>From a coding request to a reviewable diff, in one terminal workflow.</strong>
</p>

<p align="center">
  <a href="README.zh-CN.md">中文</a> ·
  <a href="https://github.com/dengmengmian/CodeLeveler/actions/workflows/ci.yml"><img src="https://github.com/dengmengmian/CodeLeveler/actions/workflows/ci.yml/badge.svg" alt="CI"></a> ·
  <a href="LICENSE-APACHE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="Apache 2.0 License"></a>
</p>

CodeLeveler is a terminal coding agent that can inspect, edit, run, and verify
real repositories. Work interactively in the TUI or automate a task from the
CLI. Sessions, permissions, and project state stay on your machine; model
requests go only to the provider you configure.

Windows, macOS, and Linux are tested in CI. CodeLeveler is currently in public
beta (`0.1.x`).

## Three focused tools, one workflow

**CodeLeveler writes the code. ReviewGate reviews it. AgentGate connects both
to your model APIs.** Each tool works independently, or they can be used
together:

| Tool | Focus |
| --- | --- |
| **CodeLeveler** | Inspect, edit, run, and verify code in the terminal |
| [AgentGate](https://github.com/dengmengmian/agentgate-ai) | Adapt model APIs behind one local gateway |
| [ReviewGate](https://github.com/dengmengmian/ReviewGate) | Review code changes and surface high-confidence issues |

## Why CodeLeveler

- **A complete coding loop.** Explore a repository, make focused edits, run
  project checks, repair failures, and leave a reviewable diff.
- **Control stays with you.** Typed tools, approval rules, workspace boundaries,
  checkpoints, and platform-aware command isolation constrain what the agent
  may do.
- **Resume saved work.** SQLite-backed sessions preserve the conversation,
  pending approvals, tool results, diffs, and verification state for later
  review or resume.
- **Bring your own model.** Use configurable OpenAI-compatible providers without
  coupling the agent runtime to one model vendor.

## Quick start

### 1. Install

**Option A — Homebrew (macOS, recommended)**

```sh
brew install dengmengmian/tap/leveler
```

`brew upgrade leveler` moves you to newer releases.

**Option B — Download a prebuilt binary**

From the [latest release](https://github.com/dengmengmian/CodeLeveler/releases/latest),
download the archive for your platform, unpack it, and move `leveler` onto your
`PATH`. Example (replace `V`/`T` for your platform):

```sh
V=0.1.0; T=aarch64-apple-darwin   # or x86_64-apple-darwin, x86_64-unknown-linux-gnu
curl -LO https://github.com/dengmengmian/CodeLeveler/releases/download/v$V/leveler-v$V-$T.tar.gz
tar -xzf "leveler-v$V-$T.tar.gz"
sudo mv "leveler-v$V-$T/leveler" /usr/local/bin/
leveler --version
```

On Windows, download `leveler-v<version>-x86_64-pc-windows-msvc.zip`, unzip it,
and add the folder to `PATH`. After installing on any platform, `leveler
upgrade` moves you to newer releases.

**Option C — Build from source**

You need [Rust 1.90+](https://www.rust-lang.org/tools/install) and Git.

```sh
git clone https://github.com/dengmengmian/CodeLeveler.git
cd codeleveler
cargo install --path crates/leveler-cli --locked
```

**First run — security prompts.** The prebuilt binaries are not yet notarized
(macOS) or code-signed (Windows), so a first run may be blocked:

- **macOS:** the `curl … | sh` installer runs without a prompt (and strips the
  quarantine flag). If you downloaded the archive in a browser and Gatekeeper
  says "unverified developer", clear it once:
  `xattr -dr com.apple.quarantine ./leveler` (or right-click → Open, or System
  Settings → Privacy & Security → Open Anyway).
- **Windows:** SmartScreen may warn — "More info → Run anyway", and if the
  `.zip` was browser-downloaded, Properties → check "Unblock" first.
- **Linux:** no signing gate. For full sandboxing install `bubblewrap`
  (`sudo apt install bubblewrap`); without it `leveler` still runs but the
  sandbox degrades — `leveler doctor` reports what is available.

### 2. Configure a model

On Windows, set a persistent Leveler home and create the config from PowerShell:

```powershell
$levelerHome = Join-Path $HOME ".leveler"
[Environment]::SetEnvironmentVariable("LEVELER_HOME", $levelerHome, "User")
$env:LEVELER_HOME = $levelerHome
New-Item -ItemType Directory -Force $levelerHome
notepad (Join-Path $levelerHome "config.toml")
```

On macOS/Linux, create `~/.leveler/config.toml`. Put the following content in
the file:

```toml
default_model = "deepseek/deepseek-chat"

[providers.deepseek]
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"

[models."deepseek-chat"]
provider = "deepseek"
context_window = 131072
max_output_tokens = 8192
streaming = true
tool_calling = true
structured_output = true
```

Set the API key for the current shell:

```powershell
# PowerShell
$env:DEEPSEEK_API_KEY = "..."
```

```sh
# macOS / Linux
export DEEPSEEK_API_KEY="..."
```

A plaintext `api_key = "..."` is also supported in a local config file. Prefer
an environment variable on shared machines or for configuration stored in Git.

### 3. Check the setup and start

```sh
leveler doctor
leveler model probe deepseek/deepseek-chat
cd path/to/your/project
leveler
```

Or run a one-off task without opening the TUI:

```sh
leveler run "find the cause of the failing tests and fix it"
```

The default `assisted` permission profile asks before higher-risk actions. For
your first run, use a clean Git worktree so every change is easy to inspect or
discard.

## One workflow, several ways to use it

| Need | Command |
| --- | --- |
| Work interactively | `leveler` |
| Run one task | `leveler run "add validation to the order endpoint"` |
| Compare several perspectives | `leveler discuss "why is this test flaky?"` |
| Investigate read-only and produce a plan | `leveler plan "replace the cache implementation"` |
| Resume previous work | `leveler resume <session-id>` |
| Coordinate a larger task | `leveler run "fix the failing tests" --orchestrate` |

On macOS/Linux, a long-running interactive session can use `leveler serve` in
one terminal and `leveler` in another. The TUI reconnects to the
repository-local runtime instead of tying the work to one terminal process.
Windows supports persisted sessions and `resume`, but this daemon transport is
not available there yet.

## What happens during a task

1. **Understand** — search the repository, inspect symbols and relevant files,
   and establish a plan when the task needs one.
2. **Change** — apply typed file operations and run commands within the active
   permission and workspace boundaries.
3. **Verify** — discover or use configured format, build, and test commands;
   failures can trigger bounded repair attempts.
4. **Hand off** — keep the diff, transcript, verification result, and session
   state available for review or resume.

## Safety and platform support

CodeLeveler can modify files and execute local commands, so its safety boundary
is explicit rather than implicit.

| Platform | Process control | Restricted command execution |
| --- | --- | --- |
| Windows | Job Objects | AppContainer and ACL restrictions when available |
| macOS | Process-group cancellation | Seatbelt profiles |
| Linux | Process-group cancellation | Bubblewrap |

`leveler doctor` reports the capabilities actually available on the machine.
Restricted modes fail closed when a required isolation backend is unavailable;
process-tree control alone is never reported as a full sandbox.

Permission rules and hooks can be defined per user or per repository. Start
with the [configuration guide](docs/README.md),
[permission example](docs/permissions.example.yaml), and
[hook example](docs/hooks.example.yaml).

## Configuration and documentation

- [Documentation index](docs/README.md)
- [Project configuration example](docs/leveler-config-example.yaml)
- [Provider and model configuration schema](configs/example.yaml)
- [Architecture](docs/ARCHITECTURE.md)
- [Evaluation harness](evals/README.md)

Run `leveler --help` or `leveler <command> --help` for the CLI reference. Use
`leveler upgrade --check` to check for a newer release.

## Public beta

The command surface and configuration format may change before 1.0. Cross-
platform CI covers Windows, macOS, and Linux, but OS-level isolation still
depends on capabilities installed and enabled on each machine.

## Contributing and security

Contributions are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) before
opening a pull request. Report vulnerabilities through the private process in
[SECURITY.md](SECURITY.md), not a public issue.

## License

Apache License 2.0. See [LICENSE-APACHE](LICENSE-APACHE).
