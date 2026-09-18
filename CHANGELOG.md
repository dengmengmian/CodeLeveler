# Changelog

Chinese version: [`CHANGELOG.zh-CN.md`](CHANGELOG.zh-CN.md)

Release notes: [`docs/RELEASE.md`](docs/RELEASE.md).

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
