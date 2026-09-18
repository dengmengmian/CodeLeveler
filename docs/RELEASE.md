# CodeLeveler 1.0.1

Chinese: [`RELEASE.zh-CN.md`](RELEASE.zh-CN.md)

A bug-fix release on top of 1.0.0. Installed 1.0.0 builds pick it up automatically, or run `leveler update` / `/update`.

## Fixed

- Starting the TUI while an older local runtime is still busy no longer fails after 10 seconds: the client waits for the old runtime to finish and hands over to the new one
- A handover no longer hangs when another client has already started the replacement runtime
- A development build is recognised by the runtime it started, so it no longer restarts an identical runtime on every launch
- An idle background runtime shuts itself down once no client is attached and no work is running
- Every visible tool row in the TUI names what ran
- A message typed while a turn runs is sent automatically as the next turn once the runtime is ready, in order and only into the session it was written for; waiting no longer shows as "status unknown"
- Option lists in questions, pickers and approvals are numbered, and labels stay aligned when moving the cursor or past 9 → 10

## Known limits

- Prebuilt binaries are not Apple- or Microsoft-signed. macOS or Windows may block the first run until you allow it
- Windows cannot deny network access per command. A command that requires that isolation is refused rather than run with the network open
- Windows has no local daemon socket. Sessions and `resume` still work
- A confined Windows `!command` prints its output when it finishes, not while it runs

Install and usage: [README](../README.md).
Updates: [README § Updates](../README.md#updates).
How the system is layered: [Architecture](ARCHITECTURE.md).
