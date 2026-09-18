# CodeLeveler 1.0.0

Chinese: [`RELEASE.zh-CN.md`](RELEASE.zh-CN.md)

The first stable release. macOS, Linux, and Windows.

From 1.0.0 on, CodeLeveler tracks [Semantic Versioning](https://semver.org/): `1.0.x` are bug fixes, `1.x.0` are backwards-compatible features, and `2.0.0` would be a breaking change. Installed builds check for and install newer stable releases automatically — at start-up, or with `leveler update` / `/update`.

## Included

- Terminal UI (`leveler tui`), Web UI (`leveler web`), and CLI (`leveler run`)
- Sessions stored on your machine, so you can resume later
- `/develop` workflow: Analyze → Coding → Verify → Review
- Custom agents: a directory with `agent.yaml` and `instructions.md`
- Mobile app and host-side remote bridge (`leveler remote`)
- Approval for file writes and commands
- Command isolation: Seatbelt on macOS, bubblewrap on Linux, Low integrity on Windows
- Self-update from GitHub Releases, with SHA-256 verification before anything is installed

## Known limits

- Prebuilt binaries are not Apple- or Microsoft-signed. macOS or Windows may block the first run until you allow it
- Windows cannot deny network access per command. A command that requires that isolation is refused rather than run with the network open
- Windows has no local daemon socket. Sessions and `resume` still work
- A confined Windows `!command` prints its output when it finishes, not while it runs

Install and usage: [README](../README.md).
Updates: [README § Updates](../README.md#updates).
How the system is layered: [Architecture](ARCHITECTURE.md).
