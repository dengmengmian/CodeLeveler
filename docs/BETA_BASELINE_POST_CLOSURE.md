# Beta Baseline — Post-Architecture Closure

**Frozen 2026-09-09.** This is the first baseline that an acceptance gate
signed off rather than a person, and the first one where the workspace suite is
green rather than green-except-known-failures.

It does not replace [`BETA_RELEASE_READINESS.md`](BETA_RELEASE_READINESS.md)
(the audit that opened the Beta) or
[`BETA_BLOCKER_RESOLUTION.md`](BETA_BLOCKER_RESOLUTION.md) (how its four
blockers closed). Those stay as they were written; a baseline rewritten to
agree with its successor stops being evidence.

## The baseline

| Field | Value |
| --- | --- |
| `BETA_BASELINE_HEAD` | `8761e24fa8ca0db4ae1394b3da8f6b2ac74c6132` |
| `BETA_BASELINE_TREE` | `e6fcf74ce8c06c4a9409af6e448f8c5f311505e8` |
| Workspace version | `0.2.0-beta.1` |
| `MODEL` / `PROVIDER` | `deepseek-v4-flash` · `deepseek` |
| `DOGFOOD_CONTRACT_VERSION` | V1 |
| `DOGFOOD_RESULT` | **PASS** — [result](evaluations/UNIFIED_DOGFOOD_ACCEPTANCE_V1_RESULT.md) |
| Machine | macOS 26.6.2 · `arm64` (Apple M4 Max) |

## What it rests on

| Gate | Result |
| --- | --- |
| `cargo fmt --check` | PASS |
| `cargo check --workspace --all-targets` | PASS |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | PASS, 0 warnings |
| `cargo test --workspace --all-features` | **PASS — 3432 passed, 0 failed** |
| `python3 -m unittest discover -s evals/tests` | PASS — 140 tests |
| Eval fixture-validity gates | PASS |
| Eval integrity audit (leakage / privilege escalation) | PASS — both counters 0 |
| Client-protocol schema export | PASS — 3 schemas current |
| WebUI protocol contract (`npm run check:protocol`) | PASS — `protocol.gen.ts` in sync |
| Release payload (`check_release_payload.sh`) | PASS |
| `leveler-agent-core` minimal example | PASS |
| Unified Dogfood Acceptance V1 | **PASS** — 7 cases, every gated counter 0 |

`WORKSPACE_TESTS_FAILED = 0` is the part that is new. The architecture-closure
baseline accepted "no regression from this change" over a suite that had been
red for a while; this one required the red to be gone. Fourteen were expected,
ten were found, all ten closed — eight of them stale assertions left by
De-engineering Wave 1/2 and the `TaskOutcome` / `VerificationStatus` split, one
a doc-comment and comment pair orphaned by the auto-format deletion, and one a
real product defect (below).

## What the dogfood gate caught

Its first run failed, which is the outcome that makes the rest of this
document worth anything.

A tool call whose elevation the user **denied** was announced to the durable
event log and never closed. `EventLog::dangling_tool_calls` pairs
`ToolCallStarted` against `ToolCallFinished`, so the refused call stayed open
forever, and `recover_crash_window` — which runs first on every resume and
every interactive chat turn — read it as a call that may have run and left a
side effect, blocking the session on `RecoveryConfirmationRequired`. A command
the runtime itself refused to run was being reported as one that might have.

Fixed in `8761e24f` with two deterministic regressions behind it. Full trace in
the [result](evaluations/UNIFIED_DOGFOOD_ACCEPTANCE_V1_RESULT.md#the-defect).

## What did not change

Stated because the temptation during a closure is to keep going:

- **Agent Core** — `leveler-agent-core`, `AgentHarness`, the generic
  `ToolRuntime` boundary and `ToolHost` admission: unchanged.
- **Authorization** — one authorization point, `ResolvedExecutionPolicy`,
  `WriteScope`: unchanged. Nothing was relaxed to make a test pass; the one
  sandbox-shaped failure on the list turned out not to exist.
- **Eval architecture** — one `evals/` root, one case format, no `eval_mode`,
  no dogfood runtime mode. V1 runs `leveler eval run` over committed YAML.
- **Deleted semantic machinery** — `CompletionContract`, `SupervisorPolicy`,
  `DriveGoalAgain`, the blocking-finding lifecycle, `auto_format`: still
  deleted. A stale-mechanism sweep over the code found `CURRENT_BUG = 0`; what
  remains is two serde aliases that decode old wire values and a handful of
  doc paragraphs that say these things were removed.

## Recovery truth

The one failing test that named recovery —
`ghost_reconciliation_survives_a_real_database_reopen` — was **not** a defect.
The file-backed reopen proves the runtime keeps the fact: window one leaves
`SubAgentStarted{agent-1, worker}` and no terminal; a fresh connection over the
same file writes `SubAgentFinished{ ok: false, contribution: None, "was lost
when its previous runtime window ended before it reported" }`, attributed back
to the turn that started the child. The unknown is recorded as unknown, nothing
is invented on the child's behalf, and no completion mechanism was restored to
satisfy the test — its assertion had simply not been migrated when the
completion supervisor was deleted.

The defect the dogfood found is the same rule one level down, and it is worth
stating once as a rule rather than twice as incidents:

> A fact the runtime holds must not decay into an unknown, and an unknown must
> never be recorded as a success.

## Next

- Re-run V1 at each release candidate. Run 2 of this result is the comparison
  baseline; `PERFORMANCE_SIGNAL` is recorded there and is not a gate.
- V2 should add an expected-`Blocked` outcome to the case format (today an
  honest block is scored as a failure by the harness's own pass rate) and
  replace the substituted case #6 with a real multi-agent ownership case once
  the control-plane matrix is runnable from this repository.
