# Long-Task Round Budget Closure

Status: **LONG_TASK_ROUND_BUDGET_CLOSURE=PASS.** Main CI `34916971546`
(attempt 1) green on `f89c1db`; the S4 revalidation ran 6 of 6 runs to
completion with no round-limit stop (MA4-C: 3 of 4 `high` runs cut at 100).

Two independent defects cut long `leveler run` goals off at 100 rounds: a hidden 100-round ceiling that overrode every larger explicit round
window, and a `wait_task` that answered a child id instantly, so a parent
polling its children spent one model round per poll. Both are fixed with
tests. No number was invented: the windows callers already declare now take
effect.

## 1. The original contract

| Source | Value | What it was supposed to mean |
|---|---|---|
| `leveler run --max-rounds N` (`cli.rs`) | default 200; `0` = no round budget | the run's round budget |
| `DEFAULT_TASK_ROUNDS` (`leveler-app/src/lib.rs`) | 200 | the default of that flag |
| eval case `max_rounds` (`multi_agent_threshold/*.yaml`) | S2 150, S3 250, S4 400 | the registered per-case window |
| `MAX_BOUNDED_TURN_ROUNDS` (`executor/drive.rs`) | 100 | "default ceiling for BOUNDED work that did not pin its own `max_rounds`" |
| interactive / TUI / resume turns | `UntilTerminal` | no round count |

In code the round ceiling is described as a runaway fuse ("not a budget: a
circuit breaker that fires regardless of progress", `limits.rs`), and the CLI
tells the user to "check if the model was looping" when it fires.

**The defect.** Every `Bounded { max_rounds }` continuation carries its own N,
but the drive still set `round_ceiling = Some(100)` for all bounded work, and
the kernel checks the ceiling before the window (`limits.rs:201-211`). The
effective limit was `min(N, 100)`: `--max-rounds 400`, the default 200, and
every registered case window above 100 were silently capped at 100 and
reported as `TurnLimitReached`. `--max-rounds 0` was the only way past it.
The CLI help ("default 200, extendable +100 up to twice") described extension
logic deleted in `9a09106`.

## 2. Stop conditions of a top-level `leveler run` goal

| Stop condition | Soft / hard | User-configurable | Product meaning → outcome |
|---|---|---|---|
| `update_goal(complete)` | terminal | — | Completed |
| `update_goal(blocked)` | terminal | — | Blocked |
| goal-mode quiet after one closeout nudge | soft, then hard | no | Stalled → Failed |
| verification | never stops the loop | `verify` config | verification status only |
| user cancel | hard | Ctrl-C | Interrupted (wins at the terminal publish boundary) |
| duration | hard | `limits.max_duration_seconds`; headless default 1 h | BudgetExhausted → BudgetLimited |
| model tokens / cost | hard | `limits.max_model_tokens` / `max_cost_usd_micros` | BudgetExhausted → BudgetLimited |
| no progress (every call refused, streak ≥ 2) | hard | no | Incomplete → Failed |
| explicit `StepLimits.max_rounds` ceiling | hard, progress-blind fuse | internal callers | TurnLimitReached → BudgetLimited |
| round window (`--max-rounds N`, case `max_rounds`) | hard | `--max-rounds`, case YAML | BudgetExhausted → BudgetLimited |
| model / provider error | hard | no | Failed |

Before this closure the round window above 100 was unreachable, and the
100-round fuse acted as the normal completion boundary of every long headless
goal.

## 3. MA4-C censorship evidence

Only the S4 `high` arm hit the limit (3 of 4 runs; `max` 0 of 4; S2/S3 0).
Per truncated run, the parent's rounds and what the last rounds did:

| Run | Parent rounds | Last parent edit (round) | Rounds whose only calls were `wait_task`/`get_task` on a child | Rounds without those | Last 20 rounds | Class |
|---|---|---|---|---|---|---|
| S4 high pilot | 100 | 70 | 45 (45 `wait_task` in 6.4 min) | 55 | 17 `wait_task`, 0 edits, 2 children settled | REPEATING (poll loop) |
| S4 high rep 0 | 100 | 98 | 3 | 97 | 7 edits, 9 test runs, `textdiff` fail → ok | PROGRESSING |
| S4 high rep 2 | 100 | 40 | 32 (82 `get_task` in 9.5 min) | 68 | polls, 0 edits, 2 children settled | REPEATING (poll loop) |

Every poll answered at once: "`<id>` is sub-agent … not a background task. It
is still running; its result is delivered to you automatically when it
settles. There is nothing to wait on or poll." — while the product already
blocks a quiet parent round until a child settles (`drive.rs`, "a quiet
round while background children run is WAITING"). The `max` parents polled
too (S4 max rep 1: 22 polls, 17 poll-only rounds, finished at 74).

```text
PRODUCTIVE_TRUNCATED_RUNS=1 (rep 0: explicit window 400 capped at 100)
STALLED_OR_REPEATING_TRUNCATED_RUNS=2 (poll loops; without poll rounds 55 and 68 < 100)
```

Raising the number alone would have let the two poll loops spin longer.

## 4. New contract

```text
ROUND_CEILING  = only an explicit StepLimits.max_rounds (internal callers)
ROUND_WINDOW   = the caller's N, and it is the hard edge: leveler run default 200,
                 --max-rounds N, eval case max_rounds; 0 = no round budget (unchanged)
UntilTerminal  = no round count (interactive, TUI, resume — unchanged)
wait_task on a child id = waits up to its bounded interval (default 30 s, max 120 s,
                 the same `wait_interval` as for background tasks), returns as soon as
                 the child ends; cancellation ends the wait
get_task / kill_task on a child id = unchanged instant answers
```

| | Before | After |
|---|---|---|
| `leveler run` (no flag) | stops at 100 | stops at 200 |
| `--max-rounds 400` | stops at 100 | stops at 400 |
| `--max-rounds 50` | stops at 50 | stops at 50 |
| eval case window ≤ 100 | its window | its window |
| interactive turn | no round count | no round count |
| parent `wait_task` on a running child | instant, 1 round per poll | waits ≤ interval, 1 round per settlement or interval |

```text
OLD_ROUND_LIMIT=100 (effective, for every bounded window)
NEW_ROUND_LIMIT=the declared window: leveler run 200, S4 case 400 (registered in c06249f, unchanged)
RATIONALE: no S4 run in MA4-B or MA4-C finished past 95 parent rounds (MA4-C max 33–74, high 54);
           the one productive truncated run needed more than its 97 non-poll rounds.
           The declared 200 default is 2.7× the longest observed natural completion; nothing new is chosen.
SCOPE: bounded windows above 100 (headless `leveler run`, eval cases, named agents with max_rounds > 100);
       no path at or below 100 rounds changes
```

Runaway protection stays: every bounded run still stops at its window, the
headless 1 h duration cap and the token/cost budgets are unchanged, and the
new wait is bounded per call and cancellable. `--max-rounds 0` (no round
budget) existed before and is unchanged.

## 5. Tests

| Test | Contract | Red before fix |
|---|---|---|
| `loop_test::a_bounded_window_above_one_hundred_rounds_is_the_hard_edge` | a never-completing loop with window 130 stops at 130, `BudgetExhausted` | yes (stopped at 100, `TurnLimitReached`) |
| `loop_test::bounded_continuation_still_stops_at_its_window` (existing) | window 3 stops at 3 (normal task unchanged) | — |
| `loop_test::absolute_round_ceiling_terminates_a_busy_never_ending_loop` (existing) | explicit ceiling 5 → `TurnLimitReached` at 5 (hard ceiling) | — |
| `loop_test::an_until_terminal_turn_is_not_cut_off_by_a_hidden_round_count` (existing) | interactive turn passes 100 | — |
| `direct_test::a_wide_round_window_keeps_completion_and_cancellation_terminals` | window 200: completion → `Completed`; cancel → `Interrupted` | no — guard; the fix does not touch these paths |
| `multi_agent_test::wait_task_on_a_child_waits_for_it_to_end` | `wait_task` returns "finished" when the child ends mid-wait, before its 5 s interval | yes (instant "still running") |
| `multi_agent_test::task_tools_naming_a_running_child_say_what_the_id_is` (existing) | a child that does not end: `wait_task` still answers "still running" after the interval (bounded) | — |
| `direct_test::bounded_eval_goal_still_stops_at_the_case_round_limit` (existing) | window stop is `BudgetLimited`, never `Completed` | — |

```text
ROUND_LIMIT_FALSE_COMPLETION=0 (window and ceiling stops map to BudgetLimited; run.rs mapping test unchanged)
```

## 6. Commits

| Commit | Change |
|---|---|
| `3b2b9d0` | (preceding, separate) rustls 0.23.45 for RUSTSEC-2026-0285; main CI `34911569295` attempt 1 green |
| `35e9055` | round window is the hard edge; hidden 100 removed; CLI help corrected; verified alone (clippy + loop/direct/multi-agent suites) |
| `376bac2` | `wait_task` on a child waits up to its bounded interval |

## 7. Final gate

```text
cargo fmt --all -- --check                                         PASS
cargo clippy --workspace --all-targets --all-features -D warnings  PASS
cargo test --workspace --all-features --locked --no-fail-fast      PASS (3910 passed, 0 failed)
```

Main CI and the S4 revalidation are recorded in
`MULTI_AGENT_PARENT_REASONING_BUDGET_VALIDATION.md` §16.
