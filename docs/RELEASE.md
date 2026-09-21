# CodeLeveler 1.0.3

Chinese: [`RELEASE.zh-CN.md`](RELEASE.zh-CN.md)

A focused TUI usability release on top of 1.0.2. Installed stable builds pick it up automatically, or run `leveler update` / `/update`.

## Changed

- Successful `wait_task` and `get_task` polling no longer adds repeated rows to Conversation; the background task remains represented once by its command row, footer and detail view
- Background command status now reads “running in background” / “后台运行” to describe the live state directly

## Fixed

- Failed background task waits remain visible with their task details instead of being hidden with successful scheduling polls
- Replayed sessions use the same background polling visibility rules as live sessions without dropping durable tool-call history

## Known limits

- Prebuilt binaries are not Apple- or Microsoft-signed. macOS or Windows may block the first run until you allow it
- Windows cannot deny network access per command. A command that requires that isolation is refused rather than run with the network open
- Windows has no local daemon socket. Sessions and `resume` still work
- A confined Windows `!command` prints its output when it finishes, not while it runs

Install and usage: [README](../README.md).
Updates: [README § Updates](../README.md#updates).
How the system is layered: [Architecture](ARCHITECTURE.md).
