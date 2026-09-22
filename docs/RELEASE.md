# CodeLeveler 1.0.5

Chinese: [`RELEASE.zh-CN.md`](RELEASE.zh-CN.md)

A first-run and idle-experience release on top of 1.0.4. Installed stable builds pick it up automatically, or run `leveler update` / `/update`.

## Added

- Running `leveler` with no configuration now starts the same first-run setup as `leveler login`, beginning with an explicit language choice (English / 中文) that is written to the config as `lang`
- A fresh install enables start-up auto-update by default; package-manager users opt out with `[update] auto_update = false`
- The config written by `leveler login` and `leveler init` carries bilingual comments for every setting it emits
- The TUI predicts the user's next message as a transient ghost after a turn, and on opening an empty session it derives a starter from repository context. Tab accepts, Esc dismisses; nothing is persisted or submitted on its own
- After three idle minutes in a substantial conversation, the TUI shows one muted recap of where the work reached and what comes next
- The client protocol adds `RequestPromptSuggestion` / `RequestAwaySummary` and the `PromptSuggestion` / `AwaySummary` events (protocol minor 1.12); the remote surface refuses both commands

## Changed

- Durable memory follows Claude Code-style boundaries: extraction classifies each candidate as user, feedback, project, reference, derived or task_state, and repository-derived truth and short-lived task state are refused rather than saved
- Memory confirmations read as one calm sentence per batch instead of listing candidate ids, and the memory-change notes in Conversation are phrased as plain sentences
- A fully settled plan checklist is no longer kept as conversation history after its turn ends

## Fixed

- A late model-generated suggestion or recap can no longer appear after the user has started typing or a new turn has begun
- An idle recap is one-shot: a consumed deadline never fires again, and it is refused while a turn is running or text is staged

## Known limits

- Prebuilt binaries are not Apple- or Microsoft-signed. macOS or Windows may block the first run until you allow it
- Windows cannot deny network access per command. A command that requires that isolation is refused rather than run with the network open
- Windows has no local daemon socket. Sessions and `resume` still work
- A confined Windows `!command` prints its output when it finishes, not while it runs

Install and usage: [README](../README.md).
Updates: [README § Updates](../README.md#updates).
How the system is layered: [Architecture](ARCHITECTURE.md).
