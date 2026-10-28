# CodeLeveler 1.0.21

[中文](RELEASE.zh-CN.md) · [Version history](../CHANGELOG.md)

This release keeps reasoning with the model that produced it, makes long-term memory manageable, and checks a goal against the request and its evidence.

- **Reasoning:** historical reasoning is sent back only to the route that produced it, and only when that route declares the mode. Switching models does not mix one model's private reasoning into another. Model versions the product already excludes stay excluded.
- **Goals:** a follow-up after a finished task stays a conversation, and ending a goal returns the session to ordinary chat. Completion follows the request and evidence that still apply. A path you only permitted is not a required deliverable. A refused check can be recovered without starting the goal over.
- **Memory:** `/memory` searches, reads, and manages records. `/memory status` shows processing. User facts and experimental experience are separate settings; experience recording stays off unless you turn it on. Recalled facts keep their source and authority. Sensitive historical records stay out of the interface and logs.
- **Sessions and terminal:** project rules load with the session, paused work can resume, and a local terminal keeps its output, its owner, and its file descriptors.

The release archive is the command-line program for macOS (Apple Silicon and Intel), 64-bit Linux, and 64-bit Windows. There is no Linux ARM archive. The desktop client can be built from source and is not in this archive. Packages are not Apple-notarized or Microsoft-signed. Existing configuration remains usable; new experience recording stays off until enabled.

See the [home page](../README.md) for installation and updates, and the [changelog](../CHANGELOG.md) for the version history.
