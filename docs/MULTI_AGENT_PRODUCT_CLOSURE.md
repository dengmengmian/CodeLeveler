# Multi-Agent Product Closure

## Status

**MULTI_AGENT_PRODUCT_CLOSURE=FAIL.** The runtime contract, product
behavior, TUI/Web/Mobile UX and every truth and safety counter pass. The
product-value gate does not: on `deepseek/deepseek-v4-flash` with the
in-repo task set, multi-agent showed no latency or cost benefit on
parallelizable work, so MA4 is recorded as FAIL (MULTI_AGENT_BLOCKED as a
claimed benefit) and the closure cannot pass. Push was deferred by user
decision. No release tag; version not bumped.

Head: `65c4215` (code), this document on top of it.

## Baseline

Frozen `v0.2.0-beta.2` = `86bc214`, untouched. It already shipped
delegation (V2 background-first, MA-RT, profiles, write scope); MA0 audited
what was real before anything was built.

## MA0 Reality

`docs/MULTI_AGENT_MA0_REALITY_AND_GAP_AUDIT.md` — gap register (runtime
G1–G10, UX U1–U6, eval E1–E6, push, mobile), the generated MA1–MA5 scope, and
three user decisions: D1 durable resumable child sessions, D2 Mobile only
with Push deferred, D3 in-repo public task set on flash.
`MA0_REALITY_AUDIT=PASS`.

## Runtime Contract (MA1)

`docs/MULTI_AGENT_MA1_RUNTIME_CONTRACT.md`. A child is a durable session:
`SubAgentStarted{spec}` and its transcript are persisted, the terminal is
typed (`outcome` × `stop`), a second terminal is refused where it is written,
interrupted children resume under the same id (up to 3 times) or settle
`lost`, settlements are flushed before the notice the parent sees,
`CancelChild` stops one child, and pinned-model pricing is honest. Proven by
real `SIGKILL` restarts of an explorer and a worker.
`MA1_RUNTIME_CONTRACT=PASS`.

## Product Behavior (MA2)

`docs/MULTI_AGENT_MA2_PRODUCT_BEHAVIOR.md`. The model owns delegation; no
new roles, no provider trait; unknown role words and unusable pinned models
are refused; task tools on a child id answer what the id is. Real batch S1–S8.
`MA2_PRODUCT_BEHAVIOR=PASS`.

## UX (MA3)

`docs/MULTI_AGENT_MA3_PRODUCT_UX.md`. Clients render children from runtime
facts: snapshot `children`, `SubAgentStateChanged`, typed outcome/stop,
runtime notices never as user input, one entry per child, stop one child
(TUI `x`, Web 取消). Real PTY TUI and real browser acceptance.
`MA3_PRODUCT_UX=PASS`.

## Eval (MA4)

`docs/MULTI_AGENT_EVAL_AND_ACCEPTANCE.md`. New eval tooling (per-child
lifecycle, ownership, usage, multi-binary A/B with kill/resume, rescoring)
and a nine-case public task set. 75 A/B runs over three arms plus 9 dogfood
runs:

| | baseline (beta.2) | single (delegation off) | multi |
|---|---|---|---|
| task success | 22/25 | 24/25 | 25/25 |
| false verified | 0 | 0 | 0 |
| incorrect and verified | 3 | 1 | 0 |
| ownership violation · lost · duplicate · orphan | 0 | 0 | 0 |
| natural delegation | 0/25 | 0/25 | 1/25 |

The one natural delegation (8-package implementation) spawned six workers;
all six claimed their file, completed once, and their work passed the
hidden-test oracle — but the run took 119.9 s and 109,170 µUSD against
93.0 s and 17,983 µUSD without delegation. `MA4_EVAL_ACCEPTANCE=FAIL`
(value not demonstrated).

## Push / Mobile (MA5)

`docs/MULTI_AGENT_MA5_MOBILE.md`. The phone shows children from runtime
facts, rebuilds them after reconnect, and stops one. Real simulator journey
(real relay, agent, model). That acceptance surfaced two runtime bugs, both
fixed test-first: the interactive chat turn carried no steering source, so
`CancelChild` and mid-turn steer never reached TUI/Web/phone chat turns
(`06714ea`), and a child's cancel handle was registered only after its start
was visible (`3544129`). Push not built (D2).
`MA5_PUSH_MOBILE=PASS (Mobile; Push deferred by user decision)`.

## Cross-platform

| Check | Result |
|---|---|
| local `cargo fmt --all -- --check` | pass |
| local `cargo clippy --workspace --all-targets --all-features -D warnings` | pass |
| local `cargo test --workspace --all-features --locked --no-fail-fast` | 3884 passed, 0 failed, 20 ignored |
| local web protocol · typecheck · test · build | in sync · pass · 192 passed · built |
| local mobile `flutter analyze` · `flutter test` | no issues · 78 passed |
| exact main CI on `65c4215` | run `34817842199`, attempt 1: Linux, macOS, Windows `fmt · clippy · test` success; web success; mobile success; deny · audit success |

Inside that run: Windows security canaries success (windows-latest);
installer checksum canaries success (ubuntu-latest); browser acceptance
success with PASS lines on Chrome (ubuntu, macOS) and Edge (Windows).

## Dogfood (F3)

Release binary `65c4215` (clean), `deepseek-v4-flash`, fresh `navsvc` copy per
task, isolated `LEVELER_HOME`:

| Task | Children | Truth |
|---|---|---|
| parallel research (S4) | 2 explorers, both `completed_with_findings / completed`; the model's first two spawns named a non-existent agent and were refused with the available names, then succeeded | `PACKAGES.md` from both reports |
| parallel code changes (S5) | 2 workers, disjoint files, both completed | both `NOTES.md` written, each child's write inside its scope |
| background + restart (S9) | 1 worker, `SIGKILL` after two child tool results, `run --resume` | interrupted 1, resumed 1, same child id settled once, its file written |

```text
REAL_PROVIDER=YES
REAL_MODEL=YES
REAL_CHILDREN=YES
NO_FALSE_VERIFIED=YES (checks not run in these fixtures; nothing claimed verified)
NO_OWNERSHIP_VIOLATION=YES
NO_ORPHAN=YES
NO_DUPLICATE_SETTLEMENT=YES
```

## Residuals

- **Value**: natural adoption 1/25 on flash; the delegated run was slower
  and 6× the cost, dominated by parent reasoning before spawning. One run is
  not a rate.
- A stopped background child settles at its parent's next round boundary.
- Snapshot-vs-live ordering on the remote path unverified; live child tokens
  may be per activation.
- G7b/G7c accounting residuals (MA1); reviewer rows buffered.
- Android untested; Windows runtime unverified on a Windows host.
- Push not built.
- The shared working tree `codeleveler` holds another session's uncommitted
  Finalization work; this program committed from a detached worktree and
  pushed `HEAD:main`, so that tree's local `main` is behind `origin/main`.

## Final Gates

```text
MA0_REALITY_AUDIT=PASS
MA1_RUNTIME_CONTRACT=PASS
MA2_PRODUCT_BEHAVIOR=PASS
MA3_PRODUCT_UX=PASS
MA4_EVAL_ACCEPTANCE=FAIL
MA5_PUSH_MOBILE=PASS (Mobile; Push deferred by user decision)

DURABLE_CHILD_SESSIONS=PASS
RUNNING_CHILD_RESTART_SAFE=PASS
CHILD_SETTLEMENT_TRUTH=PASS
OWNERSHIP_SAFETY=PASS
CAPABILITY_CONTRACT=PASS
PROVIDER_ABSTRACTION=PROVEN_NOT_REQUIRED
BACKGROUND_FIRST_UX=PASS

TUI_MULTI_AGENT=PASS
WEB_MULTI_AGENT=PASS
MOBILE_MULTI_AGENT=PASS

PUSH=DEFERRED_BY_USER_DECISION

MULTI_AGENT_EVAL=FAIL (value not demonstrated)

FALSE_VERIFIED_TOTAL=0
OWNERSHIP_VIOLATION=0
LOST_ACCEPTED_CHILD=0
DUPLICATE_SETTLEMENT=0
OPEN_ORPHAN=0

FULL_WORKSPACE=PASS
EXACT_MAIN_CI=PASS (34817842199 attempt 1 on 65c4215)

MULTI_AGENT_PRODUCT_CLOSURE=FAIL
```

## Next

The failing gate is a measurement of value, not a defect list, and the
program's rules forbid closing it by forcing or encouraging delegation. What
would settle it, each a user decision:

1. **Model / effort**: repeat MA4 on a stronger model such as
   `deepseek-v4-pro`, or on flash at a lower `reasoning_effort` (the parent's
   planning time dominated the one delegated run). The earlier MA-VALUE-A
   study reported a success gain at 2.5× wall; which model it used is not
   re-verified here.
2. **Task size**: tasks where each independent unit takes minutes rather
   than seconds, so four concurrent children can win back the parent's
   planning cost; the harness already supports adding cases.
3. **Accept as capability, not benefit**: record multi-agent as a safe,
   model-chosen capability with value unproven, and change the closure
   criterion accordingly.

Versioning: none of this warrants a release tag; if a beta is cut for the
runtime and UX fixes, `0.2.0-beta.3` fits the change (no new user-facing
contract beyond typed child facts and `CancelChild`).

## Addendum — MA4-B (2026-09-14)

The original MA4 FAIL above stands. MA4-B
(`docs/MULTI_AGENT_WORKLOAD_THRESHOLD_VALIDATION.md`) held the model fixed
and varied workload size from one function to eight independent packages
(39 runs, stopped early by user decision). Delegation adoption followed size
(none on tiny/small work, about two thirds above), parallel time saved
exceeded coordination overhead from three independent units up, and every
runtime truth and ownership counter stayed zero — but no size showed a
stable, repeated benefit: medium work had a latency and cost signal that
could not be attributed to delegation, large work was high-variance, and at
the largest size delegated runs were less correct and bounded by the
100-round turn ceiling.

```text
MA4B_WORKLOAD_THRESHOLD=FAIL
MULTI_AGENT_PRODUCT_VALUE=NOT_PROVEN
MULTI_AGENT_PRODUCT_CLOSURE=BLOCKED
DEFAULT_MULTI_AGENT_POLICY=delegation stays available and model-chosen; not forced, not advertised as a benefit
```
