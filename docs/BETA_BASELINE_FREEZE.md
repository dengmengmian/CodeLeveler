# CodeLeveler Beta Baseline Freeze

```
BETA_BASELINE_FREEZE=PASS
BETA_BASELINE_STATUS=FROZEN
READY_FOR_FORMAL_THREE_WAY_EVAL=YES

OPEN_BETA_BLOCKER=0
OPEN_BETA_REQUIRED=0
PHASE_B=HOLD
```

From here CodeLeveler is the object under measurement, not a treatment that
gets adjusted between runs. This file names the one source state, the one
executable, the one behavioural configuration, and the acceptance that stands
behind them.

---

## 1. Identity

Three identities, deliberately kept apart. Collapsing them is how a benchmark
ends up measuring something nobody can point at afterwards.

```
BASELINE_PRODUCT_HEAD=7486c3377f19be564643a41c4158b4ee77c1d8b3
BASELINE_PRODUCT_TREE=716dfa3df2f7bfa841070a1b3c410f6b1fe2cabf
                      the complete repository state that entered the freeze,
                      code plus every closure document

ACCEPTED_PRODUCT_TREATMENT_HEAD=702159956919691cae262c56aa80e2a5bd2db3c2
ACCEPTED_PRODUCT_TREATMENT_TREE=e6ffd9e1967030516634fba252ec401b71de4242
                      the object the F7 dogfood acceptance actually ran

BASELINE_RECORD_COMMIT=the docs-only commit carrying this file
                      `git log -1 --format=%H` — it describes the baseline
                      and is not the baseline product object
```

`BASELINE_PRODUCT_HEAD` is not the treatment head, and that is correct. The
repository moved past `7021599` by five documents and nothing else:

```
$ git diff --name-status 7021599..7486c33
A  docs/BETA_FINAL_PRODUCT_CLOSURE.md
A  docs/F7_GROUNDED_AUTHORITY_DOGFOOD_ACCEPTANCE.md
A  docs/O1_O2_GOVERNANCE_RESOLUTION.md
M  docs/RUNTIME_VERSION_CONSISTENCY.md
A  docs/RUNTIME_VERSION_CONSISTENCY_CLOSURE.md
```

```
PRODUCT_CODE_DELTA_SINCE_ACCEPTED_TREATMENT=NO
RUNTIME_SEMANTIC_DELTA_SINCE_ACCEPTED_TREATMENT=NO
PRODUCT_CODE_EQUIVALENT_TO_ACCEPTED_TREATMENT=YES
PRODUCT_CODE_CHANGED_DURING_FREEZE=NO
```

Checked mechanically against the diff, not asserted from the commit subjects.

## 2. The artifact

Built from `7486c33` with the repository's own release command — the one
`.github/workflows/release.yml:54` uses — not an invented one.

```
BUILD_COMMAND=cargo build --release --locked -p leveler-cli --target aarch64-apple-darwin
BUILD_PROFILE=release          lto="thin", codegen-units=1, strip=true
TARGET=aarch64-apple-darwin
FEATURES=(default)

BUILD_SOURCE_HEAD=7486c3377f19be564643a41c4158b4ee77c1d8b3
BUILD_SOURCE_TREE=716dfa3df2f7bfa841070a1b3c410f6b1fe2cabf
BUILD_WORKTREE_CLEAN=YES

OS=macOS 26.6.2
ARCH=arm64
rustc=1.90.0 (1159e78c4 2025-09-14)
cargo=1.90.0 (840b83a10 2025-07-30)
TOOLCHAIN_PINNED_BY=rust-toolchain.toml channel="1.90.0"
```

The toolchain pin matters: it makes the compiler part of the baseline rather
than a property of whoever runs the build.

### Identity, read back from the binary

```
EXPECTED_BUILD_IDENTITY=leveler 0.2.0-beta.1 (7486c3377f19)
OBSERVED_BUILD_IDENTITY=leveler 0.2.0-beta.1 (7486c3377f19)
BUILD_IDENTITY_MATCH=YES
```

Not inferred from the filename or the package version — asked of the binary.
`BuildIdentity{version, revision, dirty}` is stamped at compile time by
`leveler-core/build.rs`, so the revision is `7486c33`, the baseline product
head, and not the older treatment head. Same product semantics, different
binary identity: both facts are true and both are recorded.

### Exact bytes

```
BINARY_SHA256=121983c94a6766492b428560b06a2363c7628073bc7683b570f12daad3d67919
ARTIFACT_LOCATOR=$DOGFOOD_ROOT/eval/baselines/beta-7486c3377f19/leveler
ARTIFACT_VERSIONED=NO
```

The binary lives in the eval lab, not in Git. `BuildIdentity` gives source
identity; the hash gives byte identity; both are durable here even though the
33 MB artifact is not.

Alongside it: `leveler.sha256`, `BUILD_ENVIRONMENT.txt`, `build.log`.

### Smoke

```
cli_start          = OK    `leveler --help`, `leveler doctor`
runtime_start      = OK    `serve` bound its socket, reported pid
client_over_socket = OK    `sessions list` answered through the daemon
runtime_stop       = OK

BASELINE_ARTIFACT_SMOKE=PASS
```

Deliberately minimal. The acceptance is §6; this only proves the artifact is
not broken.

## 3. Model configuration

From the config the accepted dogfood actually used. Secrets are not recorded —
only `api_key_env` indirection exists in the file, and no key value appears
here.

```
MODEL_PROVIDER=deepseek (via the lab's OpenAI-compatible gateway)
MODEL_NAME=deepseek/deepseek-v4-flash
MODEL_ENDPOINT_CLASS=OpenAI-compatible chat completions
REASONING_MODE=thinking_flag        reasoning=true
REASONING_EFFORT=max
CONTEXT_WINDOW=1048576
RELIABLE_CONTEXT=786432
MAX_OUTPUT_TOKENS=393216
PARALLEL_TOOL_CALLS=false           max_parallel_tool_calls=1
THINKING_SUPPORTS_FORCED_TOOL_CHOICE=false
VISION=N/A
PROVIDER_TIMEOUTS=connect 20s · request 600s · idle_stream 120s
PRICING=input 0.1389 / output 0.2778 USD per MTok

MODEL_CONFIG_FROZEN=YES
```

### Reasoning pass-back — two knobs, only one of them the rejected one

```
BASELINE_REASONING_PASSBACK:
  keep_reasoning              = false   ← the knob the ablation REJECTED
                                          (TurnPolicy default; an eval-only knob)
  passback_reasoning_content  = true    ← a provider capability declaration
```

These are not the same thing and the distinction was verified in code rather
than assumed. `keep_reasoning` decides whether the assistant message carries
its chain; with it false — the production default — the wire carries
`reasoning_content: ""` whatever the provider declares.

```
REASONING_PASSBACK_PROMOTION=REJECTED
  input +34% · output +61% · turns +16% · first edit +5 turns
  cost +35% · acceptance tie
```

Formal Eval must not turn `keep_reasoning` on. Separately,
`CONTEXT_COST_MEASUREMENT_PLAN.md` leaves open whether
`passback_reasoning_content` should be deleted from the profiles since it now
carries nothing — **that is a post-baseline product question, not a mid-eval
tidy-up.** Named here so nobody "fixes" it between benchmark runs.

## 4. Runtime configuration

The dogfood config declares providers and models only. Everything below comes
from code defaults, and is recorded as such rather than presented as a chosen
value.

```
TRIMMING_THRESHOLD=64 KiB          PRUNE_BATCH_BYTES, compaction.rs:117
                                   source=runtime default
TRIMMING_TRIGGER=NOT_REACHED       observed reclaimable 25.5 / 28.6 KB
TRIMMING_THRESHOLD_CHANGE=NO

RECONCILIATION_DEADLINE=180 s      DEFAULT_RECONCILE_TIMEOUT
                                   source=runtime default
PERMISSION_MODE=assisted           CLI default
WORK_MODE=N/A                      not set by the eval harness
CONCURRENCY=N/A                    runs are sequential in the acceptance cohort

RUNTIME_CONFIG_FROZEN=YES
```

## 5. Prompt and capability identity

Prompts are source files, so their identity is the source identity. No copy is
made here; a copy would be a second thing to keep in sync.

```
PROMPT_SOURCE_HEAD=7486c3377f19be564643a41c4158b4ee77c1d8b3
PROMPT_FILES=in-tree Rust sources — notably
  leveler-agent/src/completion_contract.rs   contract derivation instruction
  leveler-agent/src/reconciliation.rs        reconciliation judge instruction
  leveler-agent/src/executor/…               system and agent instructions
PROMPT_IDENTITY_FROZEN=YES   (by BASELINE_PRODUCT_TREE)

CAPABILITIES=43 registered tools (31 core + 12 browser) + 5 agent-layer tools
             shell · read · search · write · browser · spawn/multi-agent · memory
CAPABILITY_CONFIG_FROZEN=YES   (compiled in; not toggled per run)
```

Formal Eval may not tune a prompt. A prompt edit is a source edit, and §8 says
what a source edit does to this baseline.

## 6. Acceptance provenance

An index, not a copy.

```
COMPLETION_TRUTH_BETA_CLOSURE=PASS
NEW_TREATMENT_DOGFOOD_ACCEPTANCE=PASS
FALSE_VERIFIED_TOTAL=0            over 24 fresh runs
RUNTIME_VERSION_CONSISTENCY_CLOSURE=PASS
BETA_FINAL_PRODUCT_CLOSURE=PASS
ZERO_KNOWN_BETA_RISK_GATE=PASS
```

| Evidence | Document |
| --- | --- |
| F7 engineering closure | `F7_FINAL_GROUNDED_VERIFICATION_CLOSURE.md` |
| Why a generic witness was rejected | `F7C_BEHAVIORAL_WITNESS_ARCHITECTURE.md` |
| F7 dogfood acceptance, 24 runs | `F7_GROUNDED_AUTHORITY_DOGFOOD_ACCEPTANCE.md` |
| Runtime replacement, incl. non-idle drain | `RUNTIME_VERSION_CONSISTENCY_CLOSURE.md` |
| O1/O2 governance | `O1_O2_GOVERNANCE_RESOLUTION.md` |
| The full gate audit and risk ledger | `BETA_FINAL_PRODUCT_CLOSURE.md` |
| D1–D4 real-repo dogfooding | `DOGFOOD_D1_D4_FINAL_REVIEW.md` |
| Browser capability | `GATE_6_BROWSER_CLOSURE.md` |
| Long-running goals | `LONG_TASK_RELIABILITY_FINAL_REVIEW.md` |
| Spawn / multi-agent | `MULTI_AGENT_PRODUCT_CLOSURE.md` |
| Context cost | `CONTEXT_COST_MEASUREMENT_PLAN.md`, `C2_1_CONTEXT_COST_ATTRIBUTION.md` |

Several of these carry Beta flags that were true when written and are not now;
each has a point-in-time banner pointing forward. They are left as they were
because an audit rewritten to agree with a later outcome stops being evidence.

### Workspace evidence, reused rather than re-run

```
cargo test --workspace --no-fail-fast   135 suites, 3695 passed, 0 failed, exit 0
cargo fmt --all -- --check              exit 0
clippy --workspace --all-targets        exit 0, 0 warnings

FULL_WORKSPACE_EVIDENCE_REUSED=YES
REUSE_REASON=produced at product tree e6ffd9e, which is byte-identical to the
             baseline's product code — 7021599..7486c33 is five documents
```

## 7. Known Beta limitations, carried into the baseline

`ZERO_KNOWN_BETA_RISK_GATE=PASS` means **no undispositioned blocking or
required Beta risk**. It does not mean no known limitation, and the fourteen
below travel with the baseline rather than being dropped at the freeze.

```
KNOWN_LIMITATIONS_COUNT=14
KNOWN_LIMITATIONS_FROZEN=YES
```

| | Limitation | Disposition |
| --- | --- | --- |
| BR-1 | Authority yield — 11 of 24 runs did correct work that ended `CompletedUnverified` | `AUTHORITY_YIELD_STATUS=CONCERN`, ACCEPTED |
| BR-2 | Reconciliation yield — 6 of 13 CU runs refused as `NotAccountedFor` | ACCEPTED; predates F7, and reverting F7 would not recover it |
| BR-3 | Three Windows tests fail (AppContainer semantics, a retitle hang) | ship macOS+Linux, hold the Windows artifact |
| BR-4 | Two ws-boundary tests flake under full-workspace parallelism | HISTORICAL_FLAKE, not reproduced in isolation or in the latest full run |
| BR-5 | R004-F6 vite ws through the forward proxy, never re-observed | OPEN_EVIDENCE_NEEDED, recorded as unproven |
| BR-6 | `duration_budget_stops_the_run_between_rounds` macOS flake | test margin |
| BR-7 | Reconnect historical-ToolGroup projection | cosmetic; durable events intact |
| BR-8 | Agent temp-file hygiene | deferred to Home & Runtime Hardening |
| BR-9 | Preset default models dated | `login` queries the real list |
| BR-10 | No cross-provider fallback | documented limit |
| BR-11 | No live `!command` streaming for confined Windows commands | documented limit |
| BR-12 | Linux backend enforces no read denial | evals run on macOS |
| BR-13 | R012-F1 durable-args truncation | inherited `OPEN_EVIDENCE_NEEDED` |
| BR-14 | `windows-gnu` is not `windows-msvc` | same Rust behind `cfg(windows)` |

Also carried, and not a defect: `passback_reasoning_content` now declares a
capability whose payload is always empty (§3).

`BETA_FINAL_PRODUCT_CLOSURE.md` §8 holds the full ledger with sources.

## 8. Mutation rules

The point of the freeze.

```
INVALIDATES THE BASELINE — a new candidate and re-run acceptance are required:
  product source change (any crate)
  prompt change (a prompt is source)
  model configuration change
  runtime behaviour configuration change (timeouts, budgets, thresholds,
    permission mode, capability set)
  build command, profile, target or toolchain change
  eval adapter change that alters behaviour

DOES NOT INVALIDATE:
  documentation and annotation
  raw evidence archival
  an eval scoring fix — but it must be recorded and applied symmetrically to
    every harness in the comparison, never to CodeLeveler alone
```

No patching between benchmark rounds. If something must change, the baseline is
invalidated first and said to be invalidated — not amended quietly.

```
ARTIFACT_REUSE_POLICY=Formal Eval runs the hashed artifact at
  ARTIFACT_LOCATOR. If it must ever be rebuilt, the rebuild verifies
  BuildIdentity and source head and records its own hash; the original is kept.
```

## 9. Competitor targets, frozen for the comparison

Recorded, not run. This task starts no benchmark.

```
ATOMCODE_VERSION=5.0.9
ATOMCODE_REVISION=52ca5e6

DEEPSEEK_HARNESS_VERSION=0.1.2-alpha.1
DEEPSEEK_HARNESS_REVISION=cd5ef8148
```

Both are already frozen in the lab per `$DOGFOOD_ROOT/STANDARD.md`, which also
carries the fairness constraints the comparison inherits:

```
MODEL_UPSTREAM_MATCH=PARTIAL     AtomCode adds an AtomGit hop
PERMISSION_FAIRNESS=ACCEPTABLE
TOKEN_FAIRNESS=LIMITED
TOKEN_RANKING=DISABLED           token numbers are incomparable across harnesses
```

## 10. Eval configuration

Referenced, not redesigned. `$DOGFOOD_ROOT/STANDARD.md` is authoritative for
isolation, fairness and evidence; the canonical case schema and scoring stay in
`evals/` and `crates/leveler-eval`.

```
EVAL_CONFIG_FROZEN=YES

FRESH_RUN_POLICY=every run fresh — no resumed session, no reused evidence,
                 no reused run directory (the runner refuses a used one)
RESUME_POLICY=none in acceptance runs
WORKSPACE_RESET_POLICY=isolated HOME/TMPDIR/XDG per run, inside DOGFOOD_ROOT
SUCCESS_ORACLE_POLICY=the case's own `expect` program decides correctness;
                      `false_completion_rate` is a first-class harness metric
COST_ACCOUNTING_POLICY=per-case cost_usd_micros from the harness, model pricing
                       from the model profile
TOKEN_ACCOUNTING_POLICY=recorded; NOT used for cross-harness ranking
FAILURE_CLASSIFICATION=harness `termination` + `failure_category`, with the
                       terminal refusal reason read from the session event log
HUMAN_REVIEW_POLICY=correctness from `expect_passed`, never from a transcript
                    reading; the Incorrect+Verified cell must stay 0
```

## 11. Freeze decision

```
WORKTREE_CLEAN_BEFORE_BUILD=YES
PRODUCT_CODE_DELTA_SINCE_ACCEPTED_TREATMENT=NO
RUNTIME_SEMANTIC_DELTA_SINCE_ACCEPTED_TREATMENT=NO
BETA_FINAL_PRODUCT_CLOSURE=PASS
ZERO_KNOWN_BETA_RISK_GATE=PASS
OPEN_BETA_BLOCKER=0
OPEN_BETA_REQUIRED=0
BUILD_FROM_CLEAN_HEAD=YES
BUILD_IDENTITY_MATCH=YES
BINARY_HASH_CAPTURED=YES
BASELINE_ARTIFACT_SMOKE=PASS
MODEL_CONFIG_FROZEN=YES
RUNTIME_CONFIG_FROZEN=YES
EVAL_CONFIG_FROZEN=YES
KNOWN_LIMITATIONS_FROZEN=YES
ACCEPTANCE_PROVENANCE_INDEXED=YES

BETA_BASELINE_FREEZE=PASS
BETA_BASELINE_STATUS=FROZEN
READY_FOR_FORMAL_THREE_WAY_EVAL=YES
PHASE_B=HOLD
```

`PHASE_B` stays `HOLD`: the freeze makes the comparison possible, it does not
start it. No tag, no release — a baseline is not a release, and a release tag
is not a baseline identity.

The next task is `CodeLeveler Formal Three-Way Eval — Phase B`, and it runs the
artifact named in §2 rather than building its own.
