# CodeLeveler 1.0.7

Chinese: [`RELEASE.zh-CN.md`](RELEASE.zh-CN.md)

This patch release improves long-running task convergence, background-task
control, and command safety.
Installed stable builds can update automatically, or use `leveler update`
or `/update`.

## Changed

- Pasted file references take less space in the TUI composer.
- The agent runtime no longer imposes a host-owned verification gate on task completion.

## Fixed

- Running background tasks now have visible Stop buttons in the TUI list and
  detail view. Stopping one task does not cancel the whole session. Closing a
  TUI attached to a daemon still leaves its background tasks running.
- CLI approval prompts show the operation and reason carried in the request
  description, with terminal control characters sanitized.
- The shell preflight treats comments as ending at the newline. Valid multiline
  scripts are no longer rejected because of commands on later lines, and
  later background commands and sensitive paths remain checked.
- Goal-mode runs receive one runtime-owned delivery reminder after an initial
  read-only round, preventing repeated self-audits from starving requested
  workspace edits.
- Redirects to `/dev/null`, `/dev/stdout`, and `/dev/stderr` no longer request
  unnecessary write elevation. Other absolute device paths remain protected.
- Repository metadata writes can be persistently approved without granting
  unrestricted filesystem access, so local Git commits work in assisted mode
  while writes outside the workspace remain confined.
- Background-task output identifies each retained stdout and stderr chunk,
  preserving the source of interleaved logs.

## Known limits

- Prebuilt binaries are not Apple- or Microsoft-signed. macOS or Windows may
  block the first run until you allow it
- Windows cannot deny network access per command. A command that requires that
  isolation is refused rather than run with the network open
- Windows has no local daemon socket. Sessions and `resume` still work
- A confined Windows `!command` prints its output when it finishes, not while
  it runs

Install and usage: [README](../README.md).
Updates: [README § Updates](../README.md#updates).
How the system is layered: [Architecture](ARCHITECTURE.md).
