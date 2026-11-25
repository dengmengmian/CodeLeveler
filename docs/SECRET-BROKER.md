# Secret Broker

CodeLeveler can take custody of a credential the user hands over, keep the
plaintext out of the model's context and the ordinary stores, and let the host
write it to a dotenv file the user asked for. This document is the maintained
contract. The detailed design, threat model and acceptance record live in
`docs/dev/secret-broker/` (local evidence, excluded from the source tree).

## What the feature guarantees

- **The model never receives a captured value.** The user's message carries an
  opaque `[secret:sec_…]` reference; the value lives in a process-local,
  session-bound vault and is borrowed only for the duration of one host write.
- **The value is not written to the ordinary session stores.** It does not
  enter `session_messages`, turn payloads, the WAL, events, transcripts,
  drafts or tool results. The authorized `.env` is the documented exception.
- **A reference is useless outside its capture.** It is 128 random bits and is
  checked against the session, a 12-hour TTL, the revocation state and the
  process incarnation on every use. A guessed, malformed, foreign, revoked,
  expired or post-restart reference reports the same `Unavailable` answer.
- **Use is bound to what the user declared.** A reference is minted with an
  empty use scope. Every admitted message that mentions the reference adds the
  dotenv targets it names (`.env`, `.env.<suffix>`, including a nested path)
  and the fields it explicitly assigns (`NAME=`). `secret_apply` may write
  only to those targets and fields. A request outside a non-empty scope is
  refused in every permission mode, including Full. A dimension the message
  never named is left to the model; in Auto and Restricted the first use of
  such a reference asks the person, and that approval is bound to the exact
  call (no standing rule, no reuse for another target or field).
- **The target family is dotenv files only.** The one path-authority exception
  is a workspace-relative `.env` / `.env.*`; `.git`, `.ssh`, `.aws`, the
  definition trust stores, agent and skill definition trees and the workspace
  bound still apply. The write is the ordinary atomic editor: lock, exact-byte
  compare-and-swap, symlink/junction identity revalidation, descriptor-relative
  replace or the Windows capability path.
- **File permissions are private.** A new file is `0600` on Unix. On Windows,
  the host applies and verifies an owner-only, inheritance-protected DACL
  (current user, SYSTEM, Administrators). An existing file is tightened even
  when the value is unchanged.
- **The plaintext acknowledgement is a per-command decision.** It is carried
  by that one `SubmitUserInput` / `SteerUserInput`, never stored as custody,
  never combined with captures, and never reused for a later command. It
  means "the model and provider may see these bytes"; the ordinary
  model-visible sanitizer still keeps the value out of the local session
  store.

## Channels

| Entry | Detection and review | Capture | Plaintext |
| --- | --- | --- | --- |
| TUI chat / Goal / Steer | review opens for every finding | yes | explicit choice in the review |
| `/secret` | masked paste, no detection needed | yes | n/a |
| Resume | the replayed text goes through the same admission | yes when captured before | explicit choice |
| Older clients (`SubmitMessage`, `SteerCurrentTurn`, …) | no review surface | no | no; any finding is refused |
| Direct client / IPC | same runtime admission | yes when it sends the new command | per-command ack |
| `leveler run` (headless) | no review surface | no | only `--allow-plaintext-secrets` |

A client cannot set `plaintext_secret_ack` on behalf of a tool or a model: the
tool surface has no client-command channel. The local socket and the optional
TCP endpoint require the runtime access token; a same-user process that is not
confined can read that token and act as the user. The product does not claim
process isolation between same-user processes.

## Honest limits

- A detected candidate is not proof of a credential, and no detector catches
  every unknown format. `/secret` is the guaranteed lane for anything the
  detector cannot name.
- Detection is local and bounded; it does not claim a real-world recall rate.
- The written `.env` is plaintext on disk. Anything that can read that file
  after the write can leak it. Full mode provides no filesystem or network
  isolation; under Auto/Restricted the existing sandbox and permission mode
  decide who can read it and where a process may send it.
- Memory copies are zeroized on drop, but the allocator, the OS (swap, core
  dumps) and child processes are outside this boundary.
- `secret_inspect` verifies presence and non-emptiness, never the value and
  never a live provider connection.
- The vault is process-local. A restart loses every value; the transcript
  keeps dead references and the user must provide the credential again.
- A multi-line value (an unencrypted PEM block) can be captured but the 1.0
  dotenv writer refuses it rather than writing an invalid quoted value.

## Implementation map

| Component | Location |
| --- | --- |
| Detection and use-scope extraction | `crates/leveler-core/src/secret.rs` |
| Vault, references, use scope | `crates/leveler-execution/src/secret_vault.rs` |
| Admission transform | `crates/leveler-app/src/secret_input.rs` |
| Wire types | `crates/leveler-client-protocol/src/secret.rs` |
| Broker tools | `crates/leveler-tools/src/tools/secret_apply.rs` |
| Path authority | `crates/leveler-execution/src/workspace.rs` |
| Atomic editor | `crates/leveler-tools/src/workspace/editor.rs` |
| TUI review and `/secret` | `crates/leveler-tui/src/overlay/secret.rs`, `reducer/` |
| Headless input refusal | `crates/leveler-cli/src/run_cmds.rs` |
| Windows private DACL | `crates/leveler-win-confine/src/lib.rs` |
