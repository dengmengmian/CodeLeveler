# Gate 2 · Verification & Goal Closure Truth — Closeout

**Result: PASS.** Branch `beta-closure/unattended`, on top of Gate 1.

Two ways the harness could tell the user something untrue are now closed: blaming the code for the environment, and calling a task verified when nothing was proven.

## R003-F1 · environment failure recorded as a code failure → VERIFIED_FIXED

R003's hugo run had a correct fix recorded `failed` because the repo-wide suite hit `no space left on device` and wanted a global npm package. The R005 F-P1 classifier covered toolchain/MSRV refusal and three generic patterns — **not R003's actual trigger**, which the Final Review flagged and this gate confirmed by reading the classifier.

`failure.rs` now classifies as `EnvironmentFailure` (never `retryable`, always `StopAndReport`): disk exhaustion (`no space left on device`, `os error 28`), process/fd exhaustion (`resource temporarily unavailable`, `too many open files`, `os error 24`), memory exhaustion (`out of memory`, `cannot allocate memory`, `Killed: 9`), and absent global tooling (`executable not found`, `externally-managed-environment`, `npm ERR! code ENOENT`).

Pinned by `resource_and_missing_global_tooling_are_environment_failures`, which uses R003's literal output. Existing negative tests still hold: ordinary compile and test failures are untouched.

## N2 · a reproduction that passes on the untouched tree → VERIFIED_FIXED

R007b's agent added a reproduction, watched it go green on a knowingly broken baseline, concluded the defect did not exist, and drifted to an unrelated fix. Nothing asked the obvious question: *your check passed before you changed anything — what did it prove?*

The evidence to answer it already existed. `VerifyRecord.after_mutation_seq` records the mutation sequence observed when a check ran, so `after_mutation_seq == 0` means "this passed before this task changed anything". Two predicates make that legible:

- `baseline_green_verifications()` — the passing checks that ran on an unmodified tree
- `only_baseline_green_evidence()` — every successful check so far was one of those

The terminal audit then refuses to call such a task `Verified`. It reuses the exact seam R004-F4 introduced for open todos, and applies only when the goal was expected to mutate:

```rust
if task_outcome == Verified && expected.has_mutation && ledger.only_baseline_green_evidence() {
    task_outcome = CompletedUnverified;
}
```

**This is evidence semantics, not policy.** Nothing is blocked, no test is forced to fail first, no prompt tells the model anything. A red reproduction on the baseline — the correct shape — is explicitly not flagged (`a_red_reproduction_on_the_baseline_is_not_flagged`).

## N6 / R003-F2 · closeout prose contradicting the record → runtime truth reinforced

Both occurrences share a shape: the **runtime label was honest** and the model's narrative was not. R003 said "tests pass" against `verification_finished passed=false`; R007b said "全部通过了" for an incidental fix while the original requirement was untouched.

The harness cannot stop a model from writing an over-claiming sentence, and forcing it via prompt is out of scope by directive. What it can do is ensure the authoritative terminal never agrees with the wrong claim. After this gate, a task that was expected to change code and has only baseline-green evidence cannot terminate as `Verified` — the two conditions present in R007b's substitution. Combined with R004-F4's open-todo rule, the runtime now downgrades on both "the plan is not done" and "nothing was proven".

Residual, recorded honestly: a full requirement-by-requirement ledger (goal → obligations → evidence → unmet items) is **not** built here. It needs requirement extraction, which is a design task, not a narrow repair. What this gate guarantees is that the *runtime terminal* stays truthful; the model's prose can still overstate, and the UI shows `completed_unverified` rather than a verified success.

## Verification

- Targeted: `leveler-verifier` 83/83, `leveler-lifecycle` 49/49 (2 new accident-shaped tests).
- Workspace: **2778 passed / 0 failed** (98 binaries) · clippy `-D warnings` clean · fmt clean.
- Reverse validation: disabling the baseline-green filter turns `a_verification_that_passes_before_any_change_is_not_proof` RED; restored GREEN.

## Non-scope

No new verifier framework, no requirement DSL, no second task state machine, no prompt changes, no per-repo special cases. R005-F-P2 (toolchain provenance policy) belongs to Gate 7 and is not addressed here.
