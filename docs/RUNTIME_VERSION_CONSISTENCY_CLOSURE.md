# Runtime Version Consistency — Closure Record

The file [`RUNTIME_VERSION_CONSISTENCY.md`](RUNTIME_VERSION_CONSISTENCY.md) has
pointed here since it was written. It did not exist. This is it.

```
RUNTIME_VERSION_NON_IDLE_DRAIN_PROOF=PASS
RUNTIME_VERSION_REAL_DOGFOOD=PASS
RUNTIME_VERSION_CONSISTENCY_CLOSURE=PASS
BR_A=CLOSED
```

Both halves are proven. The identity half was closed in the first cycle (§4);
the drain half — a retirement that waits on live daemon-owned work — is §5a.

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

## 5. The first cycles were idle — recorded, then answered

Both replacements in §4 happened against an **idle** daemon:
`runtime idle; retiring` followed `runtime retiring once work drains` in the
same millisecond, because `turns == 0 && background.alive_count() == 0` was
already true.

So the half of the gate it spends most of its words on had not been tested:

> **只看 turn 是不够的**：那次 `cargo check` 活过它的 turn 半小时，按"无 turn 即闲置"
> 就会在它头上结束进程，把活扔掉。

`leveler run` could not supply the work — it is headless and always embedded,
and a background task started through it had the *client* as its parent, so
the daemon stayed idle. The only daemon-reusing client is `cmd_tui`
(`SocketIntent::ProbeDefault`), and a TUI needs a terminal.

## 5a. Non-idle drain proof

Answered by driving a real `leveler tui` over a PTY — the same harness shape
`BROWSER_CAPABILITY_REPORT.md` §T used to prove daemon ownership, and the same
evidence method: **process ancestry**, not a screen scrape.

```
PTY_HARNESS_REUSED=YES   (pattern from BROWSER_CAPABILITY_REPORT.md §T)
HUMAN_ASSISTED_DOGFOOD=NO
PRODUCT_RUNTIME_CHANGED=NO
```

A goal was submitted through the PTY TUI: start `sleep 240` with
`background=true`, then run `sleep 90` in the foreground. The daemon's own
event log records the tool call and its result:

```
run_command  {"args":["240"],"background":true,"program":"sleep"}
→ background task started · task_id: bg-1 · status: running
```

and both processes were **children of the daemon**, which is what
daemon-owned means:

```
43657  ppid 43169  sleep 240      ← background task
43662  ppid 43169  sleep 90       ← the turn's foreground command
43169  = daemon A
```

### The sequence, with its timestamps

```
18:21:41   daemon A starts                              pid 43169
18:21:57   background task bg-1 alive, owned by A
18:22:19   B installed by atomic rename
18:22:20   probe → A logs:
             runtime retiring once work drains reason=BuildMismatch

18:22:39   A=alive   bg240=alive   fg90=alive
18:22:49   A=alive   bg240=alive   fg90=alive
18:22:59   A=alive   bg240=alive   fg90=alive
18:23:09   A=alive   bg240=alive   fg90=alive
18:23:19   A=alive   bg240=alive   fg90=alive     ← 59s after retirement began
18:23:29   A=alive   bg240=alive   fg90=ended     ← turn over, background still alive,
                                                    A STILL HAS NOT EXITED
18:23:30   A logs: runtime idle; retiring
18:23:39   A=gone    bg240=gone    fg90=gone
18:23:30   B starts                                pid 45519
```

**A stayed alive for about seventy seconds after being told to retire, for
exactly as long as it still owed work.** The required ordering holds:

```
T_background_start (18:21:57)
  < T_build_mismatch = T_shutdown_when_idle (18:22:20)
  < T_background_end (≤18:23:30)
  ≤ T_A_exit (18:23:39)
  ≈ T_B_start (18:23:30)

PREMATURE_EXIT=NO
```

### That the running runtime is B, both ways

The replacement daemon (45519) was probed from both sides:

```
B client → 45519 survives   ⇒ classified Current  ⇒ running identity == B
A client → 45519 retires    ⇒ classified Outdated ⇒ running identity != A
```

`lsof` on the socket named 45519, so it is the serving runtime and not a
bystander.

### One correction, kept rather than tidied away

A third `serve` process (43103) was present on the machine and was **first
recorded as B by mistake**. It is not: its `--ready-json` names pid 38909,
from an earlier aborted setup in this lab whose TUI client revived a daemon
when its own was killed. It never held this run's socket.

That is harness hygiene, not a duplicate runtime produced by the replacement —
and the distinction was settled by `lsof`, not by assumption. The lab's
cleanup between runs was at fault and the record says so.

## 6. Result matrix

```
A_RUNNING_WITH_EXPECTED_IDENTITY = YES
B_INSTALLED                      = YES
UPDATE_READY_OBSERVED            = NOT_IMPLEMENTED_BY_DESIGN
                                   (reserved for the future Updater;
                                    BuildMismatch observed instead)
SHUTDOWN_WHEN_IDLE_ENTERED       = YES   reason=BuildMismatch
PREMATURE_EXIT                   = NO
WORK_DRAINED                     = YES   ← §5a
A_EXITED_AFTER_DRAIN             = YES
B_STARTED                        = YES   pid 88688
EXPECTED_BUILD_IDENTITY          = 0.2.0-beta.1 (dd2ed3479d57)
OBSERVED_RUNNING_BUILD_IDENTITY  = 0.2.0-beta.1 (dd2ed3479d57)
RUNNING_RUNTIME_IDENTITY_MATCH   = YES
OLD_RUNTIME_STILL_ACTIVE         = NO
DUPLICATE_RUNTIME                = NO
POST_REPLACEMENT_SMOKE           = PASS
```

```
BACKGROUND_WORK_STARTED                 = YES   task_id bg-1
BACKGROUND_DAEMON_OWNED                 = YES   ppid == daemon pid
BACKGROUND_ALIVE_COUNT_BEFORE_REPLACE   = > 0
BACKGROUND_ALIVE_COUNT_DURING_RETIRE    = > 0
A_STILL_ALIVE_WHILE_BACKGROUND_ACTIVE   = YES   ~70 s
BACKGROUND_WORK_FINISHED                = YES
BACKGROUND_ALIVE_COUNT_AFTER_DRAIN      = 0
WORK_DRAINED                            = YES
```

Twelve of twelve.

## 7. A product fact this proof surfaced

A daemon-owned background task outlives its **turn**, and is reaped at its
**session's terminal settlement**:

```
INFO leveler_engine::engine: terminal settlement reaped 1 session-owned
                             background task(s) session="f9bad960-…"
```

The first attempt at this proof let the goal finish, so the task was reaped
before the replacement could be triggered and the daemon was idle again. The
window the gate protects is therefore *turn ended, session not yet settled* —
which is exactly the `cargo check` case its own text describes, and which the
proof above occupies deliberately.

Worth stating because the gate's wording ("活过它的 turn") is true and
incomplete on its own: the lifetime is bounded by the session, not the turn.

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
