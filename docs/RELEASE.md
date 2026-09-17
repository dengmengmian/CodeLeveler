# Public beta

Chinese: [`RELEASE.zh-CN.md`](RELEASE.zh-CN.md)

First public beta. macOS, Linux, and Windows.

## Included

- Terminal UI (`leveler tui`), Web UI (`leveler web`), and CLI (`leveler run`)
- Sessions stored on your machine, so you can resume later
- Custom agents: a directory with `agent.yaml` and `instructions.md`
- Mobile app and host-side remote bridge (`leveler remote`)
- Approval for file writes and commands
- Command isolation: Seatbelt on macOS, bubblewrap on Linux, Low integrity on Windows

## Known limits

- Prebuilt binaries are not Apple- or Microsoft-signed. macOS or Windows may block the first run until you allow it
- Commands and config may still change before 1.0. `run`, `resume`, and `tui` are intended to stay stable; `eval` and `remote` may not
- Windows cannot deny network access per command. A command that requires that isolation is refused rather than run with the network open
- Windows has no local daemon socket. Sessions and `resume` still work
- A confined Windows `!command` prints its output when it finishes, not while it runs

Install and usage: [README](../README.md).
How the system is layered: [Architecture](ARCHITECTURE.md).
