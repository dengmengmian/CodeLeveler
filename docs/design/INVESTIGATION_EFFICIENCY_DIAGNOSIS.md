# Why a correct CodeLeveler investigation costs so many round trips

Nine runs read off durable transcripts and model-request ledgers: seven
mechanically-correct CodeLeveler runs (the three formal ones also ACCEPTed
under blind review) and two known-failure runs. Every number is a count, a
length, or a string comparison — nothing is inferred from model output.

The question is not why the runs are long. RCP-D2 established that long silent
investigation is how these runs reach their fix, and that cutting it early
truncates correct work. The question is why the same investigation costs so
many model ↔ tool round trips.

## The finding

| run | correct | turns | 1-tool turns | multi | median tools/turn | tool calls |
|---|---|---:|---:|---:|---:|---:|
| formal C1 | ✓ | 108 | 85.0% | 15.0% | 1 | 131 |
| formal C2 | ✓ | 185 | 90.8% | 9.2% | 1 | 203 |
| formal C3 | ✓ | 159 | 85.5% | 14.5% | 1 | 182 |
| post-closure C1 | ✓ | 110 | 84.5% | 15.5% | 1 | 127 |
| post-closure C2 | ✓ | 106 | 73.3% | 26.7% | 1 | 134 |
| post-closure C3 | ✓ | 190 | **98.4%** | 1.6% | 1 | 193 |
| pre-D C3 @47c2d7d | ✓ | 127 | 91.3% | 8.7% | 1 | 138 |
| prune-ab (failed) | ✗ | 100 | 86.0% | 14.0% | 1 | 114 |
| prune-ab (failed) | ✗ | 100 | 83.0% | 17.0% | 1 | 120 |

**The median tool-using turn issues exactly one tool call. In all nine runs.**
A round trip buys about 1.15 tool calls. Correct and failed runs are
indistinguishable on this axis, which is what makes it an efficiency property
rather than a quality signal — and what makes batching unlikely to cost GOOD.

## Where the tokens go

Assistant prose is 0.6–5.1% of the transcript. Reasoning volume is not the
amplifier.

formal C3 carried 445 KB of tool results — roughly 111k tokens of content — and
spent **10.2M input tokens** across 159 requests. The content is sent about
ninety times over. Per-round input grows 17k → 110k as results accumulate; the
number of times that transcript is re-sent is the multiplier on top of it.

```
PRIMARY_INVESTIGATION_COST_AMPLIFIER = MODEL_ROUND_TRIP_COUNT
SECONDARY_AMPLIFIER                  = TOOL_RESULT_CONTEXT_GROWTH
```

## Where the wall time goes

| run | wall | model latency | share | rounds | mean latency |
|---|---:|---:|---:|---:|---:|
| post-closure C1 | 1444s | 785s | 54% | 110 | 7.1s |
| post-closure C2 | 1727s | 1511s | 88% | 106 | 14.3s |
| post-closure C3 | 3034s | 2250s | 74% | 190 | 11.8s |
| pre-D C3 | 3380s | 1705s | 50% | 127 | 13.4s |

```
PRIMARY_WALL_AMPLIFIER = model latency × round-trip count
```

A round trip costs 7–14 seconds regardless of how much work it carries. Cutting
round trips cuts wall close to proportionally.

## How much is actually batchable

Two bounds, both mechanical.

**Lower (3–6%.)** A single-call read/search turn whose argument already appears
in the transcript *before* the previous tool result — so that result cannot
have supplied it. Deliberately strict, and certainly an undercount: it cannot
recognise a model that already knew which three files it wanted.

**Upper (28–47%.)** Maximal runs of consecutive turns that emitted *only*
inspection work — reads, searches, and read-shaped shell. Nothing inside such a
run mutates or builds, so collapsing it cannot reorder a write against a read.
Batching those three-at-a-time saves:

| run | longest inspection-only runs | round trips saved (K=3) |
|---|---|---:|
| formal C1 | 30, 14, 9, 9, 7 | 46 (42.6%) |
| formal C2 | 25, 19, 18, 12, 10 | 81 (43.8%) |
| formal C3 | **71**, 14, 8, 8, 6 | 73 (45.9%) |
| post-closure C1 | 26, 9, 7, 3, 3 | 34 (30.9%) |
| post-closure C2 | 36, 11, 7, 5 | 38 (35.8%) |
| post-closure C3 | 45, 33, 21, 8, 7 | 90 (47.4%) |

formal C3 contains a run of **71 consecutive turns** doing nothing but
inspection, one call at a time.

## The capability was already there

- The executor already accepts several tool calls in one response and runs them
  concurrently, bounded by `max_parallel_tools`.
- Concurrency is gated by `Tool::supports_parallel()`, which is `false` by
  default and `true` on exactly the read-only inspection tools. Writes and
  shell run serially **by construction** — the prompt cannot cause a
  write/read reorder even if the model batches badly.
- `parallel_tool_calls` defaults to true in the config path, so the wire flag is
  omitted and the provider's native behaviour applies.
- The prompt said nothing about any of it. Its only occurrence of "batch" is
  `never batch-complete`, about plan status.

So the runtime has always been able to do this and the model was never told.

## Secondary: tools reached for through the shell

C3 runs perform 68–70% of their shell calls as text search, file discovery or
source inspection — 65 shell text-searches against 3 `grep` tool calls in
post-closure C3. Structured search exists and is parallel-safe; shell is not.

```
TOOL_ERGONOMICS_GAP = SUPPORTED
```

Not this experiment's lever. It compounds the primary one — work routed through
shell is work that cannot be parallelised — and is the natural follow-up if
batching alone under-delivers.

## The experiment

One lever, one sentence, in `operating_rules`:

> When you already know several independent inspection targets — files to read,
> patterns to search, directories to list — issue those calls together in one
> tool-call turn rather than one per turn. This applies only to reads and
> searches whose arguments you already have: anything that needs a previous
> result to construct, and anything that writes, belongs in its own turn.

No speed exhortation: "be faster" cannot be verified and would licence skipping
work to look quick. The condition is the part that matters, and a test pins it.

A/B is the frozen-binary protocol: control is `47c2d7daaa93`, treatment is this
build, same model, gateway, machine, task pins and oracles.

## Reproducing

```
python3 investigation_profile.py <sessions.db> [label] [--json]
```
in `dogfood/eval/phase-c/`, reading only durable logs.
