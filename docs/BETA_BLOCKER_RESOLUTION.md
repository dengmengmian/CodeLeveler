# Beta Blocker Resolution

**Date:** 2026-08-22 · **Branch:** `main` · **Base:** `c37a6cd4`
· **Audit this answers:** [`BETA_RELEASE_READINESS.md`](BETA_RELEASE_READINESS.md)

**`BLOCKER = 0`.** All four blockers from the readiness audit are closed, and
closing them turned up **three further Windows defects** the audit could not see:
CI had died at the first compile error, so nothing behind it had ever been
looked at.

Verification is stated per fix, and the boundary is stated too: cross-platform
work was verified locally with a cross-compiler and a Linux container. GitHub
Actions on `main` remains the authority, and this document does not claim its
verdict in advance.

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
| Tests (host, macOS arm64) | `cargo test --workspace --all-targets --all-features --locked --no-fail-fast` | **3145 passed · 0 failed · 6 ignored** |
| Tests (Linux, container) | `rust:1.90` + bubblewrap, `-p leveler-execution --lib -p leveler-app --test user_shell` | **216 / 0** and **9 / 0** — the four SBPL failures and the hard-gate failure are gone |
| Supply chain | `cargo deny check` | **PASS** — `advisories ok, bans ok, licenses ok, sources ok` |
| Installer | `sh scripts/test_install.sh` | **PASS**, including two new pinned-version canaries (verified red without the pin support) |
| Manifest | `cargo metadata --no-deps` after the version bump | **PASS** |

**CI result.** CI_RESULT_PLACEHOLDER

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
| 1 | **Windows security canaries have not run in a month.** NTFS junction behaviour, AppContainer ACLs, Job Object descendant termination — the job died before them. | They are guarded by CI on every push and will run on this merge; nothing in these fixes touches the boundaries they exercise. | A canary failing. Then Windows confinement is unproven, and that *is* a Beta blocker. |
| 2 | **`windows-gnu` is not `windows-msvc`.** | The Rust behind `cfg(windows)` is identical for both ABIs; the difference is C toolchain and linkage. | An MSVC-only compile error in CI. |
| 3 | **No live `!command` streaming for confined Windows commands.** | Output still arrives, complete, on completion; documented in three places rather than discovered. | Windows users reporting it as broken rather than limited. |
| 4 | **The Linux backend enforces no read denial.** The eval harness cannot seal an answer key on Linux. | Evals run on macOS today, and this is measurement hygiene, not a user-facing security boundary. | Running the eval harness on Linux and trusting its numbers. |
| 5 | **`leveler-browser --test reliability` is flaky under full-workspace parallelism** (`loopback_ws_from_a_granted_dev_page_connects`, R-9 in the audit). Untouched here. | It passes 11/11 in isolation; the failure is contention, and the capability it guards is separately recorded as unproven in production. | It failing in isolation, or CI failing on it — at which point it is a defect, not contention. |
| 6 | **Preset default models are dated** (`gpt-4o`, `claude-sonnet-4-5`). | `leveler login` asks the provider for the real model list; the preset is only the fallback for gateways with no `/models` endpoint. | A user on such a gateway getting a model their key cannot reach. |
| 7 | **No cross-provider fallback.** A long unattended run dies with its provider. | Not promised anywhere; retry-with-backoff within a provider exists. | Beta feedback that unattended runs die on provider blips. |

---

## Beta Release Decision

```
BLOCKER              = 0
WINDOWS_BUILD        = FIXED   (verified on windows-gnu; msvc pending CI)
LINUX_CI             = FIXED   (verified in a Linux container)
SUPPLY_CHAIN         = PASS
CANDIDATE_ON_MAIN    = YES
VERSION              = 0.2.0-beta.1
PRE_RELEASE_CHANNEL  = READY

READY_FOR_BETA_RELEASE = YES, conditional on the first green CI run on `main`
```

**The condition is not a formality, and it is deliberate.** Every blocker was
found because the pipeline was red and nobody had looked; declaring the pipeline
fixed without letting it run would repeat exactly that mistake. Three of the
four blockers were verified against a cross-compiler and a container — good
evidence, and not the same as the machine that builds what users download.

So: push, watch the run, and read it. If Windows compiles and its security
canaries pass, if Ubuntu is green and cargo-deny is green, then
`READY_FOR_BETA_RELEASE = YES` stands unconditionally and tagging
`v0.2.0-beta.1` is the next action. If the Windows job surfaces an MSVC-only
problem, that is one more defect of the same family, fixed the same way — and it
will be visible in minutes rather than in a month.

What ships when it does: a local-first coding agent runtime with a **secure
Multi-Agent Runtime** — reliable, isolated execution whenever the model elects
to collaborate — with the surfaces it promises marked Frozen, the ones still
moving marked Provisional, and the two newest marked Unstable.
