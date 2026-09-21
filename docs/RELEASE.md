# CodeLeveler 1.0.4

Chinese: [`RELEASE.zh-CN.md`](RELEASE.zh-CN.md)

A correctness-focused patch release for task continuation and durable memory. Installed stable builds pick it up automatically, or run `leveler update` / `/update`.

## Changed

- Resumed work now keeps an explicit objective anchor, root turn and goal identity, so later instructions amend the same lineage instead of being mistaken for unrelated work
- Durable-looking memory candidates wait for confirmation, while explicit `remember` commands still save immediately
- A later explicit preference that conflicts with the same remembered subject supersedes the old value and reports that the stale memory was updated
- Routine memory recall is shown once in Conversation instead of being duplicated as both a transcript note and a toast

## Fixed

- Goal checkpoints are scoped to their exact continuation lineage and cannot be consumed by unrelated chats or goals
- Restart recovery and explicit continuation preserve the authoritative goal identity, including reopening the exact settled goal when needed
- Repeated pending candidates for the same memory subject keep only the latest value; corrections also supersede compatible keyless direct memories from older builds, and retries cannot restore stale preferences out of order
- API keys, tokens, passwords and other credential-shaped values are rejected at the memory write boundary
- Sensitive entries left by older versions are hidden from automatic recall and from every model-facing memory action, including list, read, lexical search and vector search
- Memory consolidation only considers fresh user-authored turns, avoiding replay or internal-turn candidates

## Known limits

- Prebuilt binaries are not Apple- or Microsoft-signed. macOS or Windows may block the first run until you allow it
- Windows cannot deny network access per command. A command that requires that isolation is refused rather than run with the network open
- Windows has no local daemon socket. Sessions and `resume` still work
- A confined Windows `!command` prints its output when it finishes, not while it runs

Install and usage: [README](../README.md).
Updates: [README § Updates](../README.md#updates).
How the system is layered: [Architecture](ARCHITECTURE.md).
