# CodeLeveler · Beta Closure Program

Derived from Real Usage Batch #1 (R001–R010). This is the execution route to `1.0.0-beta.1`. Every gate below exists because a specific finding demands it; nothing here is speculative architecture.

Starting baseline: `c3bf11ba01c3d5e5ef66244dcc3a6ae787036268`. Finding IDs refer to `batch-01/BATCH_01_FINAL_FINDING_LEDGER.md` in the dogfood-control repo.

## Beta definition (not negotiable)

A. Reliable runtime · B. Single-agent real-repo quality · C. Long-running goal closure · D. Browser coding workflow · E. Spawn/sub-agent reliability · F. Real multi-agent product semantics · G. Truthful completion & recovery · H. Release quality. **F6 security must be closed before Beta.**

The batch does not lower these. It changes their *order and size*, because it showed which of them are nearly done and which were never really started.

---

## Gate 1 · Secret Propagation Safety (F6)

**Purpose.** A credential the agent reads must not reach the model, the provider, or durable storage in plaintext.

**Findings covered:** R007-F6 (`OPEN_BETA_BLOCKER`, the only P1).

**Why first.** It is the only security item, it is the only Beta blocker, and its Phase 0 audit is already paid for. It is also the only open finding whose risk grows with usage: every session that reads a `.env` is exposed, and the batch contained it only by a hand-enforced test-credential rule.

**Scope.** Sanitize at the single model-visible boundary, `ToolHost::dispatch_raw`, which every tool result passes through (read_file, shell, browser, MCP, web, git). Build the *value-position-aware detector* this requires. Keep F2's persistence redaction as defence in depth.

**Explicit non-scope.** No taint engine, no DLP platform, no secrets manager, no `SecretRef` execution capability, no EventLog changes.

**Hard precondition, already established by evidence.** The existing redactor **cannot be relocated**. Measured at `c3bf11b`, it rewrites `let password = config.password;` → `let password = [REDACTED];` (destroys code the agent must read) while missing `API_PASSWORD=…`, `export TOKEN=…`, and `postgres://admin:pw@host` — the commonest real shapes. Any plan that begins "move `redact_secrets` earlier" is already refuted.

**Acceptance.** Model-visible tool output carries no secret **value** while keeping keys and structure (`API_PASSWORD=[REDACTED]`, not a blanked file). Ordinary source code is untouched. Provider request payloads carry no test secret. Durable planes carry none. Agent can still reason about configuration structure. Reverse-verified accident tests. Daemon smoke on credentials-read, shell-output, and disconnect/resume.

**Exit.** Durable + provider + model-visible occurrences of the test secret = 0, with no regression in the agent's ability to work on config-bearing repos.

---

## Gate 2 · Verification & Closure Truth

**Purpose.** Stop two ways the harness can tell the user something untrue: a correct fix recorded as a code failure, and a success claim for work that was not the goal.

**Findings covered:** R003-F1 (env-vs-code failure, residual scope), R005-F-P2 (toolchain policy), N2 (green-on-baseline repro), N6 + R003-F2 (closeout prose), R010's honest `verified` terminal as the positive reference.

**Why second.** Three of these are **cross-task confirmed** (R003-F2 + N6; R003-F1 + R005-F-P1 + R009's toolchain wall), which is a higher evidence bar than anything in the spawn line. And they are the finding class that most directly damages user trust: the product either blames the code for the environment, or reports success on the wrong deliverable.

**Scope.**
1. Widen environment-failure classification to the shapes actually observed — disk exhaustion, missing global tooling, and a toolchain the repo requires but the environment lacks. Today `failure.rs` catches MSRV refusal and three generic patterns; it does **not** catch R003's actual trigger.
2. A toolchain-availability policy: when a repo pins a newer toolchain than the environment provides, that is an environment fact to surface, not a code failure to repair (R005-F-P2; R009 needed supervisor prep to run one `go` command).
3. Reproduction validity: on a bug-fix goal, a newly added reproduction that passes on unmodified code has demonstrated nothing. Mark it — `REPRO_NOT_PROVEN` / `BASELINE_GREEN` — as verification-evidence semantics.
4. Goal closure: the final claim must map onto the original requirements, and unmet requirements must be named. Reuse the existing Engine / EventLog / turn lifecycle — **no second task state machine.**

**Explicit non-scope.** No new verifier framework, no requirement DSL, no second state machine.

**Acceptance.** A correct fix on a full disk classifies as environment, not code failure. A first-run-green repro is flagged. A closeout that leaves requirements unmet says so. R010's `verified` path still verifies.

---

## Gate 3 · Long-running Goal Closure

**Purpose.** Make a goal that outlives one window a first-class, resumable, resource-correct thing.

**Findings covered:** R007-F3 (ceiling × reap ⇒ environment rebuild), R6-P5 (resume discoverability), 100-round work-window economics, interrupted-turn closure.

**Why third.** Beta requires it (definition C) and nothing else can substitute. The dependency runs the other way from what was assumed: this gate needs Gate 2's closure semantics to know what "the goal is done" means.

**Scope.**
1. Resource lifetime between turn-owned and daemon-owned. R6-P4's reap is **correct** and must not be weakened; what is missing is a *goal-owned* service class so a dev server survives a window boundary within the same goal. Reuse the existing Runtime ownership model — **do not build a scheduler.**
2. Resume affordance. Every resume in this batch used `--session <uuid>` from supervisor tooling; a real user has no such handle (R6-P5).
3. Work-window economics, informed by evidence rather than by raising a number — see the diagnosis below.

**Explicit non-scope.** No second scheduler, no second runtime, no EventLog v2.

**100-round ceiling — evidence-based diagnosis: (C) + (D), not (A).** R009 finished a Long task in 63 rounds and R010 in one short window, so the ceiling is not simply too small. R008 burned a full window mid-task and R007 burned two — but R007's rounds were polluted by guard kills and provider failures now fixed. The ceiling behaves as a symptom of low per-round throughput (§ 1-tool-per-step) and of a missing work-window abstraction. **Do not change 100 → 200.** Fix the abstraction; re-measure.

**Acceptance.** A goal spanning multiple windows keeps its services, is resumable by a user without a UUID, and closes truthfully. Ownership-aware P4 still passes at every terminal.

---

## Gate 4 · Reviewer Mechanism Decision (N7)

**Purpose.** Resolve the contradiction that currently makes the whole multi-agent line unmeasurable.

**Findings covered:** N7 (`OPEN_BETA_REQUIRED`), and it unblocks N1 and N4.

**Decision: OPTION B — build a real reviewer mechanism.** Rationale: Beta definition F requires Explorer/Worker/Reviewer product semantics, so retiring the label (Option A) would lower the Beta bar, which is not permitted. But the current state is worse than either option — a designation the system has never heard of. R008 and R009 were both marked REQUIRED_REVIEWER, both ignored it, and both passed, which proves the label is inert.

**Scope.** A reviewer must be a *harness lifecycle stage driven by policy*, not a sentence in the goal text. The batch also says clearly **when** it is justified: R008 and R009 succeeded without one, so "always review" is refuted. Candidate triggers to design against — wide diff, security-relevant change, concurrency change, uncertain verification, explicit user policy.

**Explicit non-scope.** No `SubAgentEngine`, no child scheduler, no prompt-injected "please spawn a reviewer". No mandatory reviewer on every task.

**Acceptance.** A reviewer runs when policy says so, without the goal text mentioning it; its findings are attributable; the tasks that do not need one are unaffected.

---

## Gate 5 · Spawn Reliability & Child Result (N1, N4)

**Purpose.** Make delegated work survive the child that produced it.

**Findings covered:** N1, N4 — both `OPEN_EVIDENCE_NEEDED`, both blocked behind Gate 4.

**Why here, and not earlier.** The direction is already right: *child work must survive child termination*. What is missing is evidence, and the batch could not supply it — of seven Large/Long tasks only R007b delegated at all, and the three that ignored delegation entirely all passed. Building `Structured Child Result` now would be the most architecturally attractive and least evidenced decision available.

**Scope after Gate 4 produces reviewer traffic.** Measure whether abnormal child termination still yields zero findings, and whether parents consume rather than repeat. Then design the result contract against real traffic.

**Explicit non-scope.** Do not implement the full `ChildResult` shape yet.

---

## Gate 6 · Browser Capability Closure

**Purpose.** Close remaining gaps in a capability that is otherwise done.

**Findings covered:** R004-F6 (ws through the proxy, `OPEN_EVIDENCE_NEEDED`), plus a gap audit.

**Verdict: CLOSURE_ONLY. Do not rewrite.** Across the batch there were ~160 structured browser calls and **zero** shell/Playwright/Puppeteer workarounds; the R004-era bypass pattern is gone. R010 drove 47 calls including `browser_console` to assert no page exceptions, and its work verified cleanly. The two negative cases (R007, R007b) share one cause — a dev server that never booted in a large monorepo — which is a Gate 3 problem, not a Browser defect.

**Scope.** Audit-driven only: re-prove the websocket path (never re-observed since R004), and list genuine gaps *if evidence supports them*. Do not invent a capability list.

---

## Gate 7 · Release Build Provenance

**Purpose.** Make it impossible to ship or dogfood a binary that is not the SHA it claims to be.

**Findings covered:** the provenance incident during the R007 merge — a release binary built from the primary checkout silently included another session's uncommitted work; the two binaries' hashes differed. Caught by inspection, not by tooling.

**Scope.** Formalise the practice this batch converged on: clean detached worktree → isolated target dir → install → codesign, with `git status --porcelain` empty and HEAD asserted. Small, mechanical, and it removes a class of "the evidence was measured against the wrong binary".

---

## Gate 8 · Beta Release Gate

Full P0–P4, recovery truth, security scan, provenance check, and a Batch #2 decision (below).

---

## Order and dependencies

```
Gate 1  Secret Propagation (F6)            ← only Beta blocker; independent
   │
Gate 2  Verification & Closure Truth       ← cross-task confirmed; defines "done"
   │
   ├──────────────► Gate 3  Long Goal Closure      (needs Gate 2's closure semantics)
   │
   └──────────────► Gate 4  Reviewer Mechanism (N7)
                        │
                        ▼
                    Gate 5  Spawn Reliability / Child Result (N1, N4)
                        │
Gate 6  Browser Closure (independent, small)
Gate 7  Build Provenance (independent, small)
                        │
                        ▼
                    Gate 8  Beta Release Gate
```

Gates 6 and 7 are small and independent — run them alongside whatever is in flight.

## Batch #2 · REQUIRED before Beta, but not yet

Not "run a few more tasks". Batch #1 cannot answer three questions **by construction**, and Beta depends on all three:

1. **Reviewer / multi-agent semantics** — zero organic delegation in six of seven tasks; N1 and N4 rest on a single child.
2. **Long-running goal** — no task in Batch #1 both hit a ceiling *and* required a long-lived service after Gate 3 exists.
3. **Security in practice** — F6 was contained by forbidding real credentials; after Gate 1 it must be exercised, not avoided.

Do not select repos or write task cards now. Batch #2 is defined **after** Gates 1–5 land, so its tasks can target the mechanisms those gates create.

## Do not build yet

| Item | Why the evidence does not support it |
| --- | --- |
| **Structured Child Result (full shape)** | One child, one task. Three later tasks proved the model does not delegate at all. Blocked behind Gate 4. |
| **Explorer / semantic-search architecture** | Discovery was never a bottleneck: first relevant hit at step 2–5 in every Large task, and R010 passed after only 27 read+search calls. Search is the least-supported optimisation in the batch. |
| **Browser V2 / foundation rewrite** | Zero workarounds in ~160 structured calls; both failures were unbootable apps. |
| **Full taint / DLP engine for F6** | One choke point (`dispatch_raw`) closes the observed accident. Taint would be a large subsystem for a problem a boundary already solves. |
| **EventLog v2 (ignorable envelope, `sourceEventSeqs`, synthetic interrupted turn)** | F2 closed the corruption class; nothing in R007b–R010 needed schema evolution. Revisit only if Gate 3 demands it. |
| **Raising the 100-round ceiling** | Diagnosed as a throughput/abstraction symptom, not a budget shortage. R009 and R010 finished well inside it. |
| **`SubAgentEngine` / child scheduler / second runtime** | No evidence any of it is needed; future multi-agent must reuse Engine, EventLog, ToolHost, and Runtime ownership. |
| **Prompt changes to force delegation or steer behaviour** | Would destroy the natural-behaviour signal the batch exists to produce. |
