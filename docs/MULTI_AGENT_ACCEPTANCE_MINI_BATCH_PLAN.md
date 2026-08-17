# Multi-agent Acceptance Mini-Batch plan

Do not run these tasks as part of product closure. They are the next human
step after merge.

Do not prompt “please spawn explorer/worker”. Use real work and let Main
decide.

## A. Explorer — unknown impact surface

Pick a mid-size unfamiliar crate in this workspace (for example
`leveler-execution` command classification vs `shell_ast`). Ask for the
real blast radius of changing one public entry. Success: Main spawns an
Explorer, adopts typed `relevant_file` / `callsite` findings, and does not
treat investigation as the work being done.

## B. Worker — naturally split work

A mechanical, disjoint edit: add the same `Debug` bound (or the same log
line) in two independent modules with no shared file. Success: two Workers
with disjoint `files`, both complete, no overlap refusal, parent synthesizes.

A negative probe (same file named by two Workers) must be refused honestly.

## C. Reviewer — high-risk change

A small auth/permission/path change that the harness already classifies as
review-worthy. Success: harness Reviewer runs, a blocking correctness finding
refuses `Verified` until Main rejects with a reason or addresses + fresh
verify.

## D. Combined

One real feature slice that is unfamiliar, splitable, and review-worthy
(for example a bounded CLI flag plus its tests). Success: Explorer findings
inform the Worker scopes, Reviewer findings enter the same ledger, and
closure is truthful if any required finding is still open.

## Metrics to record (not spawn count)

- Explorer findings accepted vs ignored
- Worker scoped work that landed vs conflict/refusal
- Reviewer findings accepted / rejected / verified
- Parent rereads of child-already-read files
- False `Verified` attempts (should be zero)
