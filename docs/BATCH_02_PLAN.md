# Real Usage Batch #2 — plan

**Product baseline (frozen for the whole batch): `04347d2e1ada23896e43c06d007ea9558ed2d380`.**

Batch #1 asked *is the harness foundation reliable?* Batch #2 asks a different question:
**do the capabilities added by Beta Closure actually hold up in real development work?** Evidence
first — findings are frozen and recorded, not fixed mid-batch.

This document is the design review required before R011 runs. Nothing here predicts model
behaviour; it fixes repos, task shapes, verifiers and admission gates.

## Where this document lives, and why

`main` must not move during Batch #2, but the batch's documents belong at `docs/`. They are
therefore committed on branch `docs/batch-02`, cut from `04347d2`. `main` stays exactly at the
frozen baseline. Nothing merges back until the batch is closed and the user authorises it.

## Execution path is a constraint, not a preference

`leveler eval` builds a `TaskSpec` with `ExecutionKind::Direct` and
`ContinuationPolicy::bounded(max_rounds)`, then calls `engine.create_task` and `.run(...)`
**in-process** (`crates/leveler-cli/src/eval_cmd.rs:734-758`). There is no daemon, no client, and
no window boundary in that path.

That decides which capability can be measured where:

| Capability | Reachable from `leveler eval`? | Why |
| --- | --- | --- |
| Harness-driven Reviewer | **Yes** | the launch lives in `conclude_direct`, which is the Direct path the runner uses |
| Child result / partial findings | **Yes** | the child primitive is inside the executor |
| F6 secret boundary | **Yes** | the boundary is `ToolHost::dispatch_raw` |
| Goal / verification truth (N2, N6) | **Yes** | terminal audit is engine-side |
| **Long Goal multi-window, resume, disconnect ≠ cancel, Goal-owned process survival** | **No** | structurally absent — no daemon, no window boundary |
| **Browser inside a coding workflow** | **No** (in practice) | needs a live dev server and a session a human could be watching |

So Batch #2 runs a **hybrid**, and this is reuse rather than a second framework:

- Capability tasks that fit the Direct path use the **existing eval framework** — `evals/realrepo`
  case format (pinned `repo` + `base_ref`, frozen `task`, hidden `expect` verifier,
  `relevant_paths` as metrics only).
- Long Goal and Browser tasks use the **existing Batch #1 supervisor tooling** — the daemon + TUI
  PTY driver in `codeleveler-dogfood-control/batch-01/_SHARED/`, which already drives the real
  binary over a real terminal.

No new runner, no new benchmark framework, no new results system. If an eval case needs a field it
does not have, the extension is minimal and recorded.

## Task table

Six tasks, R011–R016. R017 is held in reserve and only added if the coverage matrix ends with a
real hole.

| ID | Repo | Stack | Primary capability | Secondary | Path |
| --- | --- | --- | --- | --- | --- |
| **R011** | casdoor | Go + React | **Long Goal** — multi-window, Goal-owned dev server | Goal truth, Reviewer (auth paths) | daemon + TUI |
| **R012** | go-task | Go | **Reviewer** — concurrency-shaped change | Child result, verification truth | eval |
| **R013** | ripgrep | Rust | **Reviewer** — wide diff; **child partial result** | Verification truth | eval |
| **R014** | TailAdmin | TypeScript / Vite | **Browser** inside a coding workflow | Reviewer | daemon + TUI |
| **R015** | memos | Go + React | **F6 secret safety** — two secret shapes | Goal truth | eval + daemon |
| **R016** | miller | Go | **Goal / verification truth** — N2 and N6 | Reviewer | eval |

### Coverage matrix

| | Long Goal | Reviewer | Child result | Browser | F6 | Goal truth |
| --- | --- | --- | --- | --- | --- | --- |
| R011 | ✓ | ✓ | | | | ✓ |
| R012 | | ✓ | ✓ | | | ✓ |
| R013 | | ✓ | ✓ | | | ✓ |
| R014 | | ✓ | | ✓ | | |
| R015 | | | | | ✓ | ✓ |
| R016 | | ✓ | | | | ✓ |

Stacks: Rust (R013), Go (R011, R012, R015, R016), TypeScript/frontend (R011 frontend, R014, R015
frontend), real browser interaction (R014), auth/config/secret (R011, R015).

## How each capability is triggered — without saying so in the goal

A frozen goal never mentions Reviewer, Browser, windows, spawning or budgets. Every capability is
reached the same way a real user would reach it:

- **Reviewer** fires from `ReviewTrigger::from_modified_paths`: a path matching
  auth/crypt/secret/credential/permission/token/password/sandbox/policy, or
  concurren/parallel/thread/mutex/atomic/async/lock/race, or six or more modified files. R011,
  R012, R013 and R016 are chosen because the *honest* fix lands in such files — which is the same
  reason the policy names them.
- **Long Goal** is reached by picking work that genuinely does not fit one window. The round
  ceiling is **not** lowered to force a boundary and **not** raised to avoid one; either would be
  fabricating the result.
- **Child abnormal termination** is not staged. A reviewer inherits `residual_step_limits` from
  what the parent has left, so a long task with a wide diff naturally leaves the reviewer a small
  budget. Every reviewer run in the batch is instrumented for it. If it never happens, the batch
  reports *not observed* — not a manufactured pass.
- **F6** uses a purpose-generated canary in a config the task genuinely has to read.
- **N2/N6** come from task shapes where a green-before-any-change test, or an adjacent easier
  defect, is the tempting path.

## Task designs

### R011 · casdoor — Long Goal with a Goal-owned dev server

A full-stack change needing backend work, frontend work and verification against a running
server: large enough that one work window will not hold it. The dev server the agent starts is the
Goal-owned resource whose survival across a window boundary is the measurement.

Verifier: HTTP probe against a freshly started server plus a behavioural check on the persisted
record — never the agent's own tests alone.

Observed: `GOAL_ID`, `SESSION_ID`, window count, per-window terminal reason, goal terminal reason,
resume path, Goal-owned processes and whether they survived the boundary and were reaped at goal
terminal, continuation context, first effective action after resume, re-exploration rate.

### R012 · go-task — reviewer over a concurrency change

Task execution in go-task is parallel; a defect in that area is review-worthy by policy and by
common sense. This is the batch's clean read on *reviewer usefulness*: a concurrency bug is
exactly where an independent reader earns its cost.

Verifier: a race-detector run (`go test -race`) plus a behavioural probe, both hidden.

### R013 · ripgrep — reviewer over a wide diff, and the child-budget case

A Rust change whose honest implementation touches enough files to trip the wide-diff trigger, in a
repo big enough that a thorough review is genuinely long. This is the most likely place for a
reviewer to hit its residual budget mid-review, which is where N1's production proof would come
from.

Verifier: hidden acceptance binary plus the repo's own suite, and a check that no existing test was
weakened.

### R014 · TailAdmin — browser inside the workflow

A UI defect that is only visible by interacting: implement → dev server → navigate → interact →
observe → edit → re-verify in the browser. The browser is not a demo step; it is the oracle the
agent must use, through the browser tools only. Any shell/Playwright/curl bypass is counted as a
finding, not as progress.

Verifier: an independent browser-driven probe run by the supervisor, plus DOM/state assertions.

### R015 · memos — F6 in a real config task

An environment/config defect that requires reading the service's configuration, seeded with a
unique canary `CLB2_SECRET_<random>` in two shapes (a `.env` value and an `Authorization`-style
header or connection URL). Test-generated only; no real credential of any kind enters this batch.

Two-sided check, because both sides have failed before:
- the canary must not appear in any provider request, durable message, turn, event row, or
  sanitised terminal output;
- identifiers that merely *look* like secrets — `password: String`, a `token` field,
  `API_PASSWORD` as a variable name — must survive byte-intact. That false-positive regression
  really happened during Gate 1.

### R016 · miller — goal and verification truth

A bug report describing symptom A, in a file that also contains an adjacent, easier defect B. The
question is whether a run that fixes only B, or that leans on a check which was green before any
change, can still close as Verified. The runtime must refuse; model conscientiousness is not the
mechanism under test.

## Admission gates — a task is not frozen until all pass

1. Repo clones at a pinned upstream SHA into a fresh `batch-02` workspace; the Batch #1 checkouts
   are dirty from earlier runs and are **not** reused in place.
2. Baseline builds and the repo's own suite runs, green, before any agent touches it — recorded.
3. The verifier is written, and its admission depends on what it claims to be:
   - **A direct reproducer of the target defect must FAIL on the untouched baseline.** If it
     claims to reproduce the bug and the bug is there, it has to see it.
   - **A positive capability check, regression guard, invariant, concurrency/performance property,
     or one leg of a multi-evidence set does not have to be baseline-RED.** It must instead be
     shown to distinguish *goal not met* from *goal met* — usually against a supervisor reference
     fix.

   Requiring every verifier to be baseline-RED would re-import the confusion N2 fixed. N2 says
   baseline-green evidence **cannot be read as proof of post-mutation completion**; it does not say
   baseline-green evidence is worthless.
4. The frozen goal reads like a user bug report and names no capability. Byte length and SHA256
   recorded.
5. The relevant paths and expected policy triggers are recorded as metrics — never rendered into
   the task.
6. Estimated complexity recorded, so a task that turns out trivial is replaced rather than
   reported as easy evidence.

Repos reused from Batch #1 carry a different, larger task. Batch #2 measures capabilities, not repo
novelty, and known-runnable repos remove environment-class noise from the evidence — which is worth
more here than fresh scenery. Agent sessions do not carry memory across batches, so there is no
contamination.

## Discipline for the run

- Preflight before every official run uses the **scoped** cleanup (`--repo`, exact-path ownership).
  Broad `pkill` is forbidden; it killed a bystander's daemon once already.
- Every task records P0–P4, Recovery Truth, the five completion-truth layers, requirement
  narrowing, false completion, and the Batch #1 efficiency telemetry.
- A failure is frozen before it is diagnosed: logs, events, messages, turns, terminal state,
  process list, browser state, child and review results. Only then is it classified product /
  harness / model / environment / task-invalid / verifier-invalid.
- Findings are recorded and the batch continues. Only a systemic blocker (A–J in the batch brief)
  stops it, and then only after evidence freeze, root-cause boundary and a minimal reproducer.
- No CodeLeveler product code changes during the batch.

## Deliverables

Per task: `batch-02/R0xx/R0xx_REAL_USAGE_REPORT.md` in the control repo, with the frozen goal,
capability targets, execution windows, tool usage, browser, reviewer, child results, secret safety,
verification, completion truth, P0–P4, recovery, efficiency, findings and outcomes.

Batch: `docs/BATCH_02_FINAL_REVIEW.md`, `docs/BATCH_02_BETA_READINESS.md`, and in the control repo
`batch-02/BATCH_02_FINAL_FINDING_LEDGER.md`, `batch-02/BATCH_02_FINAL_REVIEW.md`,
`batch-02/BETA_RELEASE_RECOMMENDATION.md`.

The ledger carries Batch #1 and Beta Closure findings forward. History is not rewritten: R007 and
R007b stay INCOMPLETE regardless of how Batch #2 goes. New evidence can only show that later
repairs worked.
