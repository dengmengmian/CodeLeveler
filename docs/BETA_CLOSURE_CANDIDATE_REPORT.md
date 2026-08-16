# Beta Closure Candidate — `90d06db`

Branch `beta-closure/unattended`. Not merged, not tagged, not released.

**Verdict: READY pending one external commit.** Every Beta blocker and every Beta-required finding
from Real Usage Batch #1 is closed except R007-F4, whose fix already exists — written by a
concurrent session and sitting uncommitted in its working tree.

## What this train changed

| Area | Change | Finding |
| --- | --- | --- |
| Secret boundary | Value-position-aware detection at `ToolHost::dispatch_raw`, session-scoped registry at the durable JSON boundary | R007-F6 |
| Verification truth | Environment-vs-code classification widened; baseline-green-only evidence can no longer read as verified | R003-F1, N2 |
| Long goal | Window-boundary reap no longer kills a continuing goal; a resumable session is named on start | F3, R6-P5 |
| **Reviewer** | The harness launches the independent review its own policy asks for | **N7** |
| **Child result** | Children return `ChildResult { status, findings, stop_reason, partial }`, and an interrupted child keeps what it learned | **N1**, N4 |
| **Toolchain** | A repo-declared toolchain the environment lacks is an environment fact, not a code failure | **R005-F-P2** |
| Build provenance | `--version` reports the commit; a dirty build labels itself UNTRUSTED | Gate 7 |

Per-gate detail: `docs/GATE_4_CHILD_EXECUTION_AUDIT.md`,
`docs/GATE_4_REVIEWER_MECHANISM_CLOSEOUT.md`, `docs/GATE_5_CHILD_RESULT_CLOSEOUT.md`,
`docs/F6_IMPLEMENTATION_AUDIT.md`.

## Validation

`cargo test --workspace --all-targets` 2809 passed / 0 failed · clippy clean · fmt clean ·
F6 security smokes 12/12 on this binary · reviewer launch smoke 5/5 against a live model.

One browser test failed once under full parallelism and passed 3/3 in isolation;
`leveler-browser` depends only on `leveler-core`, which this train does not touch.

## Known limits

- The reviewer runs on Direct tasks only, and proves that a review *happened* — its findings are
  persisted and shown, but nothing parses them.
- N4 (does a parent act on `INCOMPLETE_NO_RESULT`?) is behavioural and unproven.
- Build artefacts from the agent's own commands enter `modified_files`, so they reach the review
  brief and the review trigger's file count (N8, LOW).
- No multi-task evidence: one live task exercised the reviewer.
