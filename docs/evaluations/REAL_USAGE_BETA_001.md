# Real Usage · Beta Round 1

**Status:** planned, not started · **Subject:** `v0.2.0-beta.1` · **Opened:** 2026-08-22

The first real-usage round run against a **published Beta binary** rather than a
`main` checkout. Six tasks in six third-party repositories, chosen so that each
one stresses a different thing the Beta claims to do.

## Why this is not "Batch #1"

"Real Usage Batch #1" is taken: `R001–R010`, closed, and the evidence base that
[`BETA_CLOSURE_PROGRAM.md`](../BETA_CLOSURE_PROGRAM.md) derives every gate from
(ledger: `batch-01/BATCH_01_FINAL_FINDING_LEDGER.md` in the dogfood-control
repo). Batch #2 is closed too. Reusing those IDs would renumber settled history,
so this round gets its own prefix: **`BR-01` … `BR-06`**.

The distinction is not bookkeeping. Batches #1 and #2 measured a moving tree and
fed fixes back into it. This round measures a **frozen, published artifact** and
feeds findings into 1.0 planning. Different subject, different rules.

## Rules

1. **Run the published binary, not the working tree.** Install with
   `LEVELER_VERSION=v0.2.0-beta.1 sh -c "$(curl -fsSL …/install.sh)"` and record
   `leveler --version` — including the commit hash — in every task record. A
   binary built from a local checkout is not admissible: Batch #1's Gate 8 exists
   because a release binary silently carried another session's uncommitted work
   and only a hash comparison caught it.
2. **One repository per task, pinned by commit SHA.** Fork or clone at a fixed
   SHA so a finding can be reproduced.
3. **No harness assistance beyond the prompt.** Supervisor may prepare the
   *environment* (toolchain, deps) and must log that it did. Supervisor may not
   hint at tools, delegation, or structure.
4. **Pre-existing failures are baselined first.** Run the repo's own checks
   before the agent touches anything and record the result. A test that was
   already red is not a regression the agent caused — Batch #1 lost time to this
   twice.
5. **Test credentials only.** Never a real secret, even though Gate 1 shipped.

## Tasks

| ID | Repository | Type | What it is meant to stress |
| --- | --- | --- | --- |
| BR-01 | `rust-lang/cargo` | Large Rust repository | Exploration quality, context usage, verification quality, whether a child agent earns its cost |
| BR-02 | `casdoor/casdoor` | Full-stack (Go + React) | Frontend/backend understanding, cross-module reasoning, task planning |
| BR-03 | `hoppscotch/hoppscotch` | Monorepo | Repository navigation, dependency understanding, architecture discovery |
| BR-04 | `BurntSushi/ripgrep` | Correctness-focused | Bug discovery, regression prevention, reviewer behaviour |
| BR-05 | `go-task/task` | Concurrency / review | Subtle logic issues, verification under concurrency |
| BR-06 | *frontend, long-running* | Long integration | Browser capability, long goal, recovery, spawn |

BR-02 (casdoor) was also exercised in Batch #1 as R006 and passed with five
findings. It is repeated deliberately: the same repository against a frozen
binary is the closest thing this programme has to a controlled comparison.

BR-06 has no repository named yet. Pick one at execution time that has a real
dev server and a real build — Batch #1's two browser negatives both came from a
dev server that never booted in a large monorepo, which is an environment
problem masquerading as a capability problem.

## Record template

One record per task. Fields are fixed so records are comparable.

```
Task ID:
Repository:
Commit SHA:
Goal:                 (verbatim prompt given to the agent)
Model:
leveler --version:    (must include the commit hash)
Baseline check:       (repo's own checks before the agent ran)

Duration:
Turns:
Tool calls:
Files changed:
Tests run:
Verification result:
Spawn count:
Child utility:        (did the delegation produce work the parent used?)

Failures:             (what broke, with the observable symptom)
Human judgement:      (accept / reject, and why)
```

## What is measured

| Measure | Definition |
| --- | --- |
| **Task success** | The stated goal is met, judged against the repo's own checks — not against the agent's summary |
| **Verification success** | The agent's own verdict matches reality. A truthful "incomplete" scores better than a false "done" |
| **Useful delegation** | A child produced work the parent actually consumed. Zero delegations on a task that did not need one is a pass |
| **Context efficiency** | Tokens and tool calls spent per unit of real progress |
| **Recovery ability** | Behaviour after a failed edit, a failed check, a disconnect, or a budget exhaustion |
| **Failure modes** | What went wrong, classified — the round's most valuable output |

**Not measured: spawn rate.** It is not a goal and never has been. Delegation is
opportunity-based (10–30 % adoption on qualified tasks) and
[`ROADMAP.md`](../ROADMAP.md) standing constraint 2 forbids treating a higher
number as an improvement. An intervention that raises spawning while raising
*unnecessary* spawning is a regression.

## What this round should not re-discover

Batch #1 and #2 already settled these. If a record repeats one, it is a
regression report, not a finding:

- Discovery is not a bottleneck — first relevant hit landed at step 2–5 in every
  Large task.
- The 100-round ceiling is a throughput symptom, not a budget shortage. Do not
  propose raising it without new evidence.
- The browser bypass pattern (shell-driven Playwright/Puppeteer) is gone: ~160
  structured browser calls, zero workarounds.
- "Always review" is refuted. BR-04 and BR-05 are where a reviewer would be
  justified if anywhere; that is why they are in the list.

## Exit

The round closes when all six records exist and each is `accept` or `reject`
with a reason. Findings are classified, not ranked by appeal, and feed the
Capability Closure phase in [`ROADMAP.md`](../ROADMAP.md). Nothing in this
document authorises a fix; it authorises evidence.
