# Beta Blocker Resolution

**Date:** 2026-08-22 · **Branch:** `main` · **Base:** `c37a6cd4`
· **Audit this answers:** [`BETA_RELEASE_READINESS.md`](BETA_RELEASE_READINESS.md)

**The four audit blockers are closed. `READY_FOR_BETA_RELEASE` is not YES yet** —
see [the decision](#beta-release-decision) for the two undiagnosed test failures
that still stand between here and a green pipeline.

Closing the blockers turned up more than the audit could see, because CI had
been dying at the first compile error for a month and nothing behind it had ever
run: **three further Windows defects** behind the first one, then, once the
Windows job finally reached its test suite, **twelve failures across six
targets** — eleven now fixed, including a real stack overflow in `leveler
completions` and a confined `!command` that printed nothing at all.

Verification is stated per fix, and so is its boundary. Cross-platform work was
verified locally with a cross-compiler and a Linux container; GitHub Actions is
the authority, and where it has now spoken, its verdict is quoted rather than
predicted.

---

## Fixed Blockers

### BLOCKER-1 · Windows did not compile — four defects, not one

**Root cause.** Every one of them lives behind `#[cfg(windows)]` or
`#[cfg(not(unix))]`, so neither a macOS nor a Linux compiler ever type-checked
it. They accumulated because the only machine that would have complained — the
Windows CI job — had been failing at the first of them since 2026-08-11 and
never reached the rest.

| # | Defect | Where | Why it was there |
| --- | --- | --- | --- |
| 1 | `run_windows_dispatch` called with 7 arguments, defined with 6 | `leveler-execution/src/command.rs:995` vs `1027` | `858b1b64` added the `chunks` streaming sender to the Unix path only |
| 2 | `read_only_workspace` unused on non-Unix | `leveler-execution/src/command.rs:365` | the `let _ = (…)` tuple was not extended when the parameter was added — a warning, and CI runs `clippy -D warnings` |
| 3 | `context.checkpoint` on a field that moved to `context.execution.checkpoint` | `leveler-tools/src/tools/replace.rs:620` | the `#[cfg(not(unix))]` commit path was missed by the `ToolContext` split |
| 4 | Unix-socket stub carried a copy of the real `local_waiter_count` body | `leveler-local-transport/src/lib.rs:1554` | referenced `WireRequest` / `WireResponse` / `transport_client_error`, none of which exist in the stub module |
| 5 | Unix-only `ProjectRouter` imported unconditionally | `leveler-cli/src/remote_cmds.rs` + `leveler-remote-agent` | `leveler remote` could not compile where the daemon socket does not exist |

**Code change.**

- `chunks` is threaded into `run_windows_dispatch` and on to
  `run_with_windows_job`, which reads its pipes exactly the way the Unix path
  does — so **`!command` now streams live on Windows** on the unconfined path.
  The AppContainer path (`ReadOnly` / `WorkspaceWrite`) does not stream: `rappct`
  hands back synchronous readers and giving it a live view means restructuring
  that launcher, not threading an argument. The sender is dropped there
  deliberately, which closes the channel — a consumer sees "no live view", never
  a stall. Stated in the dispatch, in `CHANGELOG.md`, and in `README.md`.
- `replace`'s non-Unix commit path uses `context.execution.checkpoint`, matching
  its Unix sibling twenty lines above.
- The transport stub answers `local_waiter_count` the way every other method on
  it answers: unavailable on this platform. Both callers already read that
  correctly — `remote-agent` approvals treat the error as "assume a person is
  watching", project reattach treats it as "this handle is dead".
- `leveler remote projects` / `agent` are Unix-only and refuse with a named
  message elsewhere, instead of being a compile error. `ProjectRouter`,
  `AgentBridge`, `run_tunnel`, `Arc`, `registry_path`, `Registry` and
  `display_for` are gated to match their only users.

**Test result.**

```
cargo clippy --target x86_64-pc-windows-gnu --workspace --all-targets --all-features
    Finished — 0 errors, 0 warnings
```

Before the fix, the same command reproduced CI exactly: `error[E0061]: this
function takes 6 arguments but 7 arguments were supplied`, plus the
`read_only_workspace` warning that would have failed the next run.

**Boundary.** That is the **`windows-gnu`** target, using the mingw-w64 C
toolchain, because macOS cannot host an MSVC C compiler. `cfg(windows)` is true
for both ABIs and the Rust code is identical, so every defect above is genuinely
fixed; what a gnu check cannot prove is an MSVC-only linkage or crate-feature
difference. The `windows-latest` CI job is the authority, and it now gets to run
for the first time in a month.

**Also true and not fixed here:** because the Windows job died at compile, its
security canaries (NTFS junctions, AppContainer ACLs, Job Object descendant
termination) have not executed in any recent run. They are not "passing" — they
are *about to run*. See Remaining Risks.

### BLOCKER-2 · Linux CI — two different problems wearing one uniform

**Root cause, part 1 (three tests): the test asserted an implementation, not a
property.** `seatbelt_confines_writes_via_params`,
`seatbelt_denies_reads_of_declared_host_roots`,
`sealed_roots_survive_the_unconfined_execution_path` — and
`read_denials_do_not_swallow_the_sandboxes_own_writable_roots`, which the audit
missed — read an SBPL policy string and assert `/usr/bin/sandbox-exec`. On Linux
the runtime correctly selects `bwrap`, so the *product* was right and the tests
were wrong.

Underneath that, a fact worth more than the fix: **the Linux backend has no read
denial at all.** `linux_sandbox_command` does not take `read_denied_roots`, and
bwrap's `--ro-bind /` grants reads rather than withholding them. The harness read
seal (C2.3C-S measurement integrity) is macOS-only.

**Code change.** The SBPL tests are `#[cfg(target_os = "macos")]` and say why.
Two new tests carry the property instead of the dialect:

- `confinement_selects_the_platform_backend` — asserts *which* backend each
  platform gets (`sandbox-exec` / `bwrap` / bare) and that the command survives
  the wrapping. Runs everywhere.
- `read_denials_are_a_macos_only_seal_today` — Linux-only, pins the gap so it is
  visible in code rather than implied by an absence. Its failure is the signal
  that Linux grew a read seal.

The Linux write-confinement property was already covered platform-independently
by `bwrap_confines_writes_to_roots`, which is why nothing was lost by gating the
SBPL ones.

**Root cause, part 2 (`hard_gate_shell_output_never_enters_model_context`): a
real race in the product.** The user-shell worker did this:

```
store.finish(...)                       // execution marked finished
log.append(UserShellFinished, forward)  // → client sees UserShellExited
active.finish(&session_id)              // ← turn slot released, AFTER
```

So a client that enables its composer on `UserShellExited` — which is what the
TUI and Web do — can submit the next message into a session that is still
holding its turn slot, and get `session … already has an active turn`. The test
does exactly what a user does. macOS usually wins the race; Linux under CI load
loses it.

**Code change.** `active.finish()` moves ahead of the terminal event. Nothing
below it needs the slot: the process has exited, output is drained, and
`store.finish` has already made `CancelUserShell` a no-op. This also matches the
failure paths in the same function, which already released before notifying.

**Test result.** Reproduced and verified in a Linux container (`rust:1.90`,
bubblewrap installed, the CI shape):

| Run | Result |
| --- | --- |
| before any fix | `hard_gate_shell_output_never_enters_model_context` FAILED at `user_shell.rs:200`, same line and message as CI run `32150249203`; 4 SBPL tests FAILED |
| after the fixes | `leveler-execution --lib` **216 passed / 0 failed** · `leveler-app --test user_shell` **9 passed / 0 failed**, `hard_gate_shell_output_never_enters_model_context` among them |

New regression test `the_session_is_free_the_moment_the_shell_reports_exit`
asserts the invariant directly — ten shell rounds, `runtime_info().health
.active_turns == 0` immediately after each exit event, no model call. It passes
on macOS both before and after the fix, which is the honest description of a
race with a microsecond window; the container is where it is decided.

### BLOCKER-3 · cargo-deny

**Root cause.** `leveler-session-wire` was declared by path with no version in
`leveler-remote-protocol`'s dev-dependencies. Under `wildcards = "deny"`
cargo-deny reads a versionless path dependency as a wildcard. Separately, the
`RUSTSEC-2023-0071` ignore no longer matched any crate: cargo-deny resolves
advisories against the dependency *graph*, and `rsa` is not in it on any target.

**Code change.** The dev-dependency uses the workspace declaration, like every
other internal crate. The advisory ignore is deleted, with a note recording that
`cargo audit` — which reads the *lockfile*, where `rsa` still sits as an optional
`sqlx-mysql` edge — still needs its own `--ignore`.

**Test result.**

```
cargo deny check
  before:  advisories ok, bans FAILED, licenses ok, sources ok
  after:   advisories ok, bans ok, licenses ok, sources ok
```

### BLOCKER-4 · The candidate is on `main`

**Root cause.** 41 commits of product work sat on `fix/ma-wa1-delegation-reliability`
with no pull request, and CI triggers only on push-to-`main` and `pull_request`.

**Change.** Merged with `--no-ff` as `c37a6cd4` and pushed
(`b967f9fe..c37a6cd4`). Every fix above is on `main` too, so CI sees the
candidate rather than a month-old ancestor.

---

## Release Quality

### Version

The audit asked for `v0.1.0-beta.1`. That string is **lower** than the published
`v0.1.4` (`0.1.0-beta.1 < 0.1.0 < 0.1.4`), so `leveler upgrade`, Homebrew and any
SemVer comparison would read the beta as a downgrade — which would defeat the
pre-release channel requested in the same breath. The beta is therefore
**`0.2.0-beta.1`**: the next minor of the 0.x line, correctly ordered after
`0.1.4`. Changing it is one line in the workspace manifest if the owner prefers
another string.

| Where | Before | After |
| --- | --- | --- |
| workspace + 30 internal dependency pins (`Cargo.toml`) | `0.1.4` | `0.2.0-beta.1` |
| `Cargo.lock` | `0.1.4` | `0.2.0-beta.1` |
| `packaging/homebrew/leveler.rb` | `0.1.0` (two releases stale) | `0.1.4` — the tap tracks **stable**, and now says so |
| `README.md` download example | `V=0.1.0` | `V=0.1.4` |
| `README.md` beta install | — | `LEVELER_VERSION=v0.2.0-beta.1 … install.sh` |

### Install channel

A beta must not arrive on a machine that did not ask for it, and must be
reachable by one that did. Both halves are now real:

- `install.sh` takes `LEVELER_VERSION=v0.2.0-beta.1` to pin a tag. Without it,
  the script resolves `releases/latest`, which never returns a pre-release.
- `release.yml` marks any tag carrying a SemVer pre-release part as a GitHub
  pre-release (`prerelease: ${{ contains(github.ref_name, '-') }}`), so
  `releases/latest`, the installer and the Homebrew tap keep serving the last
  stable build.
- `README.md` documents the one-line beta install next to the stable one.

The Homebrew formula stays on the stable line and still carries placeholder
digests; `packaging/homebrew/update-formula.sh <tag>` fills them from a
*published* release, which is the only point at which the real digests exist.

### Documentation

- `docs/STABILITY.md` is **ADOPTED** (D1/D2/D3 signed off as their recommended
  answers) and linked from `README.md`. A beta that cannot say what will keep
  working is asking for trust it has not offered anything for.
- `README.md`'s quick start leads with `leveler login` — the onboarding the
  binary already shipped, which asks the provider which models the key can
  actually reach — with hand-written `config.toml` kept as the explicit
  alternative for gateways outside the four presets.
- The Public beta section names the two platform gaps a user can hit: no daemon
  socket transport on Windows, and no live `!command` streaming for a *confined*
  Windows command.
- `docs/README.md` indexes what it was missing: `STABILITY.md`, `ROADMAP.md`,
  `multi-agent.md` (+ zh), `MA-WA1-FINAL.md`, and both Beta documents.
- The stale download example (`V=0.1.0`) now names the version that is actually
  published.

---

## Verification

Every gate CI runs, run here first.

| Gate | Command | Result |
| --- | --- | --- |
| Format | `cargo fmt --all -- --check` | **PASS** — and it caught three files the merge brought in unformatted (`leveler-media`, `remote-agent/attachments`, `remote-agent/bridge`), which would have failed CI's fmt step on `main` |
| Lint (host) | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | **PASS**, 0 warnings |
| Lint (Windows) | same, `--target x86_64-pc-windows-gnu` | **PASS**, 0 errors 0 warnings — was `error[E0061]` before |
| Tests (host, macOS arm64) | `cargo test --workspace --all-targets --all-features --locked --no-fail-fast` | **3146 passed · 0 failed · 6 ignored** (3145/0 before the second wave of Windows fixes) |
| Tests (Linux, container) | `rust:1.90` + bubblewrap, `-p leveler-execution --lib -p leveler-app --test user_shell` | **216 / 0** and **9 / 0** — the four SBPL failures and the hard-gate failure are gone |
| Supply chain | `cargo deny check` | **PASS** — `advisories ok, bans ok, licenses ok, sources ok` |
| Installer | `sh scripts/test_install.sh` | **PASS**, including two new pinned-version canaries (verified red without the pin support) |
| Manifest | `cargo metadata --no-deps` after the version bump | **PASS** |

**CI result — run `32552025802`, the first run in a month that got past
compiling.** This is the part worth reading, because it is where the local
evidence stopped and the real pipeline started.

| Job | Before this work | After |
| --- | --- | --- |
| `deny · audit` | ✗ wildcard dependency | **✓** |
| `leveler-web UI` | ✓ | ✓ |
| `leveler-mobile` | ✓ | ✓ |
| `fmt · clippy · test (ubuntu)` | ✗ 4 test failures | **✓ green** |
| `fmt · clippy · test (windows)` | ✗ died at compile | Format ✓ · Clippy ✓ · **security canaries ✓** · Test ✗ |
| `fmt · clippy · test (macos)` | ✓ (pre-merge) | ✗ one test |

Ubuntu is green for the first time since 2026-07-25. The Windows job compiled,
linted, and **ran its security canaries — NTFS junction behaviour, AppContainer
ACLs, Job Object descendant termination — which passed**. That is the evidence
Remaining Risk 1 was written to worry about, and it arrived.

Then Windows ran its test suite, which had not executed in over a month, and
found **twelve failures across six targets**. That is not a regression from this
work; it is a month of Windows test debt becoming visible for the first time.
Triaged and fixed in the same session (commit `fad465fd`):

| Failure | Nature | Fix |
| --- | --- | --- |
| `leveler completions` × 3 — `thread 'main' has overflowed its stack` | **Real defect.** Windows reserves 1 MB of main-thread stack where Unix gives 8 MB; generating a completion script walks clap's whole command tree | reserve 8 MB for both Windows targets in `.cargo/config.toml` |
| `hard_gate_user_shell_makes_zero_model_requests` | **Real defect, and mine.** A confined `!command` produced *no output at all*: the AppContainer path dropped the chunk sender, and the user-shell view is built from chunks alone | emit the captured output as one chunk at completion |
| `shell_runs_in_the_repository_root` (`pwd`), `snapshot_restores_active_shell_with_elapsed` (`sleep 5`), `approval_resolution_is_durable_before_dispatch` (`rm -rf`) | POSIX commands in platform-neutral tests | spell the intent the way the host spells it (`cd`, `ping -n`, `rmdir /s /q`) |
| `declared_read_denials_are_parsed_from_the_environment_value`, `no_declared_denials_leaves_the_read_policy_untouched` | assert a colon-separated POSIX-absolute format, and index a policy argument that AppContainer does not produce | Unix-gated, with the reason stated |
| `unmet_rust_requirement_is_environment_not_code_failure`, `unmet_go_requirement_is_environment_not_code_failure` | the runner's cargo cannot create `C:\Users\runneradmin\.rustup`, so it never reads the requirement it is meant to be refused by | skip when the toolchain cannot start, as they already skipped when it was absent — **and** teach the classifier that a compiler which cannot create its home is an environment failure, never a verdict on the code |
| `first_message_retitles_a_placeholder_session` — turn did not settle within 30 s | **Not diagnosed.** Could be a slow runner or a real hang on the retitle path | open, see Remaining Risks |

Note on the audit's R-9 (`loopback_ws_from_a_granted_dev_page_connects`): it did
**not** recur in the 3145-test host run. It was reproducible twice before and
is untouched by this work, so it is recorded as still-open contention rather
than fixed.

**What was not run.** No model-calling evaluation, no PTY session, no browser
task, no mobile pairing, and no Windows *execution* of anything — a cross
compiler type-checks, it does not run tests. Those are CI's job and the
release's own smoke test.

---

## Remaining Risks

| # | Risk | Why it is acceptable for Beta | What would change that |
| --- | --- | --- | --- |
| 0a | **`duration_budget_stops_the_run_between_rounds` fails on the macOS runner** and passes locally (5/5, and inside the 3145-test suite). Reproduced twice on CI, so it is deterministic there, not a flake. The test gives a run a 10 ms duration budget and a `sleep 0.05` tool call, then requires round 2 never to be requested — it holds only if round 1 really takes longer than 10 ms. It arrived with the 41 merged commits, which include two agent changes to round and outcome accounting. | **Not acceptable — it is the top open item.** It is listed as a risk rather than a fix because guessing at a budget test without being able to reproduce it would be the same mistake this whole document is about. | Reproducing it: run that single test on a macOS runner, or bisect the two agent commits that touch round accounting. Either the budget check moved relative to the tool call (a real regression) or the test was always margin-based and got unlucky on slower hardware (a test defect). Both are cheap once reproduced. |
| 0b | **`first_message_retitles_a_placeholder_session` times out at 30 s on Windows.** The turn never settles; a tokio worker also panics with "context was found, but it is being shutdown". | Same reasoning as 0a: undiagnosed, and it is the last Windows target still failing. | Running that one test on a Windows box. If the retitle path hangs rather than the runner being slow, it is a real defect on the client's very first message. |
| 1 | ~~**Windows security canaries have not run in a month.**~~ **Closed by run `32552025802`: they ran and passed.** NTFS junction behaviour, AppContainer ACLs, Job Object descendant termination are proven again on a real Windows runner. | — | — |
| 2 | **`windows-gnu` is not `windows-msvc`.** | The Rust behind `cfg(windows)` is identical for both ABIs; the difference is C toolchain and linkage. | An MSVC-only compile error in CI. |
| 3 | **No live `!command` streaming for confined Windows commands.** | Output still arrives, complete, on completion; documented in three places rather than discovered. | Windows users reporting it as broken rather than limited. |
| 4 | **The Linux backend enforces no read denial.** The eval harness cannot seal an answer key on Linux. | Evals run on macOS today, and this is measurement hygiene, not a user-facing security boundary. | Running the eval harness on Linux and trusting its numbers. |
| 5 | **`leveler-browser --test reliability` is flaky under full-workspace parallelism** (`loopback_ws_from_a_granted_dev_page_connects`, R-9 in the audit). Untouched here. | It passes 11/11 in isolation; the failure is contention, and the capability it guards is separately recorded as unproven in production. | It failing in isolation, or CI failing on it — at which point it is a defect, not contention. |
| 6 | **Preset default models are dated** (`gpt-4o`, `claude-sonnet-4-5`). | `leveler login` asks the provider for the real model list; the preset is only the fallback for gateways with no `/models` endpoint. | A user on such a gateway getting a model their key cannot reach. |
| 7 | **No cross-provider fallback.** A long unattended run dies with its provider. | Not promised anywhere; retry-with-backoff within a provider exists. | Beta feedback that unattended runs die on provider blips. |

---

## Beta Release Decision

```
THE FOUR AUDIT BLOCKERS = CLOSED
  WINDOWS_BUILD         = FIXED   (CI: compile ✓ clippy ✓ security canaries ✓)
  LINUX_CI              = GREEN   (CI: ubuntu job green, first time since 2026-07-25)
  SUPPLY_CHAIN          = GREEN   (CI: deny · audit green)
  CANDIDATE_ON_MAIN     = YES
VERSION                 = 0.2.0-beta.1
PRE_RELEASE_CHANNEL     = READY

READY_FOR_BETA_RELEASE  = NOT YET — two test failures stand between here and a
                          green pipeline, and neither is diagnosed.
```

**Why not YES.** The four blockers this document was asked to close are closed,
and three of them are confirmed by the pipeline itself rather than by local
proxies. But letting CI run — the whole point of the exercise — surfaced two
failures that were invisible before, and honesty about them is worth more than a
green-sounding verdict:

- `duration_budget_stops_the_run_between_rounds` on macOS (risk 0a), which came
  in with the merged branch and reproduces only on the runner.
- `first_message_retitles_a_placeholder_session` on Windows (risk 0b), the last
  of twelve Windows failures, eleven of which are fixed.

Both are single tests with a clear next step, and both need a machine this
session does not have. Neither is a reason to reopen a blocker; both are a
reason not to claim `YES` yet.

**What that means practically.** The Beta is one green CI run away, not one
project away. The path:

1. Reproduce 0a on a macOS runner — or bisect the two agent commits that touch
   round accounting. If the budget check moved relative to the tool call, that
   is a real regression and worth finding; if the test was always margin-based,
   make its margin honest instead of lucky.
2. Run 0b's single test on Windows and find out whether the retitle path hangs
   or the runner is just slow.
3. Green run → tag `v0.2.0-beta.1` → publish as a pre-release → verify the
   install path on each platform the way a user would.

**What ships when it does:** a local-first coding agent runtime with a **secure
Multi-Agent Runtime** — reliable, isolated execution whenever the model elects
to collaborate — with the surfaces it promises marked Frozen, the ones still
moving marked Provisional, and the two newest marked Unstable.

**And what this round is worth on its own,** whatever the last two tests turn
out to be: Windows compiles and its security boundaries are proven again after a
month, Linux is green, the supply-chain gate is green, a beta can be published
without landing on stable users, and the interface promise is written down. The
pipeline that found the last two failures is the deliverable — it was dead
before, and dead pipelines are how a month-old Windows regression stays
invisible.
