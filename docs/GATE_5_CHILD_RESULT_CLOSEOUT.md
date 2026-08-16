# Gate 5 · Structured child result — closeout

**N1 → VERIFIED_FIXED (proof level B). N4 → BETA_STANDARD_CLOSED — see "What is not proven".**

## The defect, precisely

R007b: the child (Newton) reached `Headers.vue`, was killed by its 1200s wall budget, and handed
the parent the string `sub-agent cancelled`. Everything it had established was discarded at the
`Err(AgentError::Cancelled)` arm of `run_one_sub_agent_on`, which built its result from a literal.
The parent read the result as "investigated, nothing to report", never opened the file itself, and
closed the task. Two opposite instructions — *finished and found nothing* and *stopped before
finding anything* — were being carried by one free-text field, so no parent could tell them apart.

## What changed

`ChildResult { status, findings, stop_reason, partial }` in `leveler-agent/src/sub_agent.rs`, with
four statuses:

| Status | Means | Parent should |
| --- | --- | --- |
| `COMPLETED_WITH_FINDINGS` | clean end, produced a report | use the report |
| `COMPLETED_NO_FINDINGS` | clean end, nothing to flag — **this is a result** | treat the subject as investigated |
| `INCOMPLETE_PARTIAL` | stopped early; what it had reached survives in `findings` | use the partial, finish the rest |
| `INCOMPLETE_NO_RESULT` | stopped early with nothing | **not** "nothing to report" — redo the work |

Three supporting changes:

1. `run_one_sub_agent_on` captures the child's `AssistantText` as it runs, so a cancelled or failed
   child returns what it had already established instead of a literal. This is the actual N1 fix —
   the status enum only *names* the distinction, the capture is what preserves the content.
2. Every terminal arm (clean stop, non-success stop reason, cancellation, error, no concurrency
   slot) now builds a `ChildResult`; `SubAgentRunResult.text`/`.ok` are gone, so no call site can
   reconstruct the old ambiguity.
3. `ChildResult::for_parent` puts the status line **first**, so a result truncated by the output
   budget still says what kind of result it is. Both entrances use it: the `spawn_agent`
   aggregation in `drive.rs` and Gate 4's `run_reviewer_child`.

## Smokes (`crates/leveler-agent/tests/multi_agent_test.rs`)

| Smoke | Scenario | Asserts |
| --- | --- | --- |
| A | child answers with a finding | `COMPLETED_WITH_FINDINGS`, the finding text survives, `is_error == false` |
| B | child stays silent to a clean end | `COMPLETED_NO_FINDINGS`, never labelled `INCOMPLETE`, `is_error == false` |
| C | child is killed mid-investigation | `INCOMPLETE`, and `"Headers.vue sets the header in two places"` — said one round before the kill — reaches the parent |
| D | child's first model call fails | `INCOMPLETE_NO_RESULT`, never `NO_FINDINGS`, `is_error == true` |

Smoke B is worth a note: an empty assistant turn does not end a run (the loop nudges a silent
child first), so it takes three silent rounds to reach a clean end with an empty report. The
branch is reachable — this was checked, not assumed, because an unreachable status would have
been dead code.

## Reverse validation

Replacing the captured text with `""` — i.e. removing only the preservation, keeping the enum —
turns smoke C's result into exactly R007b's failure:

```
[sub-agent Euclid] status: INCOMPLETE_NO_RESULT (stopped: stopped before it could finish)
The sub-agent produced NO result. ...
```

So the capture, not the naming, is what carries the fix.

Regression: `leveler-agent` + `leveler-engine`, 459 tests, 0 failed. Clippy `--all-targets` clean,
`cargo fmt --check` clean.

## What is not proven

- **N4 → BETA_STANDARD_CLOSED, not VERIFIED_FIXED.** N4 is about the parent *consuming* what the
  child returned. The result is now unambiguous and the wording tells the parent what to do with
  each status, but whether a live model acts on `INCOMPLETE_NO_RESULT` by redoing the work is a
  behavioural claim that only a real-usage run can settle. Claiming otherwise would repeat R007b's
  mistake in the opposite direction.
- **No spawn retry was added.** R007b's child died on its wall budget; a retry would double the
  cost with no evidence it converges, so the child now reports its budget death honestly instead.
  Deliberate non-change.
- **Proof level B.** Unit + regression only; no daemon smoke, no live model.
