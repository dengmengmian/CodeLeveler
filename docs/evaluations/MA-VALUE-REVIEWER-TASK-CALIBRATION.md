# MA-VALUE-REVIEWER-TASK-CALIBRATION

**Status:** calibration complete · G4 NOT cleared · **Opened:** 2026-08-24
**Arm:** control only (`independent_review = "off"`) · **No Reviewer was run**

Finds coding tasks where an independent Reviewer has room to change the
outcome. This is not an experiment about Reviewer value and claims nothing
about it.

## Why

[MA-VALUE-REVIEWER-FINAL](MA-VALUE-REVIEWER-FINAL.md) halted the pilot with a
control arm at 5/5. Every recorded baseline for `deepseek/deepseek-v4-flash` is
at ceiling:

| Baseline | Result |
| --- | ---: |
| `icg-integrated` (icg-1…5) | 15/15 |
| `c4-self-healing` (r1…r8) | 24/24 |
| `c2-bv2-navigation` | 24/24 |
| `c3-edit-reliability` | 24/24 |

A saturated control makes the primary estimand — does a Reviewer raise final
correctness — unmeasurable in principle.

There is a second, less visible reason the old corpus cannot host this
experiment: **its `expect` is usually the test file the agent can read**.
`go-gitcmd-semaphore` scores with `go test ./...`, which runs the same
`runner_test.go` shipped into the workspace. An agent that passes the visible
test has, by construction, passed the score. There is no state in which the
implementation looks done and a reviewer still has something true to say.

Every case here separates the two:

```
visible test   →  the agent sees it, must pass it
hidden expect  →  written at verification time, covers the trap
```

## Selection rules

| Control pass rate | Action | Reason |
| --- | --- | --- |
| > 80 % | reject | no headroom; a Reviewer cannot improve a ceiling |
| 50–70 % | **keep** | the target band |
| 30–50 % | judgement call, record it | usable if the failure is the trap |
| < 30 % | reject | baseline too weak; measures difficulty, not review |

A case is **not** adjusted to fit the band after measurement. Tuning a task
until the numbers work is how a null result becomes a false positive.

## Discriminance gate — run before any model spend

Every case is checked against two hand-written implementations:

| Implementation | Visible | Hidden | Meaning if wrong |
| --- | --- | --- | --- |
| reference (correct) | pass | **pass** | a failing reference means the case is unsolvable, not hard |
| trap (plausible, wrong) | **pass** | **fail** | a passing trap means no headroom exists |

All three cases below cleared this gate before calibration ran. The check
lives in `scratchpad/validate_case.py` and is cheap — it costs no model calls
and it caught two defects in Task A before a single token was spent.

## Tasks

### Task A · `go-maplimit-first-error` — concurrency

| | |
| --- | --- |
| Repository | synthetic, `files:` inline |
| Starting revision | `MapLimit` is `panic("not implemented")` |
| User request | implement bounded parallel map per the doc contract |
| Verification | `go vet` · `go test ./...` · hidden `TestHidden*` |

**Contract.** At most `limit` concurrent calls; results in **input** order;
on failure return the error of the **lowest failing input index** and a nil
slice; a known failure cancels the context given to still-running calls.

**Hidden defect opportunity.** The natural implementation keeps a
`firstErr` guarded by a mutex and assigns whichever error *arrives first in
time*. Under the visible tests only one call ever fails, so time-first and
index-first agree and the trap passes. The hidden test fails index 3
immediately and index 1 after 80 ms: index-first must return index 1's error.

Two further traps in the same shape: returning the partially filled slice
instead of nil on error, and never cancelling siblings.

**Gold criteria.** Lowest failing index wins · nil slice on error · siblings
observe cancellation · `limit <= 0` means unlimited and order still holds.

**Failure patterns to expect.** time-first error; partial results returned;
no cancel; order lost when using a completion channel.

**Why a Reviewer may help.** All four defects are visible in the diff — a
mutex-guarded `if firstErr == nil`, a `return out, err`, an absent
`context.WithCancel`. None of them need a profiler or a repro.

### Task B · `go-safepath-escape` — security

| | |
| --- | --- |
| Repository | synthetic, `files:` inline |
| Starting revision | `Resolve` is `panic("not implemented")` |
| User request | confine a request-supplied path to a base directory |
| Verification | `go vet` · `go test ./...` · hidden `TestHidden*` |

**Contract.** Result is `base` or below it; anything landing outside returns
`ErrEscape`; an absolute `userPath` returns `ErrEscape`; `""` and `"."`
resolve to `base`.

**Hidden defect opportunity.** `strings.HasPrefix(joined, absBase)` without a
trailing separator. `Resolve("/root/data", "../data-evil/secret.txt")` cleans
to `/root/data-evil/secret.txt`, which has `/root/data` as a string prefix and
is accepted. The visible tests only try `../etc/passwd`, which the bare prefix
check does reject.

Second trap: forgetting that `filepath.Join(base, "/etc/passwd")` swallows the
leading slash, so an absolute input silently becomes a path inside base rather
than an error.

**Gold criteria.** Sibling-prefix directory rejected · absolute `userPath`
rejected · deep `../` chains rejected · interior `a/../b` allowed.

**Failure patterns to expect.** bare `HasPrefix`; no absolute-path rejection;
checking the raw `userPath` string for `".."` instead of the resolved result.

**Why a Reviewer may help.** The missing `+ string(filepath.Separator)` is one
token, visible in the diff, and is a well-known class a reviewer is primed to
look for in path-handling code.

### Task C · `go-config-legacy-compat` — refactoring / compatibility

| | |
| --- | --- |
| Repository | synthetic, `files:` inline |
| Starting revision | `Parse` is `panic("not implemented")` |
| User request | support the new `timeout` format without breaking `timeout_ms` |
| Verification | `go vet` · `go test ./...` · hidden `TestHidden*` |

**Contract.** `timeout` wins over `timeout_ms` when both are present; a
timeout absent from both defaults to 30 s; `retries` absent defaults to 3 and
**present-and-zero stays zero**; negatives are errors; unknown keys are
ignored.

**Hidden defect opportunity.** Unmarshalling into value types and defaulting
on the zero value:

```go
c.Retries = r.Retries
if c.Retries == 0 { c.Retries = 3 }
```

An operator who deliberately set `"retries": 0` gets 3 — retries they turned
off, silently back on. The visible tests use `retries: 7` and `{}`, where zero
and absent never have to be told apart.

Same defect on the timeout side: `timeout_ms: 0` is a chosen value, and
`if r.TimeoutMs != 0` treats it as absent.

**Gold criteria.** Explicit `retries: 0` preserved · explicit `timeout_ms: 0`
preserved · current format wins over legacy · negatives error · unknown keys
ignored · legacy-only document still defaults the other fields.

**Failure patterns to expect.** zero-value defaulting; legacy branch
overriding the current one; unknown keys treated as an error.

**Why a Reviewer may help.** Absent-versus-zero is a named, recognisable class,
and the fix — pointer fields — is small and entirely visible in the diff. It
is also the exact defect this project's own eval observer shipped with, which
is a fair sign of how easy it is to write.

## Calibration protocol

Control only. `independent_review = "off"` in an isolated `LEVELER_HOME`,
asserted before the run starts. n=3 per case.

```sh
python3 scratchpad/run_calibration.py 3
```

Model `deepseek/deepseek-v4-flash`, binary `target/release/leveler`, same as
the pilot. Collected per run: `expect_passed`, `completed`, `termination`,
`rounds`, `tool_calls`, `latency_ms`, tokens.

## Results

Control only, n=3, `deepseek/deepseek-v4-flash`, reviewer OFF.
Batch `MA-VALUE-REVIEWER-CALIBRATION-self-20260824T152145Z-809d6e`.

| Case | Category | Pass | Rate | Rounds | Out tokens | Latency | Verdict |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- |
| `go-safepath-escape` | security | 3/3 | 100 % | 6.0 | 1 356 | 21 s | **reject** |
| `go-config-legacy-compat` | compatibility | 3/3 | 100 % | 7.3 | 2 644 | 36 s | **reject** |
| `go-maplimit-first-error` | concurrency | 1/3 | 33 % | 6.7 | 7 699 | 116 s | **hold** |

### Rejected: `go-safepath-escape` (100 %)

Not a broken case — a real negative result. The trap discriminates: the
hand-written `strings.HasPrefix(joined, absBase)` implementation fails the
hidden tests. The model simply does not write it. Its rep-1 answer:

```go
if filepath.IsAbs(userPath) { return "", ErrEscape }
joined := filepath.Join(absBase, userPath)
if joined != absBase && !strings.HasPrefix(joined, absBase+string(filepath.Separator)) {
    return "", ErrEscape
}
```

Separator included, absolute input rejected. Path confinement is not a blind
spot for this model, so a Reviewer has nothing to add here.

### Rejected: `go-config-legacy-compat` (100 %)

Same shape. The zero-versus-absent trap discriminates against a value-typed
implementation, but the model reached for pointer fields unprompted in all
three runs. Not a blind spot either.

Worth noting because this project's own eval observer shipped with exactly
this defect: the model avoided a bug its maintainers wrote.

### Hold: `go-maplimit-first-error` (33 %)

Below the 50–70 % band, inside the 30–50 % judgement range. Two distinct,
reviewer-addressable failures:

| Rep | Result | Failure |
| --- | --- | --- |
| 1 | ✗ | visible tests **hang** — `panic: test timed out`, goroutines parked on a channel receive |
| 2 | ✓ | — |
| 3 | ✗ | hidden `TestHiddenFailureCancelsSiblings` — a failure never cancels its siblings |

Rep 3 is a clean trap hit: the implementation calls `context.WithCancel` but
never reaches `cancel()` on the failure path. One missing call, plainly visible
in the diff.

Rep 1 is more interesting than a wrong answer: the agent reported
`Completed`, and its code deadlocks the very test file it was told must pass.
Self-verification did not catch a hang that `go test` catches in 60 seconds.

**Determinism was checked, not assumed.** Both failing implementations were
re-run through the full `expect` five times: 0/5 pass, 0/5 pass. The case
scores the same way every time for a given implementation.

*(An earlier reading of this as a flaky deadlock was wrong — it came from
`go test` result caching in a manual re-run, not from the case.)*

## Selection outcome

| | |
| --- | --- |
| Candidates authored | 3 |
| Cleared the discriminance gate | 3 |
| Landed in the 50–70 % band | **0** |
| Held for more data | 1 (`go-maplimit-first-error`, 33 %) |
| Rejected for ceiling | 2 |

**G4 is not cleared. The formal experiment cannot start.**

## What the rejects taught

Two of three traps were real (they catch a plausible wrong implementation) and
neither caught the model. The pattern in what survived is worth stating:

- **A named, well-published defect class is not a blind spot.** Path traversal
  and absent-versus-zero are the two most-written-about bugs in their
  categories. The model has seen the fix more often than the bug.
- **What did survive is a defect of *coordination*, not of a single
  expression.** Cancelling siblings on failure is not a line you get wrong; it
  is a step you forget when the pieces are individually correct. Rep 1's
  deadlock is the same shape — every part looks reasonable, the assembly hangs.

That suggests the next candidates should pressure multi-step invariants that
hold across a function rather than single-expression traps.

## Formal experiment readiness

**Not ready.** Required before
[MA-VALUE-REVIEWER-FORMAL](MA-VALUE-REVIEWER-FORMAL.md) runs:

1. Raise `go-maplimit-first-error` to n≥6 control runs. At n=3 a 33 % estimate
   cannot be placed against a 50 % boundary.
2. Author 4–6 more candidates biased toward cross-step coordination:
   cancellation and cleanup ordering, lock ordering across two functions,
   retry state that must survive a partial failure, resource release on the
   error path.
3. Re-calibrate. Keep only what measures into the band.

The scoring half is ready: the independent `expect` reaches the run record,
findings lifecycle is observable on the reviewer path, and unmeasured is
distinguished from zero. Only the task set is missing.

## Scoring validation

| Check | State |
| --- | --- |
| Score distinguishes partial success | ✅ per-case `expect_passed`, not a batch aggregate |
| Hidden defect is real | ✅ trap implementation fails hidden tests in all 3 cases |
| Case is solvable | ✅ reference implementation passes all 3 |
| Deterministic per implementation | ✅ 5× re-run, no flips |
| Reviewer finding maps to the lifecycle | ✅ verified end-to-end, `contribution_unmeasured = 0/5` |
| No automatic scorer mistakes | ⚠️ one fixed — see below |

**Fixed during this phase.** The `expect` scripts sent every command to
`/dev/null`, so a failing verification produced exit code 1 and no
explanation. Diagnosing the first failure required re-running the whole script
by hand. Output is no longer suppressed.

## Related

- [MA-VALUE-REVIEWER-FINAL](MA-VALUE-REVIEWER-FINAL.md)
- [MA-VALUE-REVIEWER-TASKS](MA-VALUE-REVIEWER-TASKS.md)
- [MA-VALUE-REVIEWER-FORMAL](MA-VALUE-REVIEWER-FORMAL.md)
