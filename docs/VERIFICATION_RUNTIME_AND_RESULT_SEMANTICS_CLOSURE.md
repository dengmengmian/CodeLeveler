# Verification Runtime and Result Semantics Closure

Status: closed. The four residuals that blocked a verified self-dogfood are
resolved, and the question "what may an agent verify about this repository?" is
now answered by a declaration in the repository instead of by an assumption in
the runtime.

Two claims in this document are deliberately narrow, and both are load-bearing:

```text
Agent Verification Passed
    = the declared Agent Verification Contract passed.

It does NOT mean every test in the repository ran.
```

## 1. Original residuals

From `docs/BETA_FINAL_GATE_RECONCILIATION.md` §10, the four verification-line
residuals:

| # | Residual | Class |
| --- | --- | --- |
| 1 | Self-repo tests fail inside the verify sandbox (`~/.leveler/run/locks`) | PRODUCT_USABILITY_DEBT |
| 2 | `VerificationFinished.passed` is the gate, `true` for unavailable | PRODUCT_SEMANTICS_DEBT |
| 11 | Baseline attribution does not recognise a pre-existing `node --test` failure | DEFERRED_PRODUCT_WORK |
| 12 | A failing non-gating `cargo fmt` still ends "verification passed" | PRODUCT_SEMANTICS_DEBT |

## 2. Root causes

### R1 — the sandbox did not isolate CodeLeveler's own runtime root

A confined child inherited the host's real `LevelerHome`. A project's test suite
that drives CodeLeveler's own code — which is what a self-dogfood run is —
takes an advisory lock under `<home>/run/locks` in-process, and the sandbox's
writable roots are the workspace, the per-command scratch and the tool caches.
The write was refused with `Operation not permitted`, so `edit_contract` failed
`6 passed; 2 failed` and the run ended `CompletedUnverified` for a change that
passed every check (`FOUNDATION_ACCEPTANCE_AND_FREEZE.md` §6).

### R2/R3 — the two roles were already separate facts; only the evidence was missing

`VerificationFinished.passed` is the completion gate and is `true` for a run
that owed no check. The verdict is a separate field (`verification:
Option<VerificationStatus>`), written by one mapper (`verification_status_of`),
and every consumer reads the verdict:

```text
cli/render.rs        only `Passed` prints "passed"; absent says "unverified (legacy row)"
tui/screens.rs       one glyph per CheckState, `passed` only when recorded
web/completionTruth  verifyState() maps the verdict, not the gate
mobile/session_state reads UiVerification, never the engine gate
app/observability    "ok" only for VerificationStatus::Passed
```

So R2/R3 needed no change; they needed tests, and they now have the truth table
for the legacy row, the projection invariant, and three consumer-level proofs
that a failed non-gating check is displayed as failed beside a passing verdict.

### R4 — baseline attribution had no Node parser

`parse_failed_tests` dispatched to `cargo` and `go` only, so a failed
`node --test` — run directly or through the package manager's `test` script —
produced no test-level ids. `pre_dates_change` requires test-level proof for a
`Test` check, so a failure that pre-dated the change could not be proven
pre-existing and kept gating as though the change had caused it.

## 3. Sandbox runtime isolation

`apply_sandbox_environment` already redirected `TMPDIR`, `CARGO_HOME`,
`GOCACHE`, `GOMODCACHE`, `XDG_CACHE_HOME`, `npm_config_cache` and the rest — a
confined program's private state lives inside the roots it may write. It now
does the same for CodeLeveler's own root:

```text
LEVELER_HOME = <scratch>/leveler-home
```

One variable is enough because every runtime path derives from that single root
(`leveler_core::LevelerHome`): locks, state database, daemon control files are
isolated together and cannot end up split across two homes. The root is
per-command, inside an already-writable root, and removed with the scratch.

```text
RUNTIME_ROOT_AUTHORITY        = LevelerHome::resolve() ($LEVELER_HOME first)
ISOLATED_ROOT                  = <home>/run/sandboxes/<scratch>/leveler-home
REAL_USER_HOME_TOUCHED         = NO
CONCURRENT_ISOLATION           = two runs never share a root
REPRESENTATIVE_SELF_REPO_TEST  = leveler-tools --test edit_contract
  before                       = 4/4 FAILED, "lock …/run/locks/6e234d586f5cda2a.lock:
                                 Operation not permitted", target 6 passed / 2 failed
  after                        = 4/4 passed, target green
```

The write probe is POSIX-only, and says so: it resolves `$LEVELER_HOME` for
itself in a shell script, which is the point. Windows confines with a
Low-integrity label list computed by the same `writable_roots_for_scope` that
admits the scratch, so the property holds there by construction and by the
existing label tests.

## 4. Two verification layers

### Layer 1 — Agent Verification Contract

Declared in `.leveler/config.yaml`, run inside the verify sandbox, and what an
agent's work is judged against:

```text
format  cargo fmt --all -- --check                    (non-gating, by policy)
build   cargo check --workspace --all-features --locked
test    cargo test --workspace --all-features --locked --exclude <7 crates>
```

Selected-set properties, measured inside the real sandbox:

```text
targets = 83   tests = 2022 passed, 0 failed, 15 ignored   elapsed = 90s
sandbox_apply errors = 0
stand-down lines = 0
```

### Layer 2 — Operator / CI / Release Gate

Unchanged, and unchanged on purpose:

```text
cargo test --workspace --all-features --locked --no-fail-fast   (what CI runs)
+ web: npm ci / check:protocol / typecheck / test / build
+ mobile: flutter analyze / flutter test
```

### Why the exclusion list exists, and why it is not a shortcut

Running the full suite under the fence was attempted and measured, not assumed:

```text
cargo test --workspace  inside the verification sandbox
  = 72 failing tests across 12 test binaries
    35  nested sandbox / confined child   sandbox-exec: sandbox_apply: Operation not permitted
    22  commands that could not run, so their assertions could not hold
     9  write refused
     6  daemon/socket could not start under the fence
```

macOS seatbelt and Linux bubblewrap **cannot nest**. This is a platform limit,
not a defect: those tests are about starting a sandbox, owning a process tree,
or binding a control socket, and a workspace-only write fence intentionally does
not grant any of that. Making them "pass" would require either opening the
user's real home to the sandbox (forbidden: isolate, never allow real state) or
letting each test detect confinement and skip itself — which would report
`verification=passed` for a run in which a third of the suite never executed.

So they are **absent from the contract**, not skipped at runtime:

```text
excluded: leveler-execution, leveler-verifier, leveler-tools,
          leveler-agent, leveler-app, leveler-cli, leveler-remote-agent
```

Each one owns at least one test that fails under confinement, and the exclusion
is per crate because cargo cannot express a per-target exclusion within one
invocation.

## 5. Consumer contract

```text
FALSE_VERIFIED_CONSUMER_PATHS = 0
```

CLI, TUI, Web, Mobile, the event bridge and observability all answer "did this
pass" from the verdict. A check that could not run is never a check that
passed: `tool_missing` and `environment_unavailable` keep their own states in
every consumer, and an unreadable status maps to `unknown`, never to a pass.

## 6. Compatibility

No protocol, schema or persistence change. `VerificationFinished.verification`
keeps its `#[serde(default)]` and legacy rows (`passed` alone) still read: a
closed gate that failed reads `failed`, a closed gate that passed reads
"unverified (legacy row)".

## 7. Mechanical tests

```text
leveler-execution/tests/verify_runtime_root.rs   isolated root is writable,
                                                 the parent home is untouched,
                                                 two runs never share a root,
                                                 the repository's own target passes
leveler-verifier  parse/attribute matrix         Node pre-existing, Node regression,
                                                 unparsable → still gates
leveler-verifier  real node --test               through the production verifier
leveler-app       projection truth table         gate vs verdict, legacy rows
leveler-tui       screen render                  failed check shown failed beside
                                                 a passing verdict
leveler-web       completionTruth                same, on the web projection
leveler-verifier/tests/self_repo_contract.rs     declared contract replaces
                                                 discovery; commands execute;
                                                 failure propagates
leveler-project/tests/agent_verification_contract.rs
                                                 the contract is workspace-wide;
                                                 every stand-down lives in an
                                                 excluded crate (Test C)
```

`leveler_test_support::already_confined` exists for tests that must observe
confinement. It is allowed only where the standing-down test is not part of the
contract, and `every_stand_down_lives_in_a_crate_the_contract_excludes` enforces
exactly that — from inside the contract, so the guard is active where it
protects.

## 8. Self-repo dogfood

Real product path, real model (`deepseek/deepseek-v4-flash`), real repository,
real sandbox, isolated `LEVELER_HOME` (a copy of the provider config, so the
user's own home is provably untouched).

```text
task      document TaskOutcome::Failed / Interrupted in leveler-lifecycle
tools     grep, read_file, apply_patch, run_command, update_goal
targeted  cargo test -p leveler-lifecycle → 54 passed
checks    format=passed  build=passed  test=passed
verdict   verification=passed   outcome=completed   stop_reason=Completed
exit      CLI_EXIT=0
home      ~/.leveler: 135641 files before, 135641 after, 0 added / 0 removed / 0 changed
```

```text
FULL_WORKSPACE_TESTS_IN_DOGFOOD_SANDBOX = NOT_PART_OF_CONTRACT
```

## 9. Known-red Node attribution

A `node --test` suite that is already red on the baseline, with an unrelated
change, is attributed pre-existing from the reporter's own failure markers. A
failure the baseline did not have still gates, and output from a runner with no
parser yields no ids and therefore still gates: unattributed fails closed, never
blames the baseline.

## 10. Gating semantics

```text
non-gating check = failed
gating checks    = passed
overall          = passed
per-check        = failed
```

Both facts survive in every layer: the report test, the projection, the CLI's
per-check lines, the TUI's verification screen and the web Inspector. The role
itself is not on the wire, so a client can show the two facts but cannot explain
why the failed check did not block; that is recorded as a residual below.

## 11. Final gates

```text
VERIFY_SANDBOX_RUNTIME_ISOLATION           = PASS
VERIFICATION_RESULT_MODEL                  = PASS
GATING_RESULT_SEMANTICS                    = PASS
BASELINE_ATTRIBUTION                       = PASS
VERIFICATION_CONSUMERS_AUTHORITY_SAFE      = PASS
SELF_REPO_AGENT_VERIFICATION_CONTRACT      = PASS
SELF_REPO_DOGFOOD_VERIFIED                 = PASS
FALSE_VERIFIED_CONSUMER_PATHS              = 0
AGENT_VERIFY_SELECTED_TESTS_STAND_DOWN_COUNT = 0
OPERATOR_FULL_GATE                         = PASS
VERIFICATION_CLOSURE                       = PASS
```

CI history for this closure, stated in full and not tidied:

```text
INITIAL_CANDIDATE_HEAD                   = 4ffb3f4
INITIAL_CANDIDATE_CI_ATTEMPT_1           = FAIL   (windows-latest: the new
                                            test target's own portability bugs —
                                            a .pdb picked as a test binary, and a
                                            POSIX-shell probe — not a product
                                            regression)
PORTABILITY_FIX_HEAD                     = bdb4c30
PORTABILITY_FIX_CI_ATTEMPT_1             = PASS
```

## 12. Residuals

```text
1. The per-check gating role is not on the wire. A client shows "cargo fmt
   failed" and "verification passed" but cannot say why the first did not
   block. Fixing it means EngineEvent + protocol + generated TS + mobile
   golden, which is not mechanically required by this closure.
2. The verifier's own tests cannot run under the fence (they run real checks),
   so the crate that owns the result model is verified by Layer 2 only.
3. Windows has no equivalent of the POSIX write probe for the isolated root;
   the property is covered there by construction and by the label tests.
```
