# CodeLeveler 1.0.14

Chinese: [`RELEASE.zh-CN.md`](RELEASE.zh-CN.md)

This release makes a stalled provider answer diagnosable and recoverable, and
settles how the conversation presents work: tool activity accumulates into one
receipt per visible stage, reasoning stays in the status strip, and details open
only when asked for.
Web and Desktop are outside this release's qualification scope.

## Model responses

- A response body that cannot be read records what the transport had already seen — HTTP status, declared content type and encoding, bytes received, and a redacted tail of them — under the failed request, and `leveler trace` prints it. A `200` that broke mid-body is no longer indistinguishable from an upstream that never answered.
- Failure wording follows the delivery evidence: "cannot reach the service" only when nothing was sent, "receiving the response failed" once headers arrived, "response interrupted" for a stream cut mid-flight, and "communication failed" when the transport cannot say.
- A transient reasoning-only interruption may use the bounded retry lifecycle when no answer text or tool-call data was received. Reasoning is still generated (and possibly billed) content, so the failure stays `Caution` rather than blindly safe; each failed attempt is settled before the retry and the new attempt never merges abandoned reasoning. Answer text or a started tool call is still never replayed.

## Execution

- A host that died leaves its ready file behind. Liveness is now decided by the host's own lifecycle lock rather than the file's existence, so a dead host's residue is cleared and the runtime starts instead of failing with `Connection refused`.
- The execution host shuts down cleanly on SIGTERM, removing its ready file on the same path as an idle exit.
- A semantic write reservation the host never admitted is released on the next start, but only when the archive holds no fact for it and no spawn intent is outstanding. `leveler background journal list` and `leveler background journal abandon <task-id>` inspect and release them by hand.

## Presentation

- Consecutive ordinary tools accumulate into one receipt per visible stage, including a lone tool. The closed header reports real kinds and counts, marks failures separately, and states a running call's own state and count; folding never shows busy or cancelled work as finished.
- Reasoning no longer appears in the conversation. The status strip reports the streaming fact and clears it at completion, and Ctrl+O opens the latest tool group's details without substituting a thought.
- Tool and failure details start collapsed and open through their own header.
- A confirmed edit diff is shown in full by default, and progress prose and the Final are shown as written, without a per-paragraph prefix symbol.

## Fixes

- A permission-settings edit no longer treats answer progress since the observed snapshot as a settings conflict.
- Agent progress updates connect observed work to the goal instead of restating each tool call.

## Documentation

- The Chinese architecture document is rewritten as a usage guide arranged by what a user does, with the technical constraints kept in the same file.
