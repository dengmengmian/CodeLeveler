# CodeLeveler 1.0.2

Chinese: [`RELEASE.zh-CN.md`](RELEASE.zh-CN.md)

A feature and compatibility release on top of 1.0.1. Installed stable builds pick it up automatically, or run `leveler update` / `/update`.

## Added

- First-class Zhipu BigModel Coding Plan setup for `glm-5.3` and `glm-5.3-flash`, including reasoning, vision and 1M-context model profiles
- Current DeepSeek profiles for `deepseek-flash` and `deepseek-v4-pro`, with vision and parallel-tool capabilities where supported
- A unified skills registry, skill inspection and guarded skill-authoring workflow
- Background activity, detail and plan views in the TUI
- Durable semantic memory extraction with bounded reasoning, asynchronous batching and lifecycle recovery

## Changed

- Session continuation, cancel and resume now share one explicit runtime protocol across the CLI, TUI, host and remote-control surfaces
- `leveler login` and the default `leveler init` path write complete built-in model capabilities instead of generic placeholders
- The retired `deepseek-v4-flash` configuration is replaced by `deepseek-flash`

## Fixed

- DeepSeek thinking-mode conversations now return `reasoning_content` for every historical assistant message when tools are present, preventing multi-turn tool requests from being rejected
- DeepSeek forced tool choices disable thinking without silently dropping an explicitly supplied temperature
- Dynamic DeepSeek peak/off-peak and cache pricing is no longer represented as an inaccurate static USD price

## Known limits

- Prebuilt binaries are not Apple- or Microsoft-signed. macOS or Windows may block the first run until you allow it
- Windows cannot deny network access per command. A command that requires that isolation is refused rather than run with the network open
- Windows has no local daemon socket. Sessions and `resume` still work
- A confined Windows `!command` prints its output when it finishes, not while it runs

Install and usage: [README](../README.md).
Updates: [README § Updates](../README.md#updates).
How the system is layered: [Architecture](ARCHITECTURE.md).
