# Accumulated context is not the cost multiplier

Read-only audit. No code changed, no model run.

**Verdict: the growing transcript is nearly free, and every proposed remedy
would make it more expensive.** Append-only history plus a provider prefix
cache is the cheap shape for this workload; editing history is what costs.

```
TOOL_RESULT_CONTEXT_GROWTH_CLOSURE = REJECTED
SELECTED_CONTEXT_TREATMENT         = NONE
```

## The measurement that ends it

post-closure C3, 190 main-task rounds:

```
input throughput   13,635,090
  cached           13,442,816   98.6%
  UNCACHED            192,274    1.4%
output                242,736
```

Per-round, the cache behaves exactly like a prefix cache on an append-only
transcript:

| round | input | hit rate | uncached |
|---:|---:|---:|---:|
| 1 | 18,837 | 25.1% | 14,101 |
| 2 | 19,081 | 98.6% | 265 |
| 10 | 39,217 | 98.9% | 433 |
| 50 | 65,385 | 99.1% | 617 |
| 100 | 106,037 | 99.5% | 565 |
| 190 | 108,697 | 99.0% | 1,049 |

After the first round each request pays for **only the turn just appended** —
a few hundred tokens. The 108k context at round 190 costs about as much as the
19k context at round 2.

```
TOP_DYNAMIC_CONTEXT_SOURCE = tool results
TOOL_RESULT_REPLAY_MULTIPLIER ≈ 90x in throughput, ≈1x in uncached tokens
```

## Latency does not scale with it either

Median main-request latency, bucketed by context size, across five long C3 runs:

| context | n | median | p75 | p95 |
|---|---:|---:|---:|---:|
| 0–20k | 8 | 2.81s | 5.00s | 7.88s |
| 20–40k | 130 | 3.35s | 7.35s | 26.73s |
| 40–60k | 178 | 3.72s | 12.17s | 94.02s |
| 60–80k | 195 | 4.27s | 8.90s | 131.24s |
| 80–100k | 206 | 4.04s | 6.25s | 61.80s |
| **100k+** | 124 | **3.49s** | 7.05s | 26.89s |

From 20k to 100k+ the median moves 2.8s → 3.5s, and 100k+ is *faster* than
60–80k. The p95 tail is provider queueing, not prefill: it peaks in the middle
buckets and falls at the largest. Cached prefill is close to free.

## Why every candidate remedy is backwards

This run modified its history exactly once — the trim visible as input dropping
from 106k at R100 to 61k at R125. That single edit cost **14,422 uncached
tokens in one round: 7.5% of the entire run's uncached total.**

A prefix cache keys on a stable prefix. Eliding a tool result from the middle
of the transcript invalidates everything after it. So P1 (superseded-result
elision), P2 (reference replacement) and P3 (coalescing) each convert cheap
cached throughput into expensive uncached re-warms, repeatedly. There is 1.4%
of throughput to win and 98.6% to lose.

```
CANDIDATE_P1..P4 = REJECTED — each would break the prefix cache it is trying to help
TOOL_HISTORY_ELISION_PROTOCOL_SAFE = moot; the economics fail before the protocol question
```

Stages C–J were not run. Measuring reuse ratios, building a projection
simulator and designing an A/B are all downstream of a premise the first
measurement removed.

## The accounting correction this surfaced

`deepseek-v4-flash` has no `cached_input_usd_per_mtok`, and
`ModelPricing::cost_usd_micros_cached` then bills cached tokens at the **full
input rate** — deliberately, per its own comment: an invented discount would
understate every bill.

The consequence is that every cost figure in this investigation is an upper
bound, and at a 98.6% hit rate a very loose one:

```
recorded            $1.96   13.6M input all at $0.1389/Mtok
if cache bills at 10% of input   ≈ $0.28
```

The exact provider terms for this gateway are not known here, so the real
number is not asserted — only that the ledger is an upper bound and the gap is
roughly sevenfold. RCP-A made spend single-authority and auditable; it did not
make the price table complete, and that is a configuration gap rather than a
code one.

This matters beyond bookkeeping: cost comparisons across arms with different
cache hit rates are distorted by it, including the ones in the L1 confirmation.

## Where this leaves the program

Six levers examined, none survived:

```
quiet-round early closeout        REFUTED    truncates correct runs
delegation reconsideration        WEAKENED   adoption 0/2 after reoffer
independent-inspection batching   REJECTED   effect < control variance
shell investigation ergonomics    REJECTED   87% of it is composition
prompt serialization              REJECTED   ~2% of turns; its fix already failed
tool-result context growth        REJECTED   98.6% cached; remedies break the cache
```

The cost model that drove all six was `round_trips × accumulated_context`. That
is arithmetically true of **token throughput** and false of **billed cost and
latency**, because the prefix cache makes accumulated context nearly free.

What is actually left is narrower and harder:

- **Wall time is round-trip count × per-round latency.** ~190 trips at a ~3.5s
  median with a p95 of 27–131s. The tail is provider-side.
- **Round-trip count reduction has been attempted five ways and failed**, and
  RCP-D2 established that the long investigation those trips buy is how correct
  runs reach their fix.

So the honest position is that CodeLeveler's remaining wall-clock cost is
mostly the provider's per-request latency multiplied by a round count that the
task genuinely needs. That is not a bug with a lever behind it, and further searching for one should stop until something new is measured rather than assumed.

## Also still open, unrelated to FAST

```
STRUCTURED_READ_VS_SHELL_SCOPE_ASYMMETRY = OPEN_PRODUCT_SECURITY_QUESTION
```

`read_file` refuses paths outside the workspace; `shell_command` reading the
same path succeeds. Recorded in `SHELL_INVESTIGATION_ERGONOMICS.md`, untouched
here.
