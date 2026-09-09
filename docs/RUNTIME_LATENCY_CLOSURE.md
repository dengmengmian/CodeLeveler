# Runtime Latency Closure

Measured 2026-09-09 against `23afdc8`.

## Verdict first

Wall-clock time is `model round trips × ~3.8s`, and ~80% of each round trip is
the provider's time-to-first-byte. The runtime accounts for under 9% of wall.

The levers this investigation set out to build — a smaller Balanced tool
surface, a pre-request context budget, tighter tool-result caps — all reduce
**token volume**. Token volume is 92.3% prefix-cache hits and barely moves
latency. They were not implemented, because the measurement says they would
buy under 3% while risking extra round trips, and an extra round trip costs
more than everything they save.

## 1. Problem

A user-visible turn reported `等待模型 · 12m22s · ↑109,047 ↓213`. The input
figure had no attribution: it could have been two enormous requests or twenty
ordinary ones, mostly tool schema or mostly transcript, parent or child.

## 2. Baseline environment

| | |
| --- | --- |
| Machine | Apple Silicon (arm64), macOS 25.6.0 |
| Binary | `cargo build --release` (a debug build would distort runtime share) |
| Model | `deepseek/deepseek-v4-flash`, `reasoning_effort = max` |
| Provider | Taotoken OpenAI-compatible gateway (`taotoken.net`) |
| Context | `context_window` 1,048,576 · `reliable_context` 786,432 |
| Tool calls | `parallel_tool_calls = false`, `max_parallel_tool_calls = 1` |
| Permission | Assisted |
| Cases | 7 (3 smoke, 3 core, 1 navigation) |
| Git | `23afdc8`, tree clean except the operator's untracked eval baselines |

## 3. Latency ground truth

The runtime already emits per-round telemetry from
`crates/leveler-agent-core/src/model_round.rs`: request id, connect time, first
event, total, finish reason, and input / cached / output tokens. No new
telemetry was needed to answer the question; it needed to be read.

Across the 7 cases — 84 model requests:

| | |
| --- | --- |
| Input tokens | 1,852,048 |
| Cached input | 1,710,336 (92.3%) |
| Uncached input | 141,712 (7.7%) |
| Output tokens | 10,119 |
| Average request input | 22,048 |
| Largest request input | 32,416 |
| Median time-to-first-byte | 2,647 ms |
| Median round total | 2,980 ms |
| TTFB share of a round | 79.8% |
| Total model wait | 287.8 s |
| Total wall | 315.2 s |
| Model wait share of wall | 91.3% |
| Tool calls per round | 0 on 5 rounds, 1 on 79. Never 2. |
| Compactions | 0 (largest request 32k against a 786k reliable context) |

Per case, wall time is almost exactly linear in round count:

| case | wall | rounds | s/round | input |
| --- | --- | --- | --- | --- |
| rust-mul | 18.4 s | 6 | 3.1 | 104,733 |
| go-triple | 22.3 s | 6 | 3.7 | 104,962 |
| rust-first-even | 22.3 s | 7 | 3.2 | 123,449 |
| rust-utf8-truncate | 26.7 s | 6 | 4.5 | 106,047 |
| go-json-defaults | 27.1 s | 7 | 3.9 | 123,614 |
| ts-deep-merge | 29.0 s | 7 | 4.1 | 122,910 |
| n1-unknown-location | 169.4 s | 45 | 3.8 | 1,166,333 |

Median wall 26.7 s, p90 29.0 s (excluding the 45-round navigation case, which
failed on understanding, not latency).

### Does input size drive latency?

This is the question that decides every token-reduction lever. It does not:

| total request input | n | median TTFB |
| --- | --- | --- |
| 15–20k | 44 | 2,472 ms |
| 20–25k | 13 | 2,679 ms |
| 25–32k | 27 | 2,876 ms |

Doubling the request costs 16% more TTFB. Correlation is +0.186.

Uncached tokens — the only ones a smaller prefix would actually remove — are
weaker still:

| uncached input | n | median TTFB |
| --- | --- | --- |
| 0–500 | 41 | 2,542 ms |
| 500–2,000 | 34 | 2,718 ms |
| 10,000–13,000 | 7 | 2,708 ms |

A request carrying 13,000 uncached tokens costs 6% more first-byte latency
than one carrying under 500. TTFB is a fixed ~2.5 s of provider and model time,
not prefill.

### The fixed prefix

Measured directly (now guarded by tests):

| block | bytes | ≈ tokens |
| --- | --- | --- |
| System prompt | 21,241 | ~5,300 |
| System prompt with the plan block | 22,788 | ~5,700 |
| Tool schema, core (14 tools) | 13,118 | ~3,300 |
| Tool schema, full (43 tools) | 28,758 | ~7,200 |

Round 1 of `rust-mul` pays 13,097 uncached tokens, which is the system prompt
plus the full tool schema almost exactly. Every later round pays 200–900.

## 4. Root causes, ordered

### Root cause #1 — round-trip count, at ~3.8 s each

Evidence: wall/round is 3.1–4.5 s across every case, including the 45-round
one. 91.3% of wall is model wait. Nothing else is within an order of magnitude.

### Root cause #2 — provider time-to-first-byte, ~2.5 s of every round trip

Evidence: TTFB is 79.8% of round total; median 2,647 ms; nearly flat against
input size. Streaming the answer takes ~330 ms. This is the model's own
thinking (`reasoning_effort = max`) plus gateway overhead, and the runtime
cannot shorten it.

### Root cause #3 — ceremony rounds

Evidence: 5 of 84 rounds produced no tool call. A traced `rust-mul` run shows
round 5 is a closeout nudge — the model wrote its final text without calling
`update_goal(complete)`, so the runtime spent a whole 2.6 s round trip asking
for it:

```
round 1  read_file     ttfb 3071  total 3791
round 2  list_files    ttfb 1961  total 2272
round 3  read_file     ttfb 2328  total 2633
round 4  apply_patch   ttfb 2893  total 4338
round 5  (no tool)     ttfb 2264  total 2620   closeout decided action=NudgeOnce(GoalUnresolved)
round 6  update_goal   ttfb 2308  total 2627
```

6% of rounds, ~6% of wall.

### Root cause #4 — serial exploration on a model that cannot batch

Evidence: `calls=1` on every one of the 79 tool rounds. The configured model
sets `max_parallel_tool_calls = 1`, so `read_file → list_files → read_file`
is three round trips (8.7 s) to look at a two-file crate. The executor already
runs independent read-only calls concurrently when the model emits them
together; this model never does.

## 5. What was not built, and why

| planned lever | measured effect | decision |
| --- | --- | --- |
| Balanced → core tool surface | saves 15.6 KB ≈ 3,900 tokens/request, 92.3% of which are cached; ~0.1 s/round at the measured TTFB slope, and one extra `expand_tools` round would cost 3.8 s | not implemented |
| Pre-request context budget | 0 compactions occurred; largest request 32k against a 786k reliable context | no observable target |
| Tool-result budget | tool results are cached growth; cutting them risks re-reads, and one re-read round costs more than the tokens saved | not implemented |
| Parallel independent reads | structurally unavailable: `max_parallel_tool_calls = 1` for this model. A prompt rule to batch was already A/B-rejected twice in this repo (requests 91→119, wall 1857→2314 s) | not implemented |

Two of these had already been measured and rejected in earlier work; see
`docs/design/CONTEXT_GROWTH_CLOSURE.md` and
`docs/design/INVESTIGATION_EFFICIENCY_DIAGNOSIS.md`. This run reproduces the
central fact independently: the input is a cache hit 92.3% of the time.

Cutting context is not merely low-value here. Because the provider's cache
serves the stable prefix, **moving work out of round trips and into the cached
prefix is close to free, and moving it the other way is expensive.** Any future
lever should be sized against round-trip count, not tokens.

## 6. Changes made

| file | change |
| --- | --- |
| `crates/leveler-tools/src/registry.rs` | test: the advertised tool surface stays within its measured byte budget |
| `crates/leveler-agent/src/prompt.rs` | test: the system prompt stays within its measured byte budget |

Both are tripwires on the fixed per-session prefix, so a large addition is a
decision rather than a surprise. No runtime behaviour was changed.

## 7. Correctness

No production code changed, so no correctness gate moved. The baseline run
itself: 6/7 cases passed, 100% completion accuracy, 0 false completions, loop
rate 0%. The one failure (`n1-unknown-location`) is an understanding failure —
the agent edited the wrong file behind a registry indirection — not a latency
or safety regression.

## 8. Explaining a 109k-input turn

For a 6-round task on this configuration:

| | |
| --- | --- |
| Model requests | 6 |
| Average request input | ~17,500 |
| Largest request input | ~17,800 |
| Cached | ~92% |
| Uncached | ~13,100 on round 1, 200–900 after |
| Tool schema contribution | ~7,200 tokens of every request |
| System prompt contribution | ~5,700 tokens of every request |
| Compaction requests | 0 |
| Child / reviewer requests | 0 (reviewer is Off by default) |
| Longest single wait | ~4.3 s |

So 109k input across a turn is not one enormous request. It is the same ~18k
context re-sent six times, 92% of it served from cache. The number is large
because it is counted per request, and it is nearly free.

## 9. Largest remaining latency source

Provider time-to-first-byte: 2,647 ms median, 79.8% of every round trip,
essentially independent of what CodeLeveler sends. The runtime's own share of
wall is 8.7%.

## 10. Next levers, in evidence order

1. **Remove the closeout nudge round.** Get `update_goal(complete)` into the
   same round as the final text via the completion contract in the prompt, not
   by weakening the gate. Worth ~6% of wall, and it is runtime ceremony rather
   than model work.
2. **Give the first request a repo map.** `list_files` cost a full round trip
   on a two-file crate. Because the prefix is cached, putting a bounded tree in
   it is nearly free, and it removes a round trip worth 3.8 s.
3. **Re-check `max_parallel_tool_calls` for this model.** It is 1 in the
   operator's config. If the model can in fact emit independent read-only calls
   together, the executor already runs them concurrently, and the three-round
   exploration above collapses to one.

Each of these removes round trips. None of them removes tokens.
