# Observability Product Closure

Phase D of the Beta Product Closure. The three-way comparative found that
machine-readable observation is one of CodeLeveler's real advantages over the
alternatives — and that most of it was reachable only by opening SQLite. This
makes the valuable part a product surface.

## Verdict

```text
OBSERVABILITY_PRODUCT_CLOSURE = PASS
CORE_FREEZE                   = UNCHANGED
```

No new ledger, no dashboard. One projection widened, and one truthfulness bug
found and fixed on the way.

## 1. What already existed

`leveler trace` reads the durable EventLog and `model_requests` and prints a
session header, a classified event window, per-tool aggregates, sub-agent rows
and recovery facts, with `--json` for machines. It already had: session id,
goal, repository, timestamps, status, model, axes, request count, input and
output tokens, average and last latency, request failures and retries, tool
started and finished counts, verification run count, compaction count and
sub-agent starts.

## 2. What was missing

Against §30's field list, five things the durable stores already held but the
projection dropped on the floor:

| field | where it lived | why it matters |
| --- | --- | --- |
| Duration | `sessions.created_at` / `updated_at` | "how long did this take" had no answer |
| Cached input tokens | `model_requests.cached_input_tokens` | 97.2% of a long run's input is cache; a raw input total misleads without it |
| Cost | `model_requests.cost_usd_micros` | never surfaced at all |
| Verification verdict | `verification_finished { passed }` | only the run count was reported, and a count is not a verdict |
| Per-lane spend | `model_requests.agent_id` | a reviewer's spend could not be separated from the work it reviewed |

## 3. What was added

`UiSessionObservation` gains `duration_ms`, `cached_input_tokens`,
`cost_usd_micros`, `verification` and `lanes`. `UiRequestObservation` gains
`cached_input_tokens`, `cost_usd_micros` and `agent_id`. Every one is read
from the durable store; nothing is scraped from stdout, parsed from prose, or
counted off the screen.

`verification` is the latest verdict actually recorded — `passed`, `failed`,
`not_run` when nothing ever started, and `unavailable` when something started
and never reached a verdict. A started-and-abandoned check is not a verdict and
does not pretend to be one.

`lanes` splits spend into `main` (the root session's own calls), `children`
(everything with an `agent_id`) and `total`. A lane with no requests is not
printed — a row of zeros would read as a measurement.

An unmeasured figure is `UNAVAILABLE` in the CLI and an em dash in the TUI,
never `0`. The storage layer already drew that line in its own doc comment —
`None` is an absence of measurement, `Some(0)` is a measurement — and the
projection now honours it: a lane's cached and cost totals stay `None` unless
at least one row carried the figure.

Against the 25-minute run from Phase C:

```
  duration 25m 20s
  requests 63   in 3242659  cached 3151360  out 57141  last_lat Some(6298)
  cost     UNAVAILABLE
  tools    started 81  finished 81   verify passed  agents 0
  main     requests 63  in 3242659  cached 3151360  out 57141  cost UNAVAILABLE
  total    requests 63  in 3242659  cached 3151360  out 57141  cost UNAVAILABLE
```

`cost` is `UNAVAILABLE` because the lab configures no pricing for this model.
That is the correct answer, and it is visibly different from `$0.0000`.

## 4. A truthfulness bug, found and fixed

The projection capped its request list at 200 rows for display — and then
computed the session's token totals over the capped list. A session past 200
model calls reported the token total of its **last 200 calls** as if it were
the whole session. The previous round's investigations ran sessions of 155 to
203 requests, so this was within reach of real work, not theoretical.

Accounting now runs over every row; only the display list is capped. The
regression test seeds 250 requests and asserts the totals cover all of them;
with the old code it reports 2,000 tokens where 2,500 were spent.

## 5. Where it surfaces

`leveler trace` (text and `--json`) and the TUI's observability screen, which
gains a `SPEND` row and one row per non-empty lane. Nothing was added to the
main conversation: §32 asks that the numbers be reachable when wanted, not
that they occupy the screen, and the existing screen already is that place.

## 6. Regression protection

| test | pins |
| --- | --- |
| `the_session_reports_cached_tokens_and_cost` | cached and cost sum from the durable rows; duration is present |
| `unmeasured_cache_and_cost_read_as_unavailable_not_zero` | an unrecorded figure stays `None` |
| `spend_is_split_between_the_root_session_and_its_children` | `agent_id` separates the lanes |
| `verification_reports_its_verdict_not_only_that_it_ran` | `not_run` → `unavailable` → `failed` → `passed`, latest wins |
| `accounting_covers_every_request_even_past_the_display_cap` | totals ignore the display cap |

The committed JSON schema was regenerated, so non-Rust clients see the new
fields rather than drifting from them.

## 7. Gate

| requirement | result |
| --- | --- |
| task duration truthful | PASS — 25m 20s against a 1,520.4 s measured wall |
| round / model request count truthful | PASS — no longer capped at 200 |
| token accounting truthful | PASS — bug found and fixed, regression test in place |
| cached tokens visible | PASS |
| verification visible | PASS — a verdict, not a run count |
| child accounting not falsely zero | PASS — an empty lane is omitted, not zeroed |

`OBSERVABILITY_PRODUCT_CLOSURE = PASS`.
