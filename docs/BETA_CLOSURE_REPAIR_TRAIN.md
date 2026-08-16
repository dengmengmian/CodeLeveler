# Beta Closure Repair Train — Execution Record

Branch `beta-closure/unattended` off `c3bf11b`. Ten commits, `234755f` → `23f7ede`.

**Overall: PARTIAL.** Gates 1, 2, 3, 6, 7 closed. Gate 4 partial. Gate 5 not started, because it depends on the half of Gate 4 that is not built. `BETA_CLOSURE_CANDIDATE = NOT_READY`, and the reason is stated rather than absorbed.

| Gate | Result | Commits |
| --- | --- | --- |
| 1 · Secret Propagation (F6) | **PASS** | `234755f` `c68c403` `fd5bd3c` `a37f89d` |
| 2 · Verification & Goal Truth | **PASS** | `4073dc2` |
| 3 · Long-running Goal Closure | **PASS** | `0064170` `1fa70ba` |
| 4 · Real Reviewer Mechanism | **PARTIAL** | `4714486` |
| 5 · Spawn Reliability + Child Result | **NOT STARTED** | — |
| 6 · Browser Capability Closure | **NO_PRODUCT_CHANGE** (audited) | `23f7ede` |
| 7 · Build / Release Provenance | **PASS** | `116b992` |

## What each gate actually changed

**Gate 1 — F6 → `VERIFIED_FIXED`.** Value-position-aware detection at `ToolHost::dispatch_raw`, the one boundary every tool result crosses, so a credential never reaches the model or the provider; a session-scoped registry then keeps an identified value out of durable text arriving by any other path. The obvious fix was refuted by measurement first: relocating the old scrubber destroys `let password = config.password;` while missing `API_PASSWORD=…`.

Three defects were found by the gate's own daemon smokes, not by its unit tests: durable records still used the old keyword scrubber and mangled the agent's own correct answer; a multi-byte slice panicked the sanitizer on Chinese prose; markdown inline code was read as a credential. Each is pinned by a regression.

**Gate 2 — R003-F1 → `VERIFIED_FIXED`, N2 → `VERIFIED_FIXED`, N6 reinforced.** Environment classification now covers what R003 actually hit (disk, fd, memory exhaustion, absent global tooling), so a correct fix on a full disk is no longer recorded as a code defect. N2 needed no new data: `VerifyRecord.after_mutation_seq` already knew a check ran before anything changed, and a task that was expected to mutate can no longer close as `Verified` on that evidence alone. A red baseline reproduction — the correct shape — is explicitly not flagged.

**Gate 3 — R007-F3 → `VERIFIED_FIXED`, R6-P5 → `VERIFIED_FIXED`.** A work-window boundary leaves the session resumable, so goal-owned services now survive it; R007 hit its ceiling twice and spent each following window rebuilding the dev server the reap had just killed. The four existing R6-P4 tests pin that a genuine goal terminal still reaps. Launch now names unfinished work and the exact command to resume it, because every resume in Batch #1 needed a UUID the user had no way to know.

**Gate 4 — partial.** `ExecutionRole::Reviewer` exists, a `ReviewTrigger` policy decides when review is warranted from the shape of the change (wide diff, security surface, concurrency surface, explicit request — never "always", which R008 and R009 refute), and a change that warrants review with no review on record can no longer close as `Verified`. A reviewer that started and died without finishing is deliberately not credited — that is N1's shape.

**What is missing is the mechanism half:** the harness does not itself spawn the reviewer. Child creation lives inside the model's tool-call handling in `drive.rs`, so harness-initiated review needs an orchestration path that does not exist. N7 is therefore **not closed** — the product now knows what a reviewer is and refuses to claim verification without one, but it cannot yet produce one.

**Gate 5 — not started.** It exists to fix N1 and N4 against *real* child traffic, and Gate 4's mechanism half is what produces that traffic. Building the child-result contract now would harden a path nothing takes — the exact error the Final Review warned against.

**Gate 6 — no product change.** 12 structured tools cover the measured workflow; ~160 calls across the batch with zero bypasses; 29/29 browser tests green. Both negative runs failed on an unbootable dev server, which is Gate 3's territory.

**Gate 7 — `leveler --version` reports its commit** and marks a build from a modified tree `dirty · UNTRUSTED`. It proved itself immediately: the first build off an uncommitted `build.rs` correctly refused to claim it was that commit.

## Validation

- Workspace: **2793 passed / 0 failed** (98 binaries) · clippy `-D warnings` clean · fmt clean.
- Integrated daemon smokes on the candidate binary: **F6 12/12 · P0-P4 7/7 · R6-P1 plan gate 2/2**.
- Reverse validation on every accident fix: Layer A, Layer B, N2 baseline-green, F3 window boundary — each turns its test RED when disabled and GREEN when restored.
- Binary provenance: HEAD `23f7edef1ad3`, tree clean, built `d9f6f3ad573c…` → installed `791d2c7ba86a…`, codesigned; `--version` self-reports the commit with no UNTRUSTED marker.

Two workspace failures appeared once under full parallelism (`apply_patch` concurrent-write, browser loopback) and passed both individually and at `--test-threads=4`: resource contention, not regression.

## Tooling added (control repo)

`gate_preflight.py` implements OFFICIAL_GATE_PREFLIGHT, and `install_candidate.py` records an install manifest. This exists because Gate 1 lost two investigation cycles to a stale daemon serving the previous binary — and the preflight then immediately caught a second, different drift (the installed binary had been replaced by another session). Note the subtlety it also fixed: codesigning rewrites the Mach-O, so a build artefact never hashes equal to the installed file; comparing them directly is a false-alarm generator, and the manifest is what ties them together.

## Not done, and not claimed

- **N7 mechanism half** — harness-initiated reviewer spawn (Gate 4).
- **N1 / N4** — blocked behind it (Gate 5).
- **R007-F4 TUI contrast** — deliberately untouched: another session holds uncommitted work in `theme.rs` (63 insertions). Duplicating it guarantees a conflict on the exact file being rewritten and wastes one of the two efforts. It stays `OPEN_BETA_REQUIRED`, owned elsewhere, and is explicitly not being passed off as fixed.
- **R005-F-P2** — Gate 7 makes the *binary* traceable; a policy for "the repo needs a newer toolchain than the environment provides" is not built. R009 needed supervisor prep to run one `go` command, and that gap remains.
