# Beta Final Product Closure

Audit only. No product code changed. This decides whether every Beta-scoped
gate and known risk has a disposition, and therefore whether Baseline Freeze
may begin.

```
BETA_FINAL_PRODUCT_CLOSURE=PASS
READY_FOR_BETA_BASELINE_FREEZE=YES

OPEN_BETA_BLOCKER=0
OPEN_BETA_REQUIRED=0
PHASE_B=HOLD
```

**Nothing blocks the freeze.** Both required items closed after this audit was
first written, and the two updates are kept as updates rather than folded in —
an audit rewritten to agree with its outcome stops being evidence.

> **Update 1.** BR-B closed —
> [`O1_O2_GOVERNANCE_RESOLUTION.md`](O1_O2_GOVERNANCE_RESOLUTION.md). O1 and O2
> were never frozen as Beta gates; they entered this checklist as undefined
> labels.
>
> **Update 2.** BR-A closed —
> [`RUNTIME_VERSION_CONSISTENCY_CLOSURE.md`](RUNTIME_VERSION_CONSISTENCY_CLOSURE.md)
> §5a. The non-idle drain was proven on a real macOS A/B replacement: a daemon
> holding live daemon-owned background work stayed alive ~70 s after
> `ShutdownWhenIdle(BuildMismatch)` and exited only once it had drained.
>
> `OPEN_REQUIRED` is 0.

---

## 1. Identity

```
BETA_FINAL_START_HEAD=eae074dcb12306e82bfc8868efa358ef8085d7ed
BETA_FINAL_START_TREE=357f63a50cc47afd5a83355a047ddbeabb2f9d87
BETA_FINAL_START_WORKTREE=clean

TREATMENT_HEAD=702159956919691cae262c56aa80e2a5bd2db3c2
TREATMENT_TREE=e6ffd9e1967030516634fba252ec401b71de4242

CURRENT_PRODUCT_CODE_EQUIVALENT_TO_TREATMENT=YES
DOCS_ONLY_AFTER_TREATMENT=YES
PRODUCT_CODE_CHANGED_DURING_TASK=NO
UNEXPECTED_PRODUCT_DRIFT=NO

WORKSPACE_VERSION=0.2.0-beta.1
```

`git diff --name-status 7021599..HEAD` is exactly one line:

```
A  docs/F7_GROUNDED_AUTHORITY_DOGFOOD_ACCEPTANCE.md
```

So the repository HEAD moving past the treatment is documentation, not drift,
and the F7 dogfood acceptance still applies to the product object as it stands.

## 2. Gate definitions recovered

Recovered from the repository, not inferred from names.

| Gate | Frozen definition | Source | Prev. status | Latest evidence | Freshness | Status | Beta impact |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **Completion Truth** | no false Verified; one canonical `completion_debt()`; engine verification is an evidence producer | `F7_FINAL_GROUNDED_VERIFICATION_CLOSURE.md`, `F7_GROUNDED_AUTHORITY_DOGFOOD_ACCEPTANCE.md` | PASS (eng) | 24-run dogfood @7021599, false Verified 0 | current object | **PASS** | closed |
| **Performance** | no context blow-up, no request amplification, no persistence duplication, no Beta-blocker latency/memory/token regression | `CONTEXT_COST_MEASUREMENT_PLAN.md`, `C2_1_CONTEXT_COST_ATTRIBUTION.md` | PASS | F7 added no model call, no prompt text, no new payload; workspace green | current object | **PASS** | closed (see §4) |
| **O1** | **never a frozen gate** | `O1_O2_GOVERNANCE_RESOLUTION.md` | unknown | exhaustive tree + history search | current | **NEVER_ESTABLISHED** | none — BR-B closed |
| **O2** | **never a frozen gate** | `O1_O2_GOVERNANCE_RESOLUTION.md` | unknown | exhaustive tree + history search | current | **NEVER_ESTABLISHED** | none — BR-B closed |
| **O4 (O4-B)** | behaviour really happened, proof cannot be bound → CompletedUnverified — i.e. under-crediting real work | `F7_BEHAVIORAL_EVIDENCE_CHARACTERIZATION.md` §7 | OPEN | 11 × Correct+CompletedUnverified in the cohort | current object | **OPEN, measured** | non-blocking limitation (§5) |
| **D1** viu, small Rust | locate→modify→test on a real repo | `DOGFOOD_D1_D4_FINAL_REVIEW.md` §2 | PASS | reconnect ToolGroup projection gap, NOTE | @`6983d51` | **PASS** | open NOTE → next phase (TUI) |
| **D2** duf, medium Go | cross-file config, precedence, tests, docs | ibid. §3 | PASS | `--config/--no-config` = spec mismatch, not a defect | @`6983d51` | **PASS** | none |
| **D3** TailAdmin, Next.js | real-UI feature with real verification | ibid. §4 | CONDITIONAL | round ceiling CLOSED by R2; browser gap → Gate 6; temp-file hygiene DEFERRED | @`6983d51` + gate build | **CONDITIONAL PASS** | non-blocking |
| **D4** memos, full stack | long multi-window autonomous work | ibid. §5–7 | INCOMPLETE→replay | 4 windows, `stop=completed`, disconnect survived | replay @gate build | **PASS** | none |
| **Browser Final** | 12 structured tools, no shell bypass, boundary tests | `GATE_6_BROWSER_CLOSURE.md` | `NO_PRODUCT_CHANGE` | ~160 structured calls, 0 bypasses; browser suite green in the current workspace run | current object | **PASS** | residual R004-F6 (§6) |
| **Runtime Version Consistency** | running runtime's `BuildIdentity` == expected `BuildIdentity`, proven by real A/B replacement | `RUNTIME_VERSION_CONSISTENCY.md` | `PENDING_REAL_DOGFOOD` | real macOS A/B, both directions, plus a non-idle drain | current object | **PASS** | closed — BR-A |
| **Long Goal** | multi-window continuation, interruption/resume, terminal semantics, budget extension | `LONG_TASK_RELIABILITY_FINAL_REVIEW.md` | APPROVED | no BLOCKER, no unresolved MAJOR; workspace gates green | @closeout tree | **PASS** | m-1 DEFERRED (MINOR) |
| **Spawn / Multi-Agent** | `spawn_agent`, child lifecycle, ownership isolation, `claim_write_scope`, settlement, durable provenance | `MULTI_AGENT_PRODUCT_CLOSURE.md` | `OPEN_BETA_BLOCKER=0`, `OPEN_BETA_REQUIRED=0` | FS1–FS16 PASS, 0 violations | @closure tree | **PASS** | 1 `OPEN_EVIDENCE_NEEDED`, 7 post-Beta |

## 3. Completion Truth

```
COMPLETION_TRUTH_ENGINEERING_GATE=PASS
NEW_TREATMENT_DOGFOOD_ACCEPTANCE=PASS
FALSE_VERIFIED_TOTAL=0
ICG6R_FALSE_VERIFIED=0
COMPLETION_TRUTH_PRODUCT_GATE=PASS

COMPLETION_TRUTH_BETA_CLOSURE=PASS
```

No contradicting evidence was found. F7 is not reopened here.

## 4. Performance

There is no single frozen "Performance Closure" document with numeric
thresholds; the gate's substance lives in the context-cost work, and it is
recorded here as what it actually is rather than as a gate that was passed
against a number that does not exist.

Ten defects were closed in the context/accounting batch (EventLog full-context
growth, repeated full scans, discarded summary calls, `read_file` continuation,
Anthropic cache-prefix tail, long-session full reload, `extend_budget` bypass,
advisory accounting, compression accounting, duplicate provider request id).
None was reopened.

What this closure checks, against the current object:

```
new model call added by F7      = NO
prompt text changed by F7       = NO
new evidence payload            = NO   (VerifyRecord already existed)
new persistence duplication     = NO
REASONING_PASSBACK_PROMOTION    = REJECTED  (frozen, not re-run)
TRIMMING_THRESHOLD_CHANGE       = NO        (64 KiB, frozen)
                                  observed reclaimable 25.5 / 28.6 KB — trigger
                                  not reached, not retuned to fit the sample

PERFORMANCE_CLOSURE=PASS
```

## 5. Yield review

```
AUTHORITY_YIELD_STATUS=CONCERN                       (unchanged, not re-graded)
AUTHORITY_YIELD_BETA_DISPOSITION=ACCEPT_AS_KNOWN_BETA_LIMITATION

RECONCILIATION_YIELD_STATUS=CONCERN
RECONCILIATION_YIELD_BETA_DISPOSITION=ACCEPT_AS_KNOWN_BETA_LIMITATION
```

**Authority yield.** 11 of 24 runs produced correct work that ended
`CompletedUnverified`. Accepted as a Beta limitation because every safety
condition holds simultaneously:

```
false Verified                       = 0
Verified remains reachable           = YES (6 rounds, $0.015, ordinary request)
CompletedUnverified is truthful      = YES (expect_passed distinguishes them)
work product preserved               = YES
user told a false completion         = NO
unsafe mutation                      = NO
completion truth bypass              = NO
```

A 4.2 % Verified rate is not grounds for a blocker on this evidence: the cohort
is two long multi-requirement tasks chosen for difficulty, not the distribution
of ordinary use, and the one ordinary request in the set verified.

**Reconciliation yield.** 6 of the 13 `CompletedUnverified` runs were refused
with `NotAccountedFor` — the judge never accounted for the obligations at all.
This predates F7 and is untouched by it.

```
Safety issue?             NO   — no false Verified came from it
Correctness issue?        NO   — the work was correct; the accounting was absent
UX / yield issue?         YES
Beta blocker?             NO
Known limitation?         YES
```

The consequence worth carrying forward:

```
rollback F7  !=  recover all Verified yield
```

**O4-B is the same phenomenon under an older name** — "behaviour really
happened, proof cannot be bound". The cohort measured it at 11 runs. It is now
evidenced rather than suspected, and it stays open as a capability gap, not a
defect.

## 6. Browser

Gate 6 closed the capability with `NO_PRODUCT_CHANGE`: 12 structured tools,
~160 structured calls across four runs, zero shell/Playwright bypasses.

Two things are carried rather than closed:

**R004-F6 — `OPEN_EVIDENCE_NEEDED`.** The vite-websocket-through-forward-proxy
path was fixed as a grant-scoped loopback ws and never re-observed in
production, because the two later browser tasks never booted their apps. Gate 6
records it as unproven rather than quietly closed, and that stands.

**The ws-boundary tests flake under full-workspace parallelism, and this
closure adds a second instance.** The prior record (audit R-9, resolution risk
#5) names `loopback_ws_from_a_granted_dev_page_connects`. During the F7-C
workspace run in this line of work, **`websocket_egress_is_gated`** — a
different test in the same file — failed once under full load, then passed
alone (1/1) and with its whole suite (11/11), and the later F7 Final workspace
run was green including that suite.

```
Risk #5's stated trigger:  "It failing in isolation, or CI failing on it —
                            at which point it is a defect, not contention."
Observed here:             isolated rerun PASSED; suite PASSED; latest full
                            workspace PASSED (135 suites, 3695/0, exit 0)
⇒ trigger NOT met — still contention
⇒ but the risk's scope widens: two ws-boundary tests, not one
```

Recorded as `HISTORICAL_FLAKE / CURRENTLY_NOT_REPRODUCED`, never as "did not
happen".

```
BROWSER_FINAL_CLOSURE=PASS  (capability), with R004-F6 open as evidence-needed
```

## 7. What blocks the freeze

Two items, neither related to F7.

### BR-A · Runtime Version Consistency had never had its real dogfood — **CLOSED**

`RUNTIME_VERSION_CONSISTENCY.md` closes with its own status line:

> 确定性测试见 `RUNTIME_VERSION_CONSISTENCY_CLOSURE` 的机器可读门。
> **macOS A/B 真实 dogfood 未跑**，因此闭环状态是 `PENDING_REAL_DOGFOOD`，不是 PASS。

The referenced closure document **does not exist in the repository**. The gate's
own definition is the runtime replacing itself:

```
install new artifact → ShutdownWhenIdle(UpdateReady) → drain → exit
→ start new → verify BuildIdentity
```

and its definition of "update complete" is explicitly *not* "the file on disk
changed" but "the running runtime's BuildIdentity == the expected BuildIdentity".

F7's dogfood proves **build** identity (`EXACT_OBJECT_HANDOFF=PASS`, embedded
revision read back as `702159956919`). It does not exercise **replacement** —
the daemon shutting itself down, draining, and re-handshaking as a new build.
That is a different surface and it has never been run.

```
BETA_BLOCKER=NO
BETA_REQUIRED=NO
BR_A=CLOSED
```

**Closed after this audit was written.** The dogfood ran: two real builds, the
binary replaced by atomic rename so the daemon stayed on its old image, both
directions, and — the half that mattered — a retirement against a daemon
holding live daemon-owned background work, which stayed alive about seventy
seconds and exited only once it had drained.
[`RUNTIME_VERSION_CONSISTENCY_CLOSURE.md`](RUNTIME_VERSION_CONSISTENCY_CLOSURE.md)
§5a carries the timestamps.

A gate whose own author wrote `PENDING_REAL_DOGFOOD` could not be closed by
reading it, and was not.

### BR-B · O1 and O2 have no recoverable definition — **CLOSED**

Resolved by [`O1_O2_GOVERNANCE_RESOLUTION.md`](O1_O2_GOVERNANCE_RESOLUTION.md).

An exhaustive search of the current tree and the full history found no frozen
definition, acceptance criterion, evidence contract or closure document for
either label, and no deleted or renamed gate matrix that once held them. The
one same-named artifact that does exist — `Production matrix O1–O11` in
`eval/safety/manifest.yaml` — is an ownership **eval case series** whose cases
live in an external control plane, explicitly excluded from adoption
denominators. It is not a superseding gate, and the ownership substance it
measures is separately covered by the Multi-agent execution gate (FS1–FS16
PASS, 0 ownership denials).

They entered this checklist as labels in the closure handoff, not from a
repository decision.

```
O1_STATUS=NEVER_ESTABLISHED_AS_FROZEN_GATE
O2_STATUS=NEVER_ESTABLISHED_AS_FROZEN_GATE
BETA_BLOCKER=NO
BETA_REQUIRED=NO
BR_B=CLOSED
```

## 8. Beta Risk Ledger

The single documentary ledger. Severity is the earlier documents' own where one
exists.

| ID | Title | Category | Source | Sev | Repro | Status | Blocker | Required | Disposition | Post-Beta |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| BR-A | Runtime Version Consistency never dogfooded (A/B daemon replacement) | Runtime | `RUNTIME_VERSION_CONSISTENCY.md` | MAJOR | n/a | **CLOSED** | NO | NO | real A/B dogfood + non-idle drain proof (`RUNTIME_VERSION_CONSISTENCY_CLOSURE.md`) | — |
| BR-B | O1 / O2 definitions not recoverable | Process | this audit | MAJOR | n/a | **CLOSED** | NO | NO | never frozen gates; invalid reference removed (`O1_O2_GOVERNANCE_RESOLUTION.md`) | — |
| BR-1 | Authority yield — correct work ends CompletedUnverified (11/24) | Completion | F7 dogfood | MAJOR | YES | OPEN, measured | NO | NO | ACCEPT_AS_KNOWN_BETA_LIMITATION | proof capability |
| BR-2 | Reconciliation yield — `NotAccountedFor` (6/13 of CU) | Completion | F7 dogfood | MAJOR | YES | OPEN, measured | NO | NO | ACCEPT_AS_KNOWN_BETA_LIMITATION | judge accounting |
| BR-3 | Three Windows tests fail (AppContainer semantics, retitle hang) | Platform | `BETA_BLOCKER_RESOLUTION.md` 0b | MAJOR | YES on Windows | OPEN | NO | NO | ship macOS+Linux, hold the Windows artifact — already decided | needs a Windows machine |
| BR-4 | ws-boundary tests flake under full-workspace parallelism (2 tests) | Browser | audit R-9 + this closure | MINOR | under load only | HISTORICAL_FLAKE / NOT REPRODUCED | NO | NO | serialize or diagnose; trigger for escalation is stated | — |
| BR-5 | R004-F6 vite ws through the forward proxy — never re-observed | Browser | `GATE_6_BROWSER_CLOSURE.md` | MINOR | n/a | OPEN_EVIDENCE_NEEDED | NO | NO | unproven, recorded as such | observe in a real app-boot task |
| BR-6 | `duration_budget_stops_the_run_between_rounds` macOS flake | Test | resolution 0a | MINOR | intermittent | OPEN | NO | NO | widen the margin | — |
| BR-7 | D1 reconnect historical-ToolGroup projection | TUI | `DOGFOOD_D1_D4_FINAL_REVIEW.md` | NOTE | YES | OPEN | NO | NO | cosmetic; durable events intact | TUI presentation |
| BR-8 | Agent temp-file hygiene (`scripts/.tmp-*`, `chrome.log`) | Hygiene | ibid. D3 | MINOR | YES | DEFERRED | NO | NO | Home & Runtime Hardening | — |
| BR-9 | Preset default models dated (`gpt-4o`, `claude-sonnet-4-5`) | Provider | resolution #6 | MINOR | n/a | OPEN | NO | NO | `login` queries the real list; preset is fallback | refresh |
| BR-10 | No cross-provider fallback | Provider | resolution #7 | MINOR | n/a | OPEN | NO | NO | not promised; documented limit | post-Beta |
| BR-11 | No live `!command` streaming for confined Windows commands | Platform | resolution #3 | MINOR | YES on Windows | OPEN | NO | NO | documented limit | — |
| BR-12 | Linux backend enforces no read denial (eval hygiene) | Eval | resolution #4 | MINOR | YES on Linux | OPEN | NO | NO | evals run on macOS | — |
| BR-13 | R012-F1 durable-args truncation | Multi-agent | `MULTI_AGENT_PRODUCT_CLOSURE.md` | MINOR | n/a | OPEN_EVIDENCE_NEEDED | NO | NO | inherited, unchanged | — |
| BR-14 | `windows-gnu` is not `windows-msvc` | Platform | resolution #2 | NOTE | n/a | OPEN | NO | NO | same Rust behind `cfg(windows)` | — |
| BR-15 | Binaries unsigned (no notarization / code signing) | Release | audit R-8 | MINOR | n/a | OPEN | NO | NO | stated Beta-accepted limit | signing has lead time |
| BR-16 | Long-goal m-1 coarse no-progress proxy | Long Goal | `LONG_TASK_RELIABILITY_FINAL_REVIEW.md` | MINOR | n/a | DEFERRED | NO | NO | backlog per closeout scope | — |

```
KNOWN_BETA_RISKS_TOTAL=18
OPEN_BLOCKERS=0
OPEN_REQUIRED=0
ACCEPTED_LIMITATIONS=14
POST_BETA_ITEMS=7        (per MULTI_AGENT_PRODUCT_CLOSURE, unchanged)
```

Post-Beta directions (Codex/Claude child providers, ACP, remote worker,
continuable child, extension framework, full-trajectory Web UI, cloud) are
**not** promoted to blockers. No Beta gate requires them.

## 9. Verification

`PRODUCT_CODE_CHANGED_DURING_TASK=NO`, so the workspace evidence is reused, and
it is fresh by identity rather than by date: it was produced at the tree that is
still the product code today.

```
cargo test --workspace --no-fail-fast   135 suites, 3695 passed, 0 failed, exit 0
cargo fmt --all -- --check              exit 0
CARGO_TARGET_DIR=target/clippy \
  cargo clippy --workspace --all-targets  exit 0, 0 warnings, 0 errors
```

Produced at product tree `e6ffd9e1967030516634fba252ec401b71de4242`, which is
identical to the current product code (`7021599..HEAD` is one docs file). The
browser suite is inside that run and it was green.

## 10. Final gate

```
COMPLETION_TRUTH_BETA_CLOSURE=PASS
PERFORMANCE_CLOSURE=PASS

O1=NEVER_ESTABLISHED_AS_FROZEN_GATE   (BR-B closed)
O2=NEVER_ESTABLISHED_AS_FROZEN_GATE   (BR-B closed)
O4=OPEN (measured; non-blocking limitation)

D1=PASS
D2=PASS
D3=CONDITIONAL PASS
D4=PASS

BROWSER_FINAL_CLOSURE=PASS
RUNTIME_VERSION_CONSISTENCY_CLOSURE=PASS

AUTHORITY_YIELD_BETA_DISPOSITION=ACCEPT_AS_KNOWN_BETA_LIMITATION
RECONCILIATION_YIELD_BETA_DISPOSITION=ACCEPT_AS_KNOWN_BETA_LIMITATION

ZERO_KNOWN_BETA_RISK_GATE=PASS
  0 undisposed blockers; 0 undisposed required items;
  14 documented, dispositioned, non-blocking limitations

OPEN_BETA_BLOCKER=0
OPEN_BETA_REQUIRED=0
PHASE_B=HOLD

BETA_FINAL_PRODUCT_CLOSURE=PASS
READY_FOR_BETA_BASELINE_FREEZE=YES
```

`PASS`. Nothing is known to be broken: no false Verified recurrence, no
completion-truth bypass, no unsafe mutation, no data loss, no repeatable
Beta-scoped correctness blocker, no runtime version mismatch observed. The two
gates that were open were closed against their own definitions — one by running
the dogfood it named, one by establishing it was never a gate — rather than by
assertion, which is what this program exists to prevent.

Fourteen known limitations remain, each documented, dispositioned and
non-blocking (§8). `ZERO_KNOWN_BETA_RISK_GATE=PASS` means no *undisposed*
Beta-scoped blocking or required risk, not the absence of known issues.

## 11. How the hold was cleared

```
BR-A   CLOSED. The macOS A/B runtime-replacement dogfood the gate names was
       run, including the non-idle drain: a daemon holding live daemon-owned
       background work did not exit on ShutdownWhenIdle(BuildMismatch), waited
       ~70 s, drained, then exited; the replacement reported the expected
       BuildIdentity, confirmed from both directions.
       → RUNTIME_VERSION_CONSISTENCY_CLOSURE.md §5a

BR-B   CLOSED. O1/O2 were never frozen gates; the invalid reference is
       removed from the required ledger.
       → O1_O2_GOVERNANCE_RESOLUTION.md
```

Neither required touching product code, and neither reopened F7.

`READY_FOR_BETA_BASELINE_FREEZE=YES`. The freeze itself is the next task and is
not started here: no tag, no release, no Formal Three-Way Eval. `PHASE_B` stays
`HOLD` until the baseline is frozen.
