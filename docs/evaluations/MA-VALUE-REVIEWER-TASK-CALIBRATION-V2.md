# MA-VALUE-REVIEWER-TASK-CALIBRATION-V2

**Status:** calibration complete · **G4 not cleared** · 2026-08-25
**Arm:** control only (`independent_review = "off"`) · **No Reviewer was run**

V2 tested the hypothesis that came out of
[V1](MA-VALUE-REVIEWER-TASK-CALIBRATION.md): that cross-step coordination
defects would be a blind spot where single-expression traps were not.

**The hypothesis is falsified.** Five new cases, one per coordination
category, all measured at ceiling.

## V1 lessons carried in

| Lesson | Applied |
| --- | --- |
| Known single-expression traps are not blind spots (path traversal 3/3, absent-vs-zero 3/3) | No V2 case turns on one expression |
| The old corpus scores with the test the agent can read | Every V2 case has a hidden `expect` written at verification time |
| Validate discriminance before spending model calls | All five cleared the reference/trap gate first |
| `> /dev/null` in `expect` made failures undiagnosable | Removed |

## Results

Control only, n=3, `deepseek/deepseek-v4-flash`, reviewer OFF.
Batch `MA-VALUE-REVIEWER-CALIBRATION-self-20260824T163706Z-967aed`.

| Case | Category | Pass | Rate | Rounds | Out tokens | Verdict |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| `go-openall-unwind` | cancellation cleanup | 3/3 | 100 % | 5.7 | 1 545 | reject |
| `go-retry-terminal-state` | retry state machine | 3/3 | 100 % | 7.3 | 3 219 | reject |
| `go-transfer-lock-order` | lock ordering | 3/3 | 100 % | 7.7 | 2 159 | reject |
| `go-wal-idempotent-recover` | partial-failure recovery | 3/3 | 100 % | 18.0 | 10 396 | reject |
| `go-session-reap-orphans` | resource ownership | 3/3 | 100 % | 7.0 | 5 417 | reject |
| `go-maplimit-first-error` | concurrency (held from V1) | 2/3 | 66 % | 7.0 | 8 311 | **hold** |

**17 of 18 runs passed.**

Each rejected case's trap implementation was hand-written and does fail the
hidden tests — the traps discriminate. The model simply does not fall into
them. Two examples, unprompted, from the agent's own output:

```go
// go-transfer-lock-order, rep 1
// Lock accounts in a consistent global order (by unique ID) so that
// concurrent transfers in opposite directions cannot deadlock.
first, second := from, to
if first.ID > second.ID { first, second = second, first }
```

```go
// go-retry-terminal-state, rep 1
if attempt < maxAttempts { sleep(attempt); st.Sleeps++ }
...
return st, errors.Join(ErrExhausted, last)
```

Lock-order inversion and the retry off-by-one are both correct on the first
try, with a comment explaining why.

## Why V2 failed, honestly

Two explanations fit the data. They are not separated by this run, and saying
which one is right would be a guess.

**1. The contracts were too explicit.** Each case's doc comment enumerated the
invariant the hidden test checks — `go-retry-terminal-state` literally said
*"with maxAttempts=3 and three failures, Sleeps is 2"*. That signposts the
trap. A real user request states a goal, not a checklist.

Against this reading: `go-transfer-lock-order` stated only the goal (*"no
possibility of deadlock"*) and never mentioned ordering by ID, and the model
still got it. So explicitness cannot be the whole story.

**2. The tasks asked for the idiom.** Reverse-order unwind, no sleep after the
last attempt, consistent lock order, idempotent recovery, reap-on-close — each
is the textbook answer to a textbook problem. The model has read the fix more
often than the bug.

The one case that still fails is the one that **contradicts** the idiom:
`MapLimit` must return the error of the *lowest failing input index*, where
every `errgroup`-shaped library returns the first error *in time*. The task
asks for something the idiom does not do.

That is a sharper reframing of what a Reviewer might be for: not *"did you
write the standard pattern"* — the model does — but *"did you notice this spec
is not the standard pattern"*.

Whether that reframing survives contact with data is the next experiment, and
it is not claimed here.

## A defect found in the held case

`go-maplimit-first-error` was **nondeterministic**, and the calibration data
above is contaminated by it.

`TestHiddenLowestIndexErrorWins` failed index 3 immediately and index 1 after
80 ms, expecting index 1's error to win. Whether index 1 had entered `fn`
before index 3's failure triggered cancellation was left to the scheduler. If
it had not, index 1 never failed at all and index 3's error was correctly the
lowest — the test failed a correct implementation.

Measured on one agent implementation: **4 pass / 2 fail over 6 runs of the
same code.**

Fixed: index 3 now waits on a channel closed by index 1 before failing, with a
2 s guard so a sequential implementation fails rather than hangs. Re-measured:
**8 pass / 0 fail over 8 runs.** The reference/trap gate still discriminates.

This is exactly the defect class that would have destroyed the formal
experiment. A paired design cannot tolerate a case that flips on the same
code — the treatment arm would have shown a difference that was scheduler
noise.

**Consequence for the numbers above:** `go-maplimit-first-error`'s 2/3 (V2)
and 1/3 (V1) were both measured with the flaky test. Its true control rate is
unknown and must be re-measured with the fixed case before it counts.

## Selection outcome

| | |
| --- | --- |
| V2 candidates authored | 5 |
| Cleared the discriminance gate | 5 |
| Landed in the 30–70 % band | **0** |
| Rejected for ceiling | 5 |
| Held, pending re-measurement | 1 (`go-maplimit-first-error`) |

Cumulative across V1 + V2: **8 cases authored, 7 rejected for ceiling, 1
unmeasured.**

## Selected suite

**None.** There is no frozen Reviewer evaluation suite yet.

## Formal experiment readiness

**Not ready.** G4 is not cleared.

The scoring half remains ready and unchanged:

| Gate | State |
| --- | --- |
| G1 contribution observable on the reviewer path | ✅ |
| G2 unmeasured ≠ zero | ✅ |
| G3 independent `expect` reaches the run record | ✅ |
| G4 control arm has headroom | ❌ |

## Next

1. **Re-measure `go-maplimit-first-error` at n≥6** with the race fixed. It is
   the only candidate that has ever failed for a reason that was not the case's
   own defect.
2. **Test the two explanations against each other** before authoring another
   batch. Cheapest experiment: take one rejected case — `go-openall-unwind` is
   the clearest — and produce two variants:
   - *vague*: same hidden tests, doc comment reduced to the goal
   - *anti-idiom*: same explicitness, but the required behaviour deliberately
     differs from the conventional one (unwind in **forward** order, say,
     because a later resource depends on an earlier one being alive)

   Run both control-only at n=3. If only the anti-idiom variant drops, the
   binding constraint is idiom-matching, not explicitness. That is 6 runs to
   settle a question that would otherwise be guessed at across another five
   authored cases.
3. Only then author V3.

A third possibility this suite cannot rule out: **`deepseek-v4-flash` may have
no reliable blind spot at this task size.** Every case here is a single file
under 100 lines. If the model's failures live in tasks that span modules and
hundreds of lines, no amount of trap design at this scale will find headroom,
and the Reviewer experiment needs bigger tasks rather than cleverer small ones.
The `icg` family is the existing shape closest to that, and it is also at
ceiling — but `icg` scores with visible tests, so its ceiling is not evidence
about hidden defects.

## Artifacts

- `evals/reviewer/*.yaml` — six cases, all past the discriminance gate
- `evals/reviewer-rejected/` — V1 rejects, kept with the reason
- `eval/runs/MA-VALUE-REVIEWER-CALIBRATION-self-20260824T163706Z-967aed/`

## Related

- [MA-VALUE-REVIEWER-TASK-CALIBRATION](MA-VALUE-REVIEWER-TASK-CALIBRATION.md) — V1
- [MA-VALUE-REVIEWER-FORMAL](MA-VALUE-REVIEWER-FORMAL.md) — the protocol G4 gates
- [MA-VALUE-REVIEWER-FINAL](MA-VALUE-REVIEWER-FINAL.md) — why the pilot stopped
