# CodeLeveler 1.0.23

[中文](RELEASE.zh-CN.md) · [Version history](../CHANGELOG.md)

This release makes reconnect and crash recovery evidence-based: when CodeLeveler cannot prove what happened, it says so instead of guessing.

- **Reconnect:** the runtime reports which build is serving the session and whether its process changed across a reconnect. A version mismatch stays visible as an issue, a turn that was running is reported as possibly interrupted or unknown rather than stopped without evidence, and input and approval choices are held when the connection is down.
- **Crash recovery:** a turn interrupted while tool calls had no recorded result pauses instead of retrying. `/recover` lists each call as interrupted, state unknown, or interrupted before execution; confirming marks them "result unknown, not replayed" — nothing is replayed and no call is reported as successful.
- **Messages:** each submission reports whether it was delivered, rejected with its content restored, or unconfirmed. A message whose owning process died is never retried automatically.
- **Background tasks:** status distinguishes a task lost with its background process, an unconfirmable state and an identifier conflict from a task that is running or finished.
- **Input:** `→` accepts the inline next-step suggestion like Tab; the ghost becomes editable with the caret at its end, and nothing is sent until Enter. With no suggestion showing, it remains an ordinary caret move.
- **Goals:** explicit single-letter field labels are recognized in goal accounting, while incidental prose and evidence-only mentions are still rejected.

The release archive is the command-line program for macOS (Apple Silicon and Intel), 64-bit Linux, and 64-bit Windows. There is no Linux ARM archive. The desktop client can be built from source and is not in this archive. Packages are not Apple-notarized or Microsoft-signed. Existing configuration remains usable.

See the [home page](../README.md) for installation and updates, and the [changelog](../CHANGELOG.md) for the version history.
