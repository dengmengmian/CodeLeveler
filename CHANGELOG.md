# Changelog

[中文](CHANGELOG.zh-CN.md) · [Install and use](README.md)

## [Unreleased]

## [1.0.21] - 2026-10-29

- Improved reasoning history when switching models, keeping private reasoning separate between models. Historical reasoning is sent back only to the route that produced it, and only when that route declares the mode.
- Fixed follow-up questions being treated as new goals after task completion, and improved resuming paused work. Ending a goal returns the session to ordinary chat.
- Goal completion follows the request and its evidence. A path that was only permitted is not a required deliverable. A refused check can be recovered without starting the goal over.
- Added a `/memory` management page to search, read, and manage records; `/memory status` shows processing activity.
- User facts and experimental experience records have separate settings; experience recording is off by default. Recalled facts keep their source and authority.
- Improved retrieval for large memory collections and protected historical sensitive records in UI and logs.
- Improved project rule loading, session recovery, and completion checks to reduce missed requirements and repeated work.
- A local terminal keeps its output, its owner, and its file descriptors.

## [1.0.20] - 2026-10-19

- Added Shell Mode: enter `!` for command mode and use Tab to complete files, directories, and commands.
- Read complete local command output with `/output` and copy it with `/copy output`.
- `Ctrl+B` moves a running local command to the background; `Ctrl+C` stops the current execution.
- Improved user message backgrounds, long output folding, and plain-text copying.

## [1.0.19] - 2026-10-19

- Web search can use DeepSeek or Tavily independently of the conversation model.
- Search requires `[web.search]`; an API key alone no longer enables it.
- Homebrew installations now use `brew upgrade leveler` for updates.
- Simplified the terminal footer with direct access to background tasks, failures, and plans.

## [1.0.15] - 2026-10-13

- Tool activity is grouped and collapsed by default, with individual details and full file diffs available.
- Improved diagnostics and recovery for model connection failures and interrupted responses.
- Fixed sessions failing to start after a background execution was stopped.

## [1.0.14] - 2026-10-12

- Added input undo, redo, history search, and searchable `/help`.
- Added terminal and clipboard diagnostics, distinguishing sent copies from verified copies.
- Improved background task mouse controls, scrolling, and narrow layouts.

## [1.0.13] - 2026-10-10

- Improved long code blocks, streaming highlighting, and keyboard access to tool output.
- Improved cancellation and session recovery, and fixed Windows permission checks.
- Starting the TUI without an interactive terminal now reports an explicit error.

## [1.0.12] - 2026-10-07

- Aligned execution status across interfaces and fixed failures or missing answers being shown as complete.
- Added `/btw` for read-only side questions without interrupting the main conversation.
- Improved ordinary development in Auto mode and fixed permission state after switching modes or resuming.
- Fixed execution history and command output after reconnecting, plus narrow terminal display issues.

## [1.0.11] - 2026-10-02

- Added shared reasoning intensity settings through `/thinking`, defaulting to `high`.
- Added a desktop client runnable from source; release packages still contain the CLI only.
- Improved context usage reporting, long-session compaction, and background task recovery.
- Fixed tasks unable to continue because some model configurations produced zero context capacity.

## [1.0.0] - 2026-09-18

- First stable release with startup auto-update, `leveler update`, and terminal `/update`.
- Release archives include SHA-256 checksums; fixed tool-call pairing errors in long sessions.

## [0.1.0-beta.1] - 2026-09-17

- Public beta with terminal, browser, CLI, session recovery, and custom agents.
- Supports macOS, Linux, and Windows, with experimental mobile and remote connection sources.
- Packages lack Apple or Microsoft signing; Windows does not support a local background service or per-command network isolation.
