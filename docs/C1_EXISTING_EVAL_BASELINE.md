# C1.1a Full Existing Real-model Eval Baseline

> Branch `feat/coding-real-task-completion-c1` · base `main @ 1468b46`
> Model: deepseek/deepseek-v4-flash · Platform: macOS (Darwin 25.5) · 2026-08-07
> ZERO product behavior change. Failures are data.

## 1. Inventory / Runnable Scope

38 runnable automated cases (verified by file listing, not docs):

| Suite | Cases | Notes |
| --- | --- | --- |
| smoke | 3 | go/rust micro tasks |
| core | 21 | single-file bug-fix/feature, hidden acceptance tests (go 5 / rust 5 / ts 11) |
| hard | 5 | concurrency/trait/path-boundary |
| regression | 5 | 4 re-pinned core cases + reg-recovery-compile-fail (injected) |
| scenarios/debugging | 2 | recovery-compile-fail, recovery-test-fail (injected failures) |
| scenarios/permission | 1 | secrets-canary least-privilege |
| scenarios/feature | 1 | ripgrep-total-count: REAL repo (~40k LOC), long context |
| excluded | — | scenarios/tui (README only, manual); fixtures/repos/* without case yamls |

Corrections vs audit: regression = 5 (not 6; README miscounted); scenarios
contribute 4 automated cases previously not counted.

## 2. Deterministic Regression

fmt PASS · check 0 errors · workspace tests: first run 121 suites / 2457 / 1
failed suite (build-lock contention with the concurrently compiling eval
binary); clean rerun **122 suites / 2480 passed / 0 failed** — environment
flake, recorded, not product. eval_smoke_offline green within the suite.

## 3. Full Real-model Results (first-run, kept)

All PASS at steps 4-8, tokens 55k-123k input, termination=completed,
verified via hidden acceptance, false_completion 0, repair_count 0 (engine
RepairStarted never fired), user_intervention 0, runtime_error 0 — except:

| Suite | Case | Result | Detail |
| --- | --- | --- | --- |
| smoke | go-triple / rust-first-even / rust-mul | PASS | 4-6 steps |
| core | all 21 | PASS | avg 5.8 steps, 7.6 tools; heaviest ts-cache-ttl 8 steps/122.5k tok, ts-result-type 8/116.6k |
| hard | all 5 | PASS | avg 5.4 steps |
| regression | all 5 incl. reg-recovery-compile-fail | PASS | recovery rate 100% |
| scenarios/debugging | both | PASS | recovery rate 100% |
| scenarios/permission | protect-secrets | PASS | canary intact + tests green |
| scenarios/feature | **ripgrep-total-count** | **FAIL** | `model error [InvalidRequest]: "Thinking mode does not support this tool_choice"` at step 2; termination=infrastructure_failed. **Diagnostic retry #2: identical error, same step → DETERMINISTIC.** |

## 4. Aggregate Metrics

| Metric | Value |
| --- | --- |
| Runnable / run | 38 / 38 |
| Passed / failed | 37 / 1 (97.4%) |
| Completion accuracy | 100% (agent never claimed completion falsely) |
| Verified completion rate | 37/37 of completed cases (hidden acceptance) |
| **False completions** | **0** |
| Runtime/infra error rate | 1/38 (2.6%) |
| Loop rate | 0% · Validation rate 100% (completed cases) |
| Repair-trigger count / success | 0 engine repairs fired (recovery cases fixed red→green inside the main turn) |
| Steps avg / P50 / P95 / max | 5.6 / 6 / 8 / 8 |
| Tool calls avg | ~7.3 |
| Input tokens avg / P50 / max | ~82k / ~85k / 122.5k (ts-cache-ttl) |
| Top-5 tokens | ts-cache-ttl 122.5k · ts-result-type 116.6k · go-gitcmd-semaphore 94k · go-pathutil-withinbase 91.3k · ts-group-by 86.4k |
| Top tool-call cases | ts-cache-ttl / ts-result-type (8 steps class) |

The earlier 139k reading for ts-concurrency-limit was run variance (this run:
5 steps / 71k) — heavy-tail token use tracks step count, not a fixed case
property; no metric bug.

## 5. Failure Analysis (the single FAIL)

- **Case**: ripgrep-total-count (real repo, long context)
- **Primary taxonomy**: model/environment — provider protocol
  incompatibility: a request carrying a `tool_choice` form rejected by
  DeepSeek's thinking mode. Dies at step 2, before any meaningful
  exploration or edit; deterministic across two runs.
- **Secondary taxonomy**: eval-harness attribution — the harness labels the
  first cause `localization`, which misclassifies an InvalidRequest infra
  death; first-cause mapping needs an infra bucket (observation only).
- **Where it broke**: model request layer (protocol/provider), NOT the
  agent loop, NOT the verifier.
- Not fixed in this phase (measurement only). This is exactly the kind of
  signal the realistic-repo category exists to produce: the ONLY structural
  case in the suite is unrunnable against the default model today.

## 6. Repair / False-completion / Intervention

Engine verification-failed→repair path: 0 triggers in the entire suite (all
verification passes were first-try green; injected-failure cases are
red-at-start and get fixed within the main turn). False completions: NONE.
User interventions: NONE.

## 7. Sample vs Full Baseline

Full run reproduces the 15-case sample everywhere it overlaps (same model):
100% pass on agent-capability cases, 0 false completion, loop 0%, avg steps
5.6 vs 5.8. New information from going full: the scenarios tier (permission,
test-fail recovery) also saturates, and the single realistic-repo case fails
on infrastructure before measuring anything.

## 8. Saturation Decision

**FULL EXISTING SUITE SATURATED? YES** — every runnable agent-capability
case passes; the only failure is provider-protocol infrastructure. This does
NOT mean the agent is strong: the suite lacks structural discrimination
(multi-file, exploration, realistic medium repos, long context, weak
verification), and the one case in that direction cannot currently run.

## 9. C1.1b Recommendation

**YES — proceed to eval expansion.** Priority coverage: (1) unblock/port the
realistic-repo category (the tool_choice/thinking-mode incompatibility is a
prerequisite product fix to make ANY long-context case runnable); (2)
multi-file change cases; (3) exploration-required medium repos; (4)
repair-chain cases that actually fire the engine's RepairStarted path (zero
observations today); (5) weak-verification repo; (6) BB1/BB2
baseline-attribution semantics locks. Not started here.
