# RepositoryMap Repeated A/B Validation

## 1. Objective

`docs/ROUND_TRIP_CLOSURE.md` reported that putting a bounded `RepositoryMap`
into the first-round cached prefix cut rounds 17.9% and wall 14.7%. Those
figures came from **one run per case**. This experiment tries to refute them.

One question: does the change repeatably reduce round trips, or was that noise?

## 2. Frozen objects

| | A (baseline) | B (treatment) |
| --- | --- | --- |
| HEAD | `e695b82240b988f2971bb3e2f83bc9bda333c168` | `3767bb6cfed7a067d7b681bf691541739b146878` |
| Tree | `9c44f61ea0d51e3bd9e7afbbb15c03d5a22d453b` | `2241caa80f34cdd594110ebc7e9f18e6d71be6ed` |
| Dirty | false | false |
| Build identity | `e695b82240b9` | `3767bb6cfed7` |
| Binary sha256 | `5e083af9a570d440…` | `c6e1a8df871480ec…` |

Detached worktrees, `cargo build --release --locked`, a separate
`CARGO_TARGET_DIR` per arm. Every run re-verified the binary's sha256 and
embedded build identity before spending a model request; a mismatch aborts the
cohort.

The treatment commit of interest is `98d21bc` (RepositoryMap). `3767bb6` also
carries `369283e`, the `update_goal` wording fix. It was measured NO_EFFECT
previously and is tracked here as a confound (§12).

## 3. Environment

Apple M4 Max, 64 GiB, Darwin 25.6.0 arm64. `deepseek/deepseek-v4-flash` at
`reasoning_effort = max` through the Taotoken OpenAI-compatible gateway,
Assisted permissions, `parallel_tool_calls = false`. Identical for both arms,
one machine, one continuous session.

## 4. Method

Four cases, three repetitions each, arms interleaved and the order reversed
between cases (`ABBAAB` / `BAABBA`) so provider drift cannot align with an arm.
Every run got a fresh `LEVELER_HOME`; the harness materializes a fresh worktree
per case. Rounds are classified by `evals/scripts/round_taxonomy.py` from the
tracing the runtime already emits.

`scale-s800` was escalated to because the three main cases came back with
mixed per-case direction and high within-arm variance — exactly the condition
the protocol says to escalate on. `yq-doc-count` was not run.

24 runs total.

## 5. Correctness

| | A | B |
| --- | --- | --- |
| Cases passed | 11 / 12 | 11 / 12 |
| False success | 1 | 1 |
| Runtime crashes | 0 | 0 |
| Forbidden mutations | 0 | 0 |

Both failures are the same case, `scale-s800`, one run in each arm: the agent
claimed completion and the independent oracle disagreed
(`failure_category: understanding`). It is a property of that case at this
model — the failure rate is roughly 1 in 3 in both arms — not a regression
introduced by the treatment. Correctness is identical between arms, which is
what the comparison requires.

## 6. Per-case raw results

| case | arm | rounds (3 reps) | median | wall s (3 reps) | median | passed | first tool |
| --- | --- | --- | --- | --- | --- | --- | --- |
| rust-first-even | A | 7, 6, 6 | 6 | 23, 19, 18 | 19 | 3/3 | `list_files` |
| rust-first-even | B | 5, 6, 6 | 6 | 16, 19, 19 | 19 | 3/3 | `read_file` |
| n3-caller-propagation | A | 24, 15, 15 | 15 | 100, 54, 66 | 66 | 3/3 | `list_files` |
| n3-caller-propagation | B | 20, 14, 17 | 17 | 81, 61, 70 | 70 | 3/3 | `read_file` |
| mf1-refund-propagation | A | 13, 18, 19 | 18 | 50, 71, 98 | 71 | 3/3 | `list_files` |
| mf1-refund-propagation | B | 14, 13, 15 | 14 | 64, 67, 62 | 64 | 3/3 | `read_file` |
| scale-s800 | A | 31, 28, 26 | 28 | 261, 255, 238 | 255 | 2/3 | `list_files` |
| scale-s800 | B | 33, 31, 23 | 31 | 308, 260, 226 | 260 | 2/3 | `shell_command`, `grep` |

No outlier was removed.

## 7. Aggregate

| metric | A | B | delta |
| --- | --- | --- | --- |
| **3 main cases** | | | |
| Correctness | 9/9 | 9/9 | — |
| Total rounds | 123 | 110 | **−10.6%** |
| Median rounds / run | 15 | 14 | — |
| `list_files`-first | 9/9 | 0/9 | — |
| Input tokens | 2,717,709 | 2,361,412 | −13.1% |
| Output tokens | 21,693 | 20,275 | −6.5% |
| Total wall | 500 s | 459 s | −8.2% |
| Median wall | 54.2 s | 62.2 s | +14.8% |
| Seconds / round | 4.06 | 4.17 | +2.7% |
| Closeout nudges | 2 | 5 | — |
| **All 4 cases** | | | |
| Correctness | 11/12 | 11/12 | — |
| Total rounds | 208 | 197 | **−5.3%** |
| Median rounds / run | 16.5 | 14.5 | — |
| `list_files`-first | 12/12 | 0/12 | — |
| Input tokens | 5,243,631 | 4,827,031 | −7.9% |
| Output tokens | 40,571 | 42,231 | +4.1% |
| Total wall | 1255 s | 1253 s | −0.1% |
| Median wall | 68.4 s | 65.6 s | −4.1% |
| Seconds / round | 6.03 | 6.36 | +5.5% |
| Closeout nudges | 4 | 7 | — |

## 8. First tool / discovery

The mechanism is unambiguous and it is the strongest result here:

| | A | B |
| --- | --- | --- |
| `list_files` as the first tool call | **12 / 12** | **0 / 12** |

Every A run opened by asking what files exist. No B run did. B opened with
`read_file` on the three smaller cases and with `shell_command` / `grep` on
`scale-s800`, where the listing is truncated and search is the sensible move.

`INITIAL_DISCOVERY_ROUNDS` beyond this is reported as UNAVAILABLE: separating
"structure discovery" from "task-specific reading" needs a semantic judgement
the taxonomy cannot make mechanically, and guessing per round was out of scope.

## 9. Tokens and cache

| | A | B |
| --- | --- | --- |
| Cache hit rate | 93.7% | 93.4% |
| Median first-request input | 17,310 | 17,506 |

The listing did **not** break the prefix cache — the hit rate is unchanged
within a third of a percent, which is the empirical evidence that the system
prompt stayed byte-identical across the requests of a run, as designed.

What the listing costs, once, uncached, on the first request:

| case | A | B | delta |
| --- | --- | --- | --- |
| rust-first-even | 17,175 | 17,261 | +86 |
| n3-caller-propagation | 17,303 | 17,582 | +279 |
| mf1-refund-propagation | 17,316 | 17,429 | +113 |
| scale-s800 (1,822 files) | 17,329 | 19,877 | +2,548 |

Total input is nevertheless **lower** in B (−7.9% over all four cases), because
fewer rounds means the whole transcript is re-sent fewer times. The added
prefix is paid once; a round trip is paid in full.

## 10. Wall clock

Wall is secondary here and it does not support a claim. On the three main cases
the total is −8.2% but the median is +14.8%; on all four the total is −0.1% and
the median −4.1%. The direction flips with the aggregation choice, which is
what "not distinguishable from noise" looks like.

Seconds per round is flat to slightly up (4.06 → 4.17, 6.03 → 6.36). That
matters: it confirms the provider did not get faster for B. Any total-wall
movement came from doing fewer rounds, not from cheaper ones — which is the
mechanism under test.

**The previously reported −14.7% wall is not reproduced and must not be
quoted.**

## 11. Mechanism validation

CONFIRMED. The listing removes the opening repository-discovery round in 12 of
12 runs, and does so without disturbing the prefix cache. That specific causal
claim is as well supported as this setup can make it.

What is *not* confirmed is that removing that round reliably shortens the whole
task. On `n3` and `scale` the agent spent the saved round elsewhere and the
median round count went up.

### Bounds (verified)

`workspace_listing` caps at 8 KiB on a line boundary and marks truncation;
`RepositoryMap` caps at 400 files and depth 6. Both covered by unit tests that
pass on the frozen B tree. `scale-s800` (1,822 files) exercised the truncation
path and cost +2,548 first-request tokens, inside the cap.

## 12. Confounds

**Closeout wording (`369283e`).** B fired *more* closeout nudges than A (7 vs 4
over 12 runs). It therefore cannot explain B's round advantage — it works
against it. Removing those rounds from both arms leaves B further ahead, not
less. No sensitivity adjustment is needed in B's favour.

**Case variance.** Within-arm spread is large and comparable to the effect:
A's `n3` ran 24/15/15, A's `mf1` 13/18/19. Three repetitions do not resolve an
effect of this size on these cases.

**Repo-map staleness.** B's listing is built once per task and does not track
files the agent creates. That is the design, not a defect, and was not treated
as one.

## 13. Verdict

**POSITIVE_SIGNAL.**

- Correctness is identical between arms — the hard gate holds.
- Aggregate rounds are lower in both aggregations (−10.6% over the three main
  cases, −5.3% with the large repository).
- The mechanism is directly observed and total: `list_files`-first went 12/12
  to 0/12.
- Input tokens fell despite the added prefix.

It is not CONFIRMED, because the protocol's strong criterion requires every
main case to have `B median ≤ A median` and `n3` did not (15 → 17). Per-case
direction is mixed on two of four cases, and wall clock gives no consistent
answer.

Recommendation: **KEEP**. The change is safe, cheap, correctness-neutral, and
its mechanism is proven. Report it as "removes the opening repository-discovery
round trip on this evaluation set, with roughly 5–11% fewer model rounds",
and stop quoting the single-run 17.9% / 14.7% figures.

Core Freeze: UNCHANGED.
