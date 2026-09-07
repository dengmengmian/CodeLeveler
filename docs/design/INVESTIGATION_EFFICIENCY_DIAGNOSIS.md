# Why a correct CodeLeveler investigation costs so many round trips

Nine runs read off durable transcripts and model-request ledgers: seven
mechanically-correct CodeLeveler runs (the three formal ones also ACCEPTed
under blind review) and two known-failure runs. Every number is a count, a
length, or a string comparison — nothing is inferred from model output.

The question is not why the runs are long. RCP-D2 established that long silent
investigation is how these runs reach their fix, and that cutting it early
truncates correct work. The question is why the same investigation costs so
many model ↔ tool round trips.

## Verdict, after a clean A/B

The batching lever was implemented, measured twice, and **rejected**. The
second measurement — contemporaneous, interleaved, latency-matched — refuted
the first. What follows records both, because the difference between them is
the more useful lesson.

```
L1_BATCHING_CONFIRMATION = REJECTED
L1_BATCHING_AS_PRIMARY_FAST_LEVER = REJECTED
```

### The confirmation cohort

14 runs, alternating arms within the same hours (`C T T C` / `T C C T` /
`C T C T T C`), one machine, serial, same model and gateway, frozen engineering
timeouts (C1/C2 3600s, C3 5400s; the formal 3600s contract untouched).

```
LATENCY_PARITY = YES     control median 4.7s (4.2–5.5) · treatment 4.9s (3.7–5.7)
```

The arms sat in one latency regime. That is what the earlier attempt could not
say, and it is what makes this result readable.

| | control (n=7) | treatment (n=7) |
|---|---|---|
| GOOD | 6/7 | 5/7 |
| multi-tool turn ratio | **16.4%** (6.5–30.8) | **21.2%** (12.2–28.6) |
| tools per tool-using turn | 1.19 (1.07–1.39) | 1.22 (1.13–1.36) |
| main-task requests | **91** (39–200) | **119** (36–151) |
| input tokens | 5.67M | 7.03M |
| cost | **$0.82** | **$1.01** |
| wall | **1857s** | **2314s** |
| timeouts | 0/7 | 1/7 |

Paired by task, only C3 moved in the hypothesised direction:

| task | multi% C→T | requests C→T | cost C→T |
|---|---|---|---|
| C1 | 28.3 → 26.6 | 57 → 78 | $0.47 → $0.62 |
| C2 | 18.7 → 17.9 | 79 → 144 | $0.69 → $1.20 |
| C3 | 12.7 → 16.1 | 165 → 98 | $1.86 → $0.99 |

Requests, cost and wall all moved the **wrong** way at the median. The ~40%
round reduction reported from the first attempt does not reproduce.

### Why the first reading was wrong

**The control's own batching varies more than the treatment changes it.**
Untreated runs range from 6.5% to 30.8% multi-tool turns. The treatment shifts
the median by about five points into a distribution that already spans
twenty-four. At n=7 per arm the effect is smaller than the noise it sits in.

The original diagnosis anchored on post-closure C3 at 1.6% multi-tool and
called single-tool turns a stable 73–98% property. It is not stable. That run
was at the far tail of a wide natural distribution, and building a lever on it
measured the tail, not the property.

### What the diagnosis still gets right

Unchanged by any of this, because it is arithmetic inside single runs:

- Cost is round-trip count × accumulated context. formal C3 carried 445 KB of
  tool results and spent 10.2M input tokens — the transcript is re-sent about
  ninety times.
- Assistant prose is 0.6–5.1% of the transcript. Reasoning volume is not the
  amplifier.
- 50–88% of wall is model latency at a per-round-trip cost that does not depend
  on how much work the round carries.
- Long silent investigation is how correct runs reach their fix (RCP-D2), so
  stopping earlier is not available.

The cost structure is real. **Telling the model to batch is not a lever on it.**

### What this rules out, and what is left

Three levers have now been measured and none survived:

```
quiet-round early closeout      REFUTED    (truncates correct runs)
delegation reconsideration      WEAKENED   (adoption 0/2 after reoffer)
independent-inspection batching REJECTED   (effect < control variance)
```

All three tried to change what the model *chooses* to do. The remaining
directions change what a choice *costs*:

1. **Shell ergonomics.** C3 routes 68–70% of its shell calls at text search,
   file discovery and source inspection — and `shell_command` is not
   parallel-safe, so that work cannot batch even in principle. This is a
   capability gap, not an instruction gap.
2. **Tool-result and context growth.** The secondary amplifier, untouched.

Neither has been attempted. Both are mechanical rather than persuasive, which
on this evidence is the distinction that matters.

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

## The experiment — run, measured, and rejected

The lever below was implemented (f438664), measured against the frozen-binary
protocol, and **reverted**. What follows is what it actually did.

Control is every mechanically-scored CodeLeveler run available: formal C1/C2/C3,
post-closure C1/C2/C3, pre-D C3. Treatment is C1x2, C2x2, C3x3 on the same
model, gateway, machine, task pins and oracles.

| task | arm | mech | main rounds | wall | cost |
|---|---|---|---:|---:|---:|
| C1 | control formal | PASS | 108 | 1872s | n/a |
| C1 | control post-c | PASS | 110 | 1444s | $0.95 |
| C1 | **treatment r1** | PASS | 39 | 828s | $0.23 |
| C1 | **treatment r2** | PASS | 89 | 1006s | $0.68 |
| C2 | control formal | PASS | 185 | 3485s | n/a |
| C2 | control post-c | PASS | 106 | 1727s | $0.87 |
| C2 | **treatment r1** | PASS | 88 | 1564s | $0.81 |
| C2 | **treatment r2** | **FAIL** | 88 | 3600s (timeout) | $0.70 |
| C3 | control formal | PASS | 159 | 2384s | n/a |
| C3 | control post-c | PASS | 190 | 3034s | $2.03 |
| C3 | control pre-D | PASS | 127 | 3380s | $1.28 |
| C3 | **treatment r1** | **FAIL** | 12 | 219s | $0.05 |
| C3 | **treatment r2** | PASS | 127 | 3600s (timeout) | $1.64 |
| C3 | **treatment r3** | PASS | 53 | 1076s | $0.41 |

```
GOOD:  control 7/7 PASS  ->  treatment 5/7 PASS
```

That is the whole decision. GOOD is the first-priority gate and it dropped, so
the change is reverted regardless of what else improved.

**What else improved, because it is worth keeping on the record.** On the runs
that succeeded, every efficiency axis moved the right way:

| task | rounds (control -> treatment, medians) | wall | cost |
|---|---|---|---|
| C1 | 109 -> 64 (-41%) | 1658s -> 917s (-45%) | $0.95 -> $0.46 (-52%) |
| C2 | 145 -> 88 (-40%) | 2606s -> 1564s (-40%) | $0.87 -> $0.81 (-7%) |
| C3 | 159 -> 90 (-43%) | 3034s -> 2338s (-23%) | $1.66 -> $1.03 (-38%) |

So the lever is not inert and it is not merely cosmetic: when a run lands, it
lands in roughly 60% of the rounds. The problem is that two of seven did not
land, against zero of seven for control.

**Honest reading of the significance.** Two failures out of seven against zero
out of seven is not statistically distinguishable at this sample size. This is
not "the prompt was proven harmful"; it is "the safety gate was not cleared,
and the gate is the one that comes first". A larger cohort could show the
failures were variance. It could equally show them to be real. Shipping on the
strength of the FAST column while that is open would be exactly the trade the
gate exists to prevent.

**Two failure shapes, both "did not land the fix":**
- C3 r1 gave up after 12 rounds having changed nothing.
- C2 r2 ran to the 3600s wall-clock timeout without a correct tree.

Timeouts: treatment 2/7, control 0/7. Whatever else batching does, it did not
make these runs finish sooner in the way the round counts suggest it should.

**A correction to an earlier claim in this document's first draft.** The
batching shift was stated as "1.6% -> 25% multi-tool turns". That compared
against post-closure C3, which at 1.6% is the most extreme control run in the
set. The honest comparison is medians: **14.5% -> 21.2%**. The mechanism does
work, and the effect is real, but it is a modest shift and not the near-total
one that number implied.

## What would make this shippable

The efficiency gain is large enough to be worth another attempt, but not as an
unconditional prompt rule. Directions, in order of how well this data supports
them:

1. **Find out whether the failures are the lever.** Seven more treatment runs
   would settle whether 5/7 was variance. That is the cheapest decisive step and
   nothing should be redesigned before it.
2. **Constrain when the rule applies.** Both failures are long/complex runs. A
   rule that only offers batching for read-only inspection *after* the model has
   a plan, or only below some transcript size, keeps the gain where it was
   measured and removes it where the failures were.
3. **Close the shell ergonomics gap first** (below). Work routed through
   `shell_command` cannot be parallelised at all, so a large share of what the
   prompt asks for is unreachable today. Fixing that changes what batching can
   even do, and should probably precede a second attempt at the prompt.

## The reverted lever

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
