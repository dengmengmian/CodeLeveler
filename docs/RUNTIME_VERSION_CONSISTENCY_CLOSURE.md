# Runtime Version Consistency — Closure Record

The file [`RUNTIME_VERSION_CONSISTENCY.md`](RUNTIME_VERSION_CONSISTENCY.md) has
pointed here since it was written. It did not exist. This is it.

```
RUNTIME_VERSION_REAL_DOGFOOD=PARTIAL
RUNTIME_VERSION_CONSISTENCY_CLOSURE=PENDING_REAL_DOGFOOD
BR_A=OPEN
```

**The identity half is proven; the drain half was never put under load.** The
gate's own status line stays as it was, for the reason given in §5 — not
because anything failed.

---

## 1. What the gate says, recovered rather than restated

```
RUNTIME_VERSION_GATE_SOURCE=docs/RUNTIME_VERSION_CONSISTENCY.md
```

Its definition of a completed update:

> **更新完成的定义**：不是"磁盘上的文件换了"，而是
> **运行中的 runtime 的 BuildIdentity == 期望的 BuildIdentity**。

Its retirement mechanism:

```
ShutdownWhenIdle { reason }   # BuildMismatch | UpdateReady（预留给 Updater）
    ↓ accepting_work = false
    ↓ 已在跑的活继续跑，不取消、不 kill
    ↓ 排空后进程自己退出
```

And its safe-retire predicate, which it warns must not be shortened to "idle":

```
RUNTIME_SAFE_TO_RESTART_PREDICATE =
    活跃 turn 数 == 0  AND  存活后台任务数 == 0
```

**Two corrections to how this gate is often described.**

`UpdateReady` is reserved for a future Updater. The document is explicit —
「本次不实现：发现、下载、安装、回滚」— so it cannot be observed today, and its
absence is by design, not a gap. The implemented reason is `BuildMismatch`.

Replacing the binary on disk is not a way around the updater; it *is* the
incident and it *is* the trigger. `run_cmds.rs` says so in as many words:

```rust
// Replacing a binary on disk is not an update — this is.
```

## 2. Why this is not the F7 exact-object handoff again

```
F7 dogfood proved:   built object == executed object   (EXACT_OBJECT_HANDOFF=PASS)
This gate asks:      does the RUNNING runtime become B when B replaces A
```

They are different surfaces. F7's evidence supports this one; it cannot stand
in for it.

## 3. A and B

Two real builds, no product code changed to create them:

```
A = 0.2.0-beta.1 (702159956919)   sha256 9605be4783be1999d6fd…
B = 0.2.0-beta.1 (dd2ed3479d57)   sha256 1b6afb9d7e0979df19af…
```

They differ only by documentation commits, so their behaviour is identical and
only their provenance differs — `version` equal, `revision` different, both
clean. That is precisely the row the gate's matching table marks as the
incident class:

| 情况 | 结果 |
| --- | --- |
| 干净 A vs 干净 B（同 version） | **不匹配** ← 事故这一类 |

## 4. What was executed, on macOS 26.6.2 / arm64

Real binaries, real daemon, real socket, isolated `HOME`/`XDG`/`TMPDIR` inside
the dogfood lab.

### Forward cycle A → B

```
17:16   daemon A started from the install path        pid 71964
17:30   B installed over the install path by atomic rename
        → A stays alive on its original image          ← the incident, reproduced
17:31   a ProbeDefault client (now B) runs ensure_default_runtime
```

Observed in daemon A's own log:

```
INFO leveler_app::interactive: runtime retiring once work drains reason=BuildMismatch
INFO leveler_app::interactive: runtime idle; retiring
```

then A exited, and a new daemon came up as **pid 88688**.

The client got past `ensure_default_runtime` — it printed its session picker,
which happens only after the bootstrap returns — and the bootstrap's last act is

```rust
verify_replacement(reported.as_ref(), &BuildIdentity::current())?;
```

so the running runtime reported an identity matching B, once, with no retry.

**Independent confirmation.** Probing again with the B client left 88688 alive.
Had the running daemon still been A, a B client would have retired it a second
time. It did not, so the running runtime was B.

### Reverse cycle B → A

```
17:32   A installed by atomic rename; B (88688) still alive on its own image
        a ProbeDefault client (now A) probes
        → B exited, new daemon pid 89832
        → probing again with A left 89832 alive ⇒ running runtime == A
```

```
AB_DIRECTION_REQUIRED=one real replacement cycle
AB_DIRECTION_EXECUTED=both (A→B and B→A)
```

### Post-replacement smoke

`leveler sessions list` against the replaced runtime returned the durable
sessions, exit 0. Exactly one daemon process existed throughout.

### An aside worth recording

Overwriting the running binary in place (`cp` onto the same inode) got the
process **SIGKILLed by macOS** — observed `raw_exit=137`. Atomic rename, which
is what a real installer does and what leaves the running daemon on its old
image, behaves correctly. Anyone writing the future Updater's install step
should know this before discovering it in production.

## 5. Why this is PARTIAL and not PASS

**The daemon was idle at both retirements.** `runtime idle; retiring` followed
`runtime retiring once work drains` in the same millisecond, because
`turns == 0 && background.alive_count() == 0` was already true.

So the half of the gate it spends most of its words on — that a background task
outliving its turn must not be destroyed by a replacement — was never put under
test. The document is explicit that this is the case that matters:

> **只看 turn 是不够的**：那次 `cargo check` 活过它的 turn 半小时，按"无 turn 即闲置"
> 就会在它头上结束进程，把活扔掉。

**Why the work could not be placed in the daemon.** `leveler run` is headless
and always embedded: a background task started through it had the *client* as
its parent process, confirmed by `ps` — the daemon stayed idle. The only path
that reuses a daemon is `cmd_tui` (`SocketIntent::ProbeDefault`), and a TUI
needs a terminal: without one it fails `terminal io error: Device not
configured`, and driving it through a PTY did not get a task submitted.

That is a harness limitation, not a product defect. No product code was changed
to work around it.

```
WORK_DRAINED=NOT_EXERCISED
REASON=no daemon-hosted active work could be created headlessly
```

## 6. Result matrix

```
A_RUNNING_WITH_EXPECTED_IDENTITY = YES
B_INSTALLED                      = YES
UPDATE_READY_OBSERVED            = NOT_IMPLEMENTED_BY_DESIGN
                                   (reserved for the future Updater;
                                    BuildMismatch observed instead)
SHUTDOWN_WHEN_IDLE_ENTERED       = YES   reason=BuildMismatch
PREMATURE_EXIT                   = NO
WORK_DRAINED                     = NOT_EXERCISED   ← §5
A_EXITED_AFTER_DRAIN             = YES
B_STARTED                        = YES   pid 88688
EXPECTED_BUILD_IDENTITY          = 0.2.0-beta.1 (dd2ed3479d57)
OBSERVED_RUNNING_BUILD_IDENTITY  = 0.2.0-beta.1 (dd2ed3479d57)
RUNNING_RUNTIME_IDENTITY_MATCH   = YES
OLD_RUNTIME_STILL_ACTIVE         = NO
DUPLICATE_RUNTIME                = NO
POST_REPLACEMENT_SMOKE           = PASS
```

Eleven of twelve. The twelfth is the one the gate cares about most.

## 7. What closes it

A PTY-driven TUI that can submit one task, so the daemon holds a turn and a
background task when the replacement is triggered. The repository already has
PTY harness precedent (`F4 PTY contrast smoke 6/6`, `PTY 9/9` in the D4
replay), so this is reuse rather than new tooling.

The assertion that would then be added:

```
start a background task through a daemon-hosted session
→ let its turn end while the task stays alive
→ trigger the replacement
→ A must NOT exit while alive_count() > 0
→ A exits only after the background task ends
```

Until that runs, the gate's own status line stands.

## 8. Evidence

Raw evidence is local to the dogfood lab and not versioned:

```
$DOGFOOD_ROOT/eval/state/br-a-runtime-version-702159956919/
  evidence/environment.txt          macOS, arch, install path, A/B identities
  evidence/daemon-A.log             A's retirement, verbatim
  evidence/ready-A.json             A's pid, socket, runtime_id
  evidence/probe-B.log              the client that triggered A→B
  evidence/probe-A2.log             the client that triggered B→A
  evidence/timeline.txt             pids and timestamps for both cycles
  install/, home/, repo/            the isolated runtime environment
```

One artifact was lost and it is recorded rather than glossed: the new daemon
truncates `daemon.log` on startup (`File::create`), so the reverse cycle's
retirement lines were overwritten by the daemon that replaced it. The forward
cycle survived because that daemon was started with its own redirected log.
