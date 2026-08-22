# Beta Release Readiness

> **Superseded — this is the audit that opened the Beta, kept as the record of
> what it found.** All four blockers it names are closed; the workspace version
> is `0.2.0-beta.1`, not the `0.1.4` this page audited, and the target is
> `0.2.0-beta.1`, not `1.0.0-beta.1`. For the current state read
> [`BETA_BLOCKER_RESOLUTION.md`](BETA_BLOCKER_RESOLUTION.md); nothing below is
> edited to match, because an audit that is rewritten to agree with its outcome
> stops being evidence.

**Audit date:** 2026-08-22 · **Audited tree:** `fix/ma-wa1-delegation-reliability` @ `4a7d6616`
· **`main`:** `b967f9fe` · **Workspace version:** `0.1.4` · **Target:** `1.0.0-beta.1`

**Verdict: `BETA_READY = NO`. `BLOCKER = 4`.** The product capabilities are ready.
The *release pipeline* is not: `main` has not had a green CI run since **2026-07-25**,
60 consecutive runs ago, and the beta candidate is not on `main` at all.

This is an audit. It changes no product code — see [Scope and method](#scope-and-method).

---

## Executive Summary

Every capability line the Beta definition names is finished and evidenced. What is
not finished is the part that turns finished capabilities into a release: a green
cross-platform pipeline, a candidate commit on `main`, and a distribution channel
that can serve a pre-release tag.

The gap is not a surprise about the software — it is a gap between *how the work was
validated* and *how it will be shipped*. Every closure gate in `docs/` validated
itself with a local `cargo test` run on macOS. None of them ran the pipeline that
actually produces the binaries users install. That pipeline builds Windows, and
**CodeLeveler has not compiled on Windows since 2026-08-11**.

Three consequences follow, and all three are cheap to fix:

1. `crates/leveler-execution/src/command.rs:995` calls a function with seven
   arguments that takes six, under `#[cfg(windows)]`. macOS and Linux never compile
   that line. It is one commit's oversight, not a design problem.
2. Four Linux test failures, of which three are a test-side platform assumption
   (a test asserts macOS's `sandbox-exec` while the runtime correctly selects
   `bwrap`), not a product defect.
3. `cargo-deny` rejects one path dependency that carries no version.

The fourth blocker is organisational: 41 commits, 281 files and +27.8k lines of the
beta candidate — late-bound write ownership, Web v2, the mobile client, the signed
remote RPC, the eval framework — exist only on a feature branch with no pull request,
so no CI job has ever seen them on any platform.

Fix those four and the release is a tagging decision. Nothing in this audit asks for
new features, new architecture, or more multi-agent work.

---

## Current Status

| # | Area | Verdict | Basis |
| --- | --- | --- | --- |
| 1 | Core agent runtime | **PASS** | 68 annotated tests whose names carry a session-lifecycle verb (resume / interrupt / cancel / restart / reconnect); `Interrupted` is a recorded terminal, not an inferred one; message, event and turn stores each have their own repo + store split (`crates/leveler-storage`, 13 migrations) |
| 2 | Long-running task | **PASS** | window ceiling, no-progress cap, hard bound, crash-window acknowledge/recover all have deterministic tests (`a_goal_spinning_across_windows_finally_stops`, `even_a_progressing_never_completing_goal_is_capped_by_the_hard_bound`); the D4 replay ran a goal across four windows end-to-end (`LONG_TASK_RELIABILITY_REPORT.md`) |
| 3 | Tool system | **PASS** | 43 registered tools (31 core + 12 browser) plus five agent-layer tools (`spawn_agent`, `claim_write_scope`, `report_finding`, `resolve_finding`, `update_goal`); five-level risk classification; MCP proxy tools prompt on **every** confined profile (`permission_rules.rs:325`); secrets sanitized at the single model-visible boundary before the model sees tool output (`executor/host.rs:562`) |
| 4 | Multi-agent | **PASS (execution)** | FS1–FS16 PASS, 13 safety counters 0, ~500 recorded runs; adoption is `NOT_GUARANTEED` **by decision**, not by defect ([`MA-WA1-FINAL.md`](evaluations/MA-WA1-FINAL.md)) |
| 5 | Provider layer | **PASS with ISSUE** | two wire protocols wired (`openai_chat`, `anthropic_messages`), retry with exponential backoff, four onboarding presets. **No cross-provider fallback exists** and the preset default models are stale — see R-6, R-7 |
| 6 | UI / clients | **PASS** | TUI closed (669 tests) with PTY-verified flows; Web v2 closed on Project → Sessions; mobile frozen at `mobile-beta-mvp`; `leveler remote` correctly labelled Unstable |
| 7 | Eval framework | **PASS with ISSUE** | two trees, both real: `crates/leveler-eval` + `evals/` (capability, with recorded baselines) and `eval/` (behaviour; `EVAL_SELFTEST = PASS`, 49 tests, run during this audit). **Neither runs in CI** — see N-1 |
| 8 | Documentation | **ISSUE** | user-facing claims verified accurate, including the CI platform claim *as a claim about configuration*; but several status headers are stale, the index omits the newest documents, and the quick start ignores the onboarding command the binary ships — see R-3 … R-5 |
| 9 | Release quality | **BLOCKER** | CI red on `main` for 28 days / 60 runs; Windows does not compile; candidate not on `main`; no pre-release distribution channel |

Workspace suite on the audited tree, macOS: **3139 passed · 1 failed · 6 ignored**
(the one failure is R-9). Linux and Windows are not covered by that number — that is
the whole of BLOCKER-1 and BLOCKER-2.

---

## Completed Capabilities

Shipped, evidenced, and not to be reopened for Beta:

| Capability | Evidence |
| --- | --- |
| Persistent session runtime — create, resume, interrupt, restart, fork, archive | 68 lifecycle-named tests; SQLite stores per aggregate; `completed_status_survives_a_restart`, `an_unanswered_approval_does_not_survive_a_restart` |
| Agent loop — typed tool calls, bounded retry, timeout, cancellation, process-tree kill | `cancellation_kills_grandchildren`, `cancellation_of_long_sleep_returns_within_bound`, `retryable_stream_error_exhausts_to_failed_after_max_attempts` |
| Long-task closure — goal windows, no-progress cap, hard bound, crash recovery | deterministic window tests; D4 four-window replay |
| Tool system — 43 registered tools (31 core + 12 browser) + 5 agent-layer tools, five risk levels, approval profiles, workspace boundary, checkpoints | `leveler-execution` (261 tests), `leveler-tools` (261 tests) |
| Secret safety at the model boundary | Gate 1 F6 closeout: durable + provider + model-visible occurrences of the test secret = 0 |
| Multi-agent execution — `spawn_agent`, child lifecycle, ownership isolation, `claim_write_scope`, settlement, durable provenance | FS1–FS16 PASS, 0 violations, 0 ownership denials |
| Verification and completion truth — environment-vs-code classification, closure review stage, honest terminals | Gate 2 + Batch #2 closeouts; Recovery Truth 4/4 |
| Browser coding workflow | Gate 6: 12 structured tools, `NO_PRODUCT_CHANGE` |
| Clients — TUI, Web v2, mobile MVP, remote bridge | respective closure documents; `leveler-tui` 669 tests, web-ui CI job green |
| Supply chain and platform policy — `cargo-deny` + `cargo-audit` configured, three OS matrix, Windows security canaries as their own step | `.github/workflows/ci.yml` |

---

## Remaining Blockers

Four. All four are pipeline or assembly, none is a product capability.

### BLOCKER-1 · CodeLeveler does not compile on Windows

`crates/leveler-execution/src/command.rs:995` passes seven arguments to
`run_windows_dispatch`, defined at line 1027 with six. The extra argument is the
`chunks` streaming sender added by `858b1b64` ("Run user shells on the same host
substrate the agent uses — streaming", 2026-08-11): the Unix path took the new
parameter, the `#[cfg(windows)]` path did not.

- Present on `main` **and** on the beta candidate branch. Verified in the working tree, not only in the log.
- CI evidence: run `32150249203`, `error[E0061] ... could not compile leveler-execution`.
- Post-dates the last release: `v0.1.4` (2026-07-25) has the six-argument call and is unaffected. Users on a published binary are fine; anything built from `main` since 2026-08-11 is not.
- The release workflow builds `x86_64-pc-windows-msvc`. Tagging today produces **no Windows artifact**.
- Consequence beyond the build: because the job dies at compile, the Windows security canaries (NTFS junction behaviour, AppContainer ACLs, Job Object descendant termination) have not run in any recent CI run. The Windows boundary is currently **unproven**, not merely unbuilt.

**Fix direction:** thread `chunks` into `run_windows_dispatch` and on to
`run_with_windows_job` / `run_appcontainer`, or drop it explicitly with a stated
reason (Windows has no live streaming yet). Then let the Windows job run to
completion — the first error may not be the only one.

### BLOCKER-2 · CI on `main` has been red for 28 days

Last green run on `main`: **2026-07-25**, `fix(memory): collapse nested ifs for clippy -D warnings`. Every one of
the 60 runs since has failed. The README carries a public CI badge, so this is
visible to anyone who opens the repository.

At the current `main` HEAD (run `32150249203`):

| Job | Result | Cause |
| --- | --- | --- |
| macOS | ✓ | — |
| Windows | ✗ | BLOCKER-1 |
| Ubuntu | ✗ | 4 test failures, below |
| mobile | ✓ | — |
| web-ui | ✓ | — |
| deny · audit | ✗ | BLOCKER-3 |

Ubuntu failures:

- `command::tests::seatbelt_confines_writes_via_params`, `seatbelt_denies_reads_of_declared_host_roots`, `sealed_roots_survive_the_unconfined_execution_path` — assert `/usr/bin/sandbox-exec` while the runtime correctly selects `bwrap` on Linux (`command.rs:3169`, `left: "bwrap"`, `right: "/usr/bin/sandbox-exec"`). **Test-side platform assumption; the product picked the right sandbox.** Gate them per platform or assert the selected backend rather than a macOS path.
- `hard_gate_shell_output_never_enters_model_context` (`crates/leveler-app/tests/user_shell.rs:200`) — `Runtime("session … already has an active turn")`. This one is *not* obviously a platform artefact: it is either a test that reuses a session across turns or a real serialization edge. It guards a security property (`!command` output must never reach the model), so it must be understood, not just made green.

**Why this was not caught:** every closure gate in `docs/` validated with a local
`cargo test` on macOS — the one platform that passes. The rule that follows is in
[Standing corrections](#standing-corrections).

**Documentation consequence:** the README states *"Windows, macOS, and Linux are
tested in CI."* The configuration says so and the workflow really does run all
three, but two of the three have not passed in a month, and the badge next to that
sentence says `failing`. Fixing the pipeline makes the sentence true again; leaving
it red makes the sentence a claim the repository's own badge contradicts.

### BLOCKER-3 · `cargo-deny` rejects the dependency graph

`crates/leveler-remote-protocol/Cargo.toml:36` declares
`leveler-session-wire = { path = "../leveler-session-wire" }` with no `version`.
With `wildcards = "deny"` (`deny.toml:57`), cargo-deny reports *"found 1 wildcard
dependency for crate 'leveler-remote-protocol'"* — reproduced in CI runs
`31012817303` (2026-08-05) and `32150249203` (2026-08-18), so it has been failing
for at least 17 days.

**Fix direction:** use the workspace dependency (`{ workspace = true }`) as every
other internal crate does, or add `version = "0.1.4"` alongside the path.

Same job, separate signal: `warning[advisory-not-detected]` for `RUSTSEC-2023-0071`
— the ignore no longer matches any crate in the graph. It should be removed rather
than left to expire on 2026-10-01.

### BLOCKER-4 · The beta candidate is not on `main` and has never been CI-verified

`main` is `b967f9fe`. The audited branch is **41 commits / 281 files / +27,827 −1,688**
ahead of it, and carries product code Beta depends on:

late-bound child write ownership and the atomic parent fence · the MCP ownership
bypass fix · Web v2 desktop shell and session-wide agent projection · the mobile
runtime control client · the signed `fetch_attachment` RPC · the durable runtime
observatory · the eval experiment framework.

`gh pr list` shows no pull request for this branch. CI triggers on push to
`main`/`master` and on `pull_request` only, so **not one of these 41 commits has been
compiled or tested by CI on any platform.**

Meanwhile several documents in `docs/` still state `OPEN_BETA_BLOCKER = 0`. Those
statements were true at their own baselines and were never wrong; they simply do not
describe this tree. This document supersedes them for release purposes.

**Fix direction:** open a PR for the branch, let CI run it, fix what it finds
(BLOCKER-1 … 3 will surface there), merge to `main`, confirm green, then tag.

---

## Required Before Beta

Not release-stopping on their own; each will embarrass the release if skipped.

| # | Item | Evidence | Fix |
| --- | --- | --- | --- |
| R-1 | **No pre-release distribution channel.** `install.sh:40` resolves `/releases/latest`, which never returns a pre-release; the Homebrew formula's `livecheck` uses `strategy :github_latest`; `release.yml` creates a draft with no `prerelease:` flag. Publishing `1.0.0-beta.1` as a pre-release leaves both install paths silently serving `0.1.4`; publishing it as *latest* pushes a beta onto every stable user. | `install.sh`, `packaging/homebrew/leveler.rb`, `.github/workflows/release.yml` | Decide the channel, then make it real: either a `LEVELER_VERSION` override in `install.sh` + an explicit beta install line in the README, or accept "beta is latest" deliberately and say so |
| R-2 | **Version identity is inconsistent.** Workspace is `0.1.4`; the Homebrew formula is pinned to `0.1.0` with placeholder SHA256s; the README's manual-download example says `V=0.1.0`. | `Cargo.toml:38`, `packaging/homebrew/leveler.rb:4`, `README.md` | Bump the workspace to the release version; regenerate the formula with `update-formula.sh` after publishing; update the README example |
| R-3 | **`STABILITY.md` is still DRAFT.** It says so in its own banner and its D1/D2/D3 decisions are marked *awaiting maintainer sign-off*. It is linked from `CONTRIBUTING.md` but not from `README.md`. A beta whose promise about CLI and config stability is a draft has no promise. | `docs/STABILITY.md:1–10` | Owner decision on D1–D3 (recommendations are already written), flip the banner to ADOPTED, link from the README |
| R-4 | **The quick start ignores the onboarding the binary ships.** `leveler login` presents four provider presets and writes the config; the README instead has the user hand-write `~/.leveler/config.toml`. | `crates/leveler-cli/src/login_cmd.rs:156`, `README.md` §2 | Lead with `leveler login`, keep the manual config as the explicit alternative |
| R-5 | **Doc index and status headers have drifted.** `docs/README.md` lists neither `ROADMAP.md`, `STABILITY.md`, `multi-agent.md` nor `evaluations/MA-WA1-FINAL.md`. `HOME_RUNTIME_HARDENING_REPORT.md` still says "not merged; awaiting human review" although it merged as PR #9 on 2026-08-13. | `docs/README.md`, `docs/HOME_RUNTIME_HARDENING_REPORT.md:3` | Index the four; add a superseded/merged banner where a status line is stale. (This audit added its own index entry only — the other four are left for the owner.) |
| R-6 | **Provider presets suggest outdated models.** `gpt-4o` for OpenAI and `claude-sonnet-4-5` for Anthropic; both are behind their vendors' current lines. A first-run user gets a stale default without being told. | `crates/leveler-provider/src/presets.rs:62–80` | Refresh `suggested_model` / `suggested_context`; the file's own doc comment already says never to present these as verified |
| R-7 | **No cross-provider fallback.** Retry with backoff exists inside one provider; there is no secondary provider or model on failure. Not a defect — an absent feature that the README does not promise — but a long unattended run dies with its provider. | `crates/leveler-provider/src/transport.rs` | Decide explicitly: document the limit for Beta, or schedule it post-Beta |
| R-8 | **Binaries are unsigned.** Not notarized on macOS, not code-signed on Windows; the README documents the Gatekeeper/SmartScreen workaround. Acceptable for a beta *if* it is a stated decision rather than an omission. | `README.md` first-run section | Confirm as a Beta-accepted limit, or start the signing setup now — it has lead time |
| R-9 | **A browser boundary test fails under full-workspace parallelism, twice.** `loopback_ws_from_a_granted_dev_page_connects` (`crates/leveler-browser/tests/reliability.rs:714`) returned `WS_SELF_BLOCKED` where `WS_SELF_OPEN` is required, in both full runs of this audit; run in isolation the same target passes 11/11 in 7 s. Note what it guards: a granted loopback dev page reaching its **own-origin** websocket while all other egress stays gated — the vite-HMR class that `GATE_6_BROWSER_CLOSURE.md` already records as `R004-F6`, *fixed but never re-observed in production*. So the one browser capability recorded as unproven is also the one whose test is unstable under load. | this audit, two full runs + one isolated run | Diagnose before dismissing: if it is harness contention (ports, browser startup under load), serialize the target; if the grant races the connect, that is a real defect on a security-adjacent boundary |

---

## Post Beta Items

Already recorded elsewhere with their evidence; repeated here so the Beta decision
is made against a complete list. Nothing here blocks the release.

| Item | Source |
| --- | --- |
| Delegation Advisor — change the decision surface, measure `appropriate_delegation_rate`, not spawn rate | `ROADMAP.md` Phase 1 · `design/DELEGATION_ADVISOR_DESIGN.md` |
| `SubAgentProvider` seam; durable child sessions (a resumed run reports children lost and releases their scopes); background-first multi-agent UX; capability negotiation | `ROADMAP.md` Phases 2–5 |
| Web has no multi-agent chrome (`MAPC-W1`); explorer has no browser-read tools (`MAPC-B1`) | `MULTI_AGENT_PRODUCT_CLOSURE.md` |
| Sub-agent token totals not durable after restart; model TTFT / request id not recorded on tools | `DURABLE_RUNTIME_OBSERVATORY.md` |
| `R012-F1` durable-args truncation — `OPEN_EVIDENCE_NEEDED` | `BATCH_02_TARGETED_REPAIR_CLOSEOUT.md` |
| `R004-F6` vite websocket through the forward proxy — fixed but never re-observed, recorded as unproven | `GATE_6_BROWSER_CLOSURE.md` |
| Cross-client resume of an unfinished goal; goal-owned process survival — `NOT_OBSERVED` (driver limitation) | `BATCH_02_TARGETED_REPAIR_CLOSEOUT.md` |
| `m-1` coarse "new distinct file path" no-progress proxy | `LONG_TASK_RELIABILITY_FINAL_REVIEW.md` |
| `N8` build artefacts in `modified_files` — `DEFER_POST_BETA` | `BETA_CLOSURE_FINALIZATION.md` |
| `R013r-M1` flash's reviewer reads wide diffs without concluding inside its bound | `BATCH_02_TARGETED_REPAIR_CLOSEOUT.md` |
| Windows durable daemon transport (embedded mode remains) | `RUNTIME_EVOLUTION_PLAN.md` |

---

## Nice to Have

| # | Item | Why it is only *nice* |
| --- | --- | --- |
| N-1 | Neither eval tree runs in CI, so regression detection on agent behaviour is manual and depends on someone remembering. `eval/scripts/selftest.sh` is fast, hermetic and model-free — it could be a CI step today; the model-calling suites cannot. | Beta ships on capability gates, not on eval automation |
| N-2 | No prebuilt `aarch64-unknown-linux-gnu`; ARM Linux users build from source. `install.sh` says so cleanly instead of failing obscurely. | Small audience, honest error |
| N-3 | Bundled model profiles (`configs/models/`) cover DeepSeek and Kimi only. Presets are self-contained, so OpenAI/Anthropic users are not blocked. | Cosmetic asymmetry |
| N-4 | Migration numbering jumps 0006 → 0012. | Harmless if intentional; confusing to a new contributor |
| N-5 | `docs/` holds ~120 internal gate reports next to a dozen user-facing documents, unindexed. | Internal record, not user-visible harm |

---

## Release Recommendation

**Do not tag `1.0.0-beta.1` yet.** Not because the product is unready — because a tag
today builds no Windows binary, ships from a commit that lacks the candidate work,
and lands on a pipeline that has been red for 28 days.

The path is short and mechanical:

1. **Make the pipeline honest.** Open a PR for `fix/ma-wa1-delegation-reliability`, let CI run it on all three platforms. Fix BLOCKER-1 (Windows arity), BLOCKER-2 (three platform-gated test assertions + one real active-turn failure to understand), BLOCKER-3 (the versionless path dependency, plus drop the no-longer-matching RUSTSEC ignore). Merge only on green.
2. **Settle the release identity.** R-1 and R-2 together: pick the pre-release channel, bump the workspace version, and make sure `install.sh` and Homebrew can actually serve what you publish. Do a dry-run tag on a throwaway version if that is cheaper than reasoning about it.
3. **Sign off the promises.** R-3 (STABILITY D1–D3) and R-4 (quick start via `leveler login`). Both are decisions plus small edits.
4. **Tag, publish, install from the published artifact on each platform.** The install path is part of the product; verify it the way a user would, including Windows.

Steps 1 and 2 are the release. Steps 3 and 4 are what makes it a *beta* rather than a
build. Nothing on this list requires new features, and nothing requires reopening
multi-agent work.

**What Beta claims, once tagged:** a local-first coding agent runtime with a **secure
Multi-Agent Runtime** — reliable, isolated execution whenever the model elects to
collaborate. It does not claim automatic multi-agent orchestration, and it does not
promise that the agent splits tasks on its own
([`MA-WA1-FINAL.md`](evaluations/MA-WA1-FINAL.md)).

---

## Standing corrections

Two rules this audit earned, both from the same root cause:

1. **A gate that never ran the release pipeline has not measured release readiness.** Every `OPEN_BETA_BLOCKER = 0` in `docs/` was computed from a local macOS `cargo test`. That is a real signal about the code and no signal at all about Windows, Linux, or the supply-chain gate. A closure claim should name the platforms its evidence covers.
2. **Red CI is not background noise.** 60 consecutive failures were survivable only because nobody's work depended on the badge being green — which is precisely how a month-old Windows regression stays invisible. Before the next gate opens, `main` should be green and stay green.

## Scope and method

**Method.** Static audit of the working tree, the git history, the GitHub Actions
history (`gh run list/view` over the last 60 runs on `main`), plus two things
actually executed during the audit: `eval/scripts/selftest.sh` (`EVAL_SELFTEST =
PASS`, 49 tests) and a full local `cargo test --all-targets --no-fail-fast`.

**Local test result** (macOS 15.5, arm64, `4a7d6616`, two full runs). Second run,
full output captured: **3139 passed · 1 failed · 6 ignored**, exit 101. Exactly one
target failed in both runs — `-p leveler-browser --test reliability`, and within it the
single test `loopback_ws_from_a_granted_dev_page_connects`. Run in isolation the
target passes 11/11 in 7.1 s. Every other target in the workspace passed. The
failure is reproducible under parallel load and disappears without it; it is
recorded as R-9 rather than waved off, because of what it guards.

**What this audit did not do.** It ran no model-calling evaluation, no PTY session,
no browser task and no mobile pairing. Statements about those flows are inherited
from their own closure documents and are cited as such — not re-proved here. It also
did not attempt to fix anything: every defect above is recorded with a file, a line
and a fix direction, and left in place.

**Git constraint honoured.** This audit modified `docs/` only. `crates/`, `apps/`,
`eval/`, `evals/` and every workflow file are byte-identical to `4a7d6616`.
