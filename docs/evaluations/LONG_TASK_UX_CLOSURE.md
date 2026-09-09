# Long Task UX Closure

Phase C of the Beta Product Closure. One real 25-minute coding task, and one
real interruption, judged on whether a person could follow them — not only on
whether they passed.

## Verdict

```text
LONG_TASK_UX_CLOSURE = PASS
CORE_FREEZE          = UNCHANGED
```

No premature stop, no state loss, no misleading live status, no resume
corruption, no terminal contradiction. Two things could not be observed because
they did not happen; both are recorded rather than simulated.

## 1. The task

`yq-doc-count` against the frozen `yq` fixture at `bbdd9748`, run headless at
PRODUCT_HEAD `98d21bc` with the Phase A evaluation baseline.

| | |
| --- | --- |
| Wall | 1,520.4 s (25 min 20 s) |
| Model requests | 63 |
| Tool calls | 81 |
| Input / cached / output tokens | 3,242,659 / 3,151,360 (97.2%) / 57,141 |
| Median round | 6.3 s · p95 81.7 s · max 128.3 s |
| Time to first byte | 4.5 s median over 63 rounds |
| Retries / failures | 0 / 0 |
| Changed files | 8 |
| Terminal | `Completed`, acceptance and hidden tests both passed |

Time went where the previous round said it does: model wait 87.6%, tool
execution 12.3% (176 s of it real `shell_command` work), verification 1.4 s,
runtime overhead **0.09%**.

## 2. Could the user tell what was happening?

The session was sampled from outside, three times, while it ran:

| clock | status | requests | tools started / finished | in / out |
| --- | --- | ---: | --- | --- |
| 15:57 | running | 49 | 64 / 64 | 2,370,473 / 54,767 |
| 16:00 | running | 60 | 78 / 77 | 3,049,506 / 56,511 |
| 16:03 | completed | 63 | 81 / 81 | 3,242,659 / 57,141 |

Every question §20 asks is answerable from that: the run is alive, it is on
request 60, one tool call is in flight (78 started against 77 finished), and
nothing is retrying or failing. The second sample is the interesting one — the
one-call gap is the runtime telling the truth about work in progress, not a
lost event.

The interface side of the same question was settled in Phase B against
rendered frames: a running command names itself with its own elapsed, a model
round says `等待模型` rather than a bare spinner, and the plan dock tracks the
running step. This run's event mix is the shape those frames were built from —
81 tool calls across `read_file` (24), `shell_command` (21), `apply_patch` (11),
`run_command` (9), `grep` (5), `update_plan` (5).

## 3. Plan behaviour

The model updated its plan **5 times** across the 63 rounds, and the runtime
issued **zero** advisories. Nothing intervened, nothing was auto-advanced, and
no extra round was spent on plan ceremony. This is the first long run in which
the plan was used at all — Stages 1 to 4 of the previous round produced no plan
updates whatsoever — which suggests the plan earns its place on long work and
is ignored on short work, exactly as an author-owned artefact should be.

## 4. No hidden ceiling

63 rounds, terminal `Completed`. Nothing stopped the run but the model
deciding it was finished. The explicit bounds are unchanged and still covered
by name in the workspace suite: `a_pinned_round_ceiling_is_unconditional`,
`an_unpinned_round_ceiling_never_stops_the_run`,
`an_until_terminal_turn_is_not_cut_off_by_a_hidden_round_count`, the token and
cost caps at equality, the deadline reported as duration rather than
cancellation, and `headless_runs_get_a_wall_clock_ceiling_by_default`.

## 5. Interruption and resume

`n3-caller-propagation`, killed with `SIGKILL` to the whole process group at
18.1 s, then reopened with `leveler run --resume <id>`.

| | interrupted | resumed |
| --- | --- | --- |
| Session status | `running` | `completed` |
| Sessions on disk | 1 | 1 — the same id |
| Model requests | 3 | 6 |
| Tool calls started / finished | 7 / 7 | 10 / 10 |
| Dangling tool calls | 0 | 0 |
| `turn_started` / `turn_finished` | 1 / 0 | 2 / 2 |
| `task_started` / `task_finished` | 1 / 0 | 1 / 1 |
| Acceptance | — | passed, 2 files changed |

The kill left the session honestly unfinished: `running`, with a started turn
and no finished one, and no `task_finished`. The resume continued that session
rather than starting a new one — same id, still one session, tools 7 → 10
rather than back to zero — closed the interrupted turn, and reached the
terminal with the acceptance passing.

A separate probe caught the other case by accident: `run --resume` on a session
that had already completed exits non-zero and changes nothing, which is the
right answer to "resume what?".

## 6. What did not happen, and was not simulated

**Context compaction.** Zero compactions in the 25-minute run and zero across
every run this round. The prefix cache served 97.2% of a 3.2M-token input
without the window ever needing a fold, so §24's questions — does the plan
survive, does modified-file knowledge survive — have no real occurrence to
answer them here. Phase B rendered the compaction frame from a constructed
event and confirmed the interface says `上下文已压缩 84 → 12 条`; that is
presentation evidence, not runtime evidence, and it is not claimed as more.

**Child agents.** None spawned. §19 says not to force one, so none was forced.
`subagent_started = 0` is recorded, not worked around.

**Blocked.** No blocked outcome arose here. The honesty lane covered it two
rounds ago and again in the previous comparative, where CodeLeveler produced
`HONEST_FAILURE` twice out of two.

## 7. Gate

| requirement | result | evidence |
| --- | --- | --- |
| no hidden premature stop | PASS | 63 rounds, terminal `Completed`, no ceiling |
| no state loss | PASS | 81/81 tool calls paired, 0 retries, 0 failures |
| no misleading live status | PASS | live sampling tracked requests, in-flight calls, no phantom progress |
| no resume corruption | PASS | one session id across the break, 0 dangling, acceptance passed |
| no terminal contradiction | PASS | `Completed` with acceptance and hidden tests both green |

`LONG_TASK_UX_CLOSURE = PASS`. Open, carried from Phase B: a resumed session
does not say on the default screen that the working tree already carries
changes.

Evidence: `evals/baselines/beta-phaseC-98d21bc/`.
