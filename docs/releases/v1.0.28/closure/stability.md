# Stability: steer/exit flake and 24-hour soak

## 1. `an_exit_racing_a_steer_keeps_the_result_and_the_message`

Status: **FLAKY_UNRESOLVED**.

The test at `crates/leveler-app/tests/background_wait_interaction.rs:491` once
timed out at 20 s in an earlier gate. The original process log was not
retained, so neither the expired helper nor the event sequence can be
recovered. The test synchronizes on durable rows, runtime events and task
terminal facts; the steering admission close and the round drain share one
mutex.

Controlled reproduction used the compiled integration-test binary from
`main` at `153f0e19` (SHA-256 `e4606d80…`), exact test, single-threaded:

| Round | Runs | Concurrency | CPU burners | PASS | FAIL | TIMEOUT | p95 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 200 | 1 | 0 | 200 | 0 | 0 | 0.363 s |
| 2 | 320 | 4 | 0 | 320 | 0 | 0 | 1.300 s |
| 3 | 800 | 8 | 8 | 800 | 0 | 0 | 1.712 s |

1,320 closure runs plus 450 prior runs produced no failure. No source change
was made and no timeout was raised. The slowest run is two orders of magnitude
inside the smallest helper deadline, which makes a short-deadline explanation
unlikely but does not exclude an unknown CI host condition. The correct status
is unresolved, not fixed and not innocent. The stress driver is preserved for
the next occurrence.

## 2. 24-hour soak

Status: **IN_PROGRESS** — not PASS before 24 hours.

| Control | Value |
| --- | --- |
| Binary | published v1.0.28 macOS arm64, SHA-256 `5022619d7dda605cc8b57bcd7c5d3b27e409dd56460ccd516efb81592dc9615e` |
| Config | lab config + auto-update off, SHA-256 `2f10b9011519c738d2edcd194f0d16afb404b03bbded650aa0fa7f10d95810cd` |
| Started | 2026-11-22T13:15:52Z |
| Target end | 2026-11-23T13:15:52Z |
| Cycle | 600 s |
| Host | Apple M4 Max, macOS 26.6.2, arm64, detached under `caffeinate -i` |

Each cycle: a real tool turn; Goal every 3rd; failing-tool recovery every 4th;
background success/failure every 5th; abnormal `SIGKILL` + session read +
`run --resume` every 7th; compaction-sized read every 11th; live TUI `/help`,
conversation snapshot every cycle, and a steer every 5th. Resource samples
(TUI RSS/CPU/FDs, state size, sockets, process count, load) and a heartbeat are
written every cycle; failed scenarios keep their raw stdout/stderr.

Before the long run, two one-cycle smokes exercised the same driver. The first
found a harness defect (the TUI language was not pinned and the composer
predicate knew only one shipped placeholder), which was fixed in `soak.py`;
the second passed 9/9 with status `COMPLETED`. A first long run was then
stopped in cycle 0 because its process counter matched every `leveler` process
on the host, including unrelated daemons; the counter is now scoped to the soak
lab and the run restarted at the time above. The aborted run's heartbeat is kept
under `/tmp/pr28-soak-run0-aborted-20261122T131456Z/`. No product code changed.

At report time the run is still inside its first cycles: cycle 0 recorded
`tui_start` PASS at 308 ms, tool/Goal/background/abnormal-exit/recovery/
compaction PASS, a scoped process count of 5, and a heartbeat. That is progress
evidence, not a stability verdict. The authority is
`/tmp/pr28-soak/heartbeat.json` during the run and `/tmp/pr28-soak/result.json`
only if it completes; an interrupted run is `IN_PROGRESS`/`INCOMPLETE`, never
PASS.
