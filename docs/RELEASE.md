# CodeLeveler 1.0.21

[中文](RELEASE.zh-CN.md) · [Version history](../CHANGELOG.md)

This release retires the memory system, makes configuration live, and treats a pause you asked for as a boundary the runtime respects.

- **Memory:** Memory 2.0 is retired. Background extraction, storage and retrieval, context injection, its tools, `/memory` commands and management screens are removed. Session history, resume, compaction, goals, plans and project rules are unchanged, and an existing database drops the retired queues and policy columns on upgrade.
- **Configuration:** editing `~/.leveler/config.toml` (or the repository bundle) applies to the next turn without a restart, and `/reload` re-reads it immediately. A running turn keeps the model and provider it started with; an explicit `/model` choice stays pinned. An incomplete write keeps the last working configuration and names the reason.
- **Goals and plans:** "Finish stage 1, then wait for my confirmation" ends the turn with stage 2 still `pending`, and it survives resume and compaction. Plan progress counts only the current plan. A request asked as a question keeps its authority. A chat turn that ends with open plan steps is told so instead of stopping silently.
- **Context and usage:** `/context` and `/usage` are one page, and `/trace`, `/context` and `/usage` open while the agent is running. Usage comes from the runtime's durable model-call ledger; an unmeasured provider, an unpriced model and a partly priced scope are never shown as `$0.00`.
- **Terminal:** opening the `/` command menu no longer crashes on terminals 40 columns and narrower, and committing a goal is no longer described as delegation.
- **Welcome:** an empty conversation opens on a welcome card with the brand mark, a short tip that names the useful commands, and clickable entries for session history, help and model selection. It steps aside as soon as the conversation has content.

The release archive is the command-line program for macOS (Apple Silicon and Intel), 64-bit Linux, and 64-bit Windows. There is no Linux ARM archive. The desktop client can be built from source and is not in this archive. Packages are not Apple-notarized or Microsoft-signed. Existing configuration remains usable. The `/develop` command is removed; `/goal` remains the explicit-goal entry point.

See the [home page](../README.md) for installation and updates, and the [changelog](../CHANGELOG.md) for the version history.
