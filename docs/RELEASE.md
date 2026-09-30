# CodeLeveler 1.0.11

Chinese: [`RELEASE.zh-CN.md`](RELEASE.zh-CN.md)

This release rebuilds how a model request is assembled, folded, and paid for.
The control context is now separate from the transcript, every delivered prompt
segment has one source and one authority, and every model attempt is recorded in
the request ledger.
Installed stable builds can update automatically, or use `leveler update`
or `/update`.

## Changed

- Prompt construction gives every delivered segment a single source, authority,
  and lifecycle. Providers may change how a segment is represented on the wire
  (a system message or a top-level system field) but not its order, source, or
  authority, and the control context no longer travels as part of the transcript.
- Wire encoding and context statistics read the same projected request, so the
  reported input size and the bytes actually sent cannot disagree. Folding
  pressure follows the projected request, project rules scoped to a directory
  survive a fold verbatim, and historical reasoning follows the route's replay
  contract.
- Every model attempt is recorded in the request ledger and the resource budget,
  including failures, retries, compaction summaries, and child task calls.
  Consumption is rebuilt from a scope's persisted request facts on recovery, so a
  background child can no longer keep spending a stale balance snapshot.
- Rule delivery is budgeted against the request: every rule stays authoritative
  even when only part of a rule file fits verbatim.
- The repository's `./dev` development entry point is documented in the release
  archives, for local verification and release qualification.

## Fixed

- A reduced-context route can declare `max_output_tokens` larger than its own
  `context_window`. Reserving that full completion against the window used to
  saturate the usable input capacity to `0`, so every request looked over the hard
  capacity and the run aborted with `context management failure: ... capacity 0`
  after the first tool batch. A bound of zero is not a bound the model declared,
  so folding now follows the quality boundary alone. Routes whose reservation
  fits their window are unchanged.
- The context snapshot produced before a request reaches the caller's observer
  instead of being dropped.
- A model-spend scope change no longer discards the task epoch's remaining
  command budget.
- The spend of a completed command is flushed when a batch is cancelled
  mid-round, instead of being lost with the cancelled round.
- A child task's rendered spawn brief is persisted on the child spec and reused
  on resume, so a resumed child sees the brief it started with.
- `/btw` read-only calls pass through the ToolHost admission pipeline instead of
  bypassing it.
- Background task writes are constrained to the OS execution boundary, task
  mutation settles only after the whole workload ends, and the live stdout and
  stderr channels are capped with an explicit truncation marker rather than
  growing without bound.

## Known limits

- Prebuilt binaries are not Apple- or Microsoft-signed. macOS or Windows may
  block the first run until you allow it
- Windows cannot deny network access per command. A command that requires that
  isolation is refused rather than run with the network open
- Windows has no local daemon socket. Sessions and `resume` still work
- A confined Windows `!command` prints its output when it finishes, not while
  it runs

Install and usage: [README](../README.md).
Updates: [README § Updates](../README.md#updates).
How the system is layered: [Architecture](ARCHITECTURE.md).
