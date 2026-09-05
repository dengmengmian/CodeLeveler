# Accounting Authority Closure

Work package 1 of the Fast / Good / Cheap engineering closure.

```
ACCOUNTING_AUTHORITY_CLOSURE=PASS

CACHED_TOKEN_PARSE=PASS
CACHED_TOKEN_PERSISTENCE=PASS
CACHED_TOKEN_PRICING=PASS
CHILD_BUDGET_AGGREGATION=PASS
CHILD_DURABLE_REQUEST_ATTRIBUTION=PASS
SESSION_USAGE_RECONCILIATION=PASS
COST_RECONCILIATION=PASS

ACCOUNTING_AUDITABLE=YES
CHEAP_MEASUREMENT_READY=YES
CHEAP_IMPROVEMENT_CLAIM=NOT_YET_ALLOWED
```

Phase C could not rank cost, and the reason was not the competitors' books —
it was ours. This closes that. It does not make anything cheaper; it makes the
bill readable.

**The first thing the readable bill said: CodeLeveler runs at a 92–96% prompt
cache hit rate.** That number did not exist before this change, and it is the
one that decides whether Phase C's 6–10M raw input tokens were a large bill or
a small one. They were a small one.

---

## 1. Four root causes, not one

Phase C recorded this as "accounting incomplete". It is four separate defects
with four separate fixes.

### `CACHED_TOKEN_PERSISTENCE_ROOT_CAUSE`

Parsing was never the problem. `TokenUsage::cached_input_tokens` is populated
by both protocol paths — `openai_chat/stream.rs` reads DeepSeek's
`prompt_cache_hit_tokens`, `anthropic_messages` reads `cache_read_input_tokens`
— and the agent-side `ModelRequestRecord` carries the whole `TokenUsage` to the
writer.

`leveler_storage::ModelRequestRecord` had no field for it. `turn.rs`'s
`record_model_request` copied `usage.input_tokens` and `usage.output_tokens`
into the row and dropped `usage.cached_input_tokens` on the floor, because
there was nowhere to put it. The value travelled the entire chain and died one
statement from the database.

### `CACHED_TOKEN_PRICING_ROOT_CAUSE`

`ModelPricing` held two rates. `cost_usd_micros(input, output)` charged every
prompt token at the uncached rate. At a 92% hit rate that overstates the bill
by roughly four times, which also meant a configured cost budget bound long
before the money was spent.

### `CHILD_REQUEST_ATTRIBUTION_ROOT_CAUSE`

A sub-agent runs as an owned `'static` future so a background delegation can be
spawned — which means it cannot borrow the parent's persistence sink. It was
given `SubAgentProgressSink` instead, whose `record_model_request` emitted a
`SubAgentProgress` event and accumulated three in-memory counters. Nothing
delegated to storage.

That is precisely why a Phase C reviewer's `↑486,369` appears in stdout and in
no table: the child's sink was a progress emitter, and the only sink that could
write was held by a parent the child could not reach.

### `SESSION_RECONCILIATION_ROOT_CAUSE`

No reconciliation existed, and `model_requests` had no cost column, so a
session's cost could not be summed from rows at all. The only cost figure
anywhere was `ProgressLedger.cumulative_cost_usd_micros` — an in-memory
counter with nothing to check it against.

## 2. What changed

### Migration 0022 — three columns where NULL is load-bearing

```sql
ALTER TABLE model_requests ADD COLUMN cached_input_tokens INTEGER;
ALTER TABLE model_requests ADD COLUMN cost_usd_micros INTEGER;
ALTER TABLE model_requests ADD COLUMN agent_id TEXT;
```

| | NULL means | 0 / value means |
| --- | --- | --- |
| `cached_input_tokens` | the writer did not record it (every pre-0022 row) | the provider reported that many cache-hit tokens, `0` included |
| `cost_usd_micros` | no pricing configured, or a pre-0022 row | priced at that amount, `0` included |
| `agent_id` | the root session's own call | the sub-agent that made it |

Backfilling zeros would assert facts nobody measured. Old rows stay NULL and
any total resting on one reports itself incomplete.

### Cache-aware pricing

`ModelPricing` gains `cached_input_usd_per_mtok: Option<f64>` and
`cost_usd_micros_cached(input, cached_input, output)`. Cached tokens are a
**subset** of input, never an addition, so the uncached remainder is the
difference and no token is billed twice. With no cached rate configured the
whole prompt bills at the input rate — the conservative reading, because a
harness that invents a discount understates every bill.

`cost_usd_micros(input, output)` remains, delegating with `cached = 0`.

### Child attribution over the channel that already exists

The child's sink now also emits `AgentEvent::SubAgentModelRequest { record }`,
stamped with its own agent id. The parent's drive loop intercepts it at every
point it drains a child's progress channel and writes the row through the sink
the child could not borrow:

```rust
macro_rules! forward_child_event {
    ($event:expr) => {{
        let event = $event;
        if let AgentEvent::SubAgentModelRequest { record } = &event {
            sink.record_model_request(record).await?;
        } else {
            observer(event);
        }
    }};
}
```

It is not forwarded to the observer: the live progress line already carries the
child's running totals, and this event exists for the ledger, not the screen.
The pattern is the one `handlers.rs` already uses to land a child's ownership
transitions on the parent's durable channel.

### Reconciliation from the rows

`ModelRequestRepository::reconcile_session` sums a session's rows and splits
them into the root's own calls and each agent's, counting separately the rows
that contributed no cached figure and no cost. `SessionUsageTotals::is_complete`
is what lets a caller showing a cost say whether it is the whole bill.

### One behaviour-adjacent correction, stated plainly

The runtime's own `cost_spent_micros` was still pricing without the cache
discount. It now uses `cost_usd_micros_cached`. This is inside the work
package's pricing scope and it does change one thing: a configured
`max_cost_usd_micros` now binds on the real bill instead of one roughly four
times too large. Nothing else about agent behaviour moved.

## 3. Evidence from a real run

A scratch repository, one Python bug, an instruction to have a reviewer
sub-agent check the fix. `deepseek-v4-flash`, cache-aware pricing configured
(`0.1389` uncached, `0.0139` cached).

```
agent                                 kind      reqs   input  cached  output   cost
──────────────────────────────────────────────────────────────────────────────────
(root)                                advisory     4    4250    1280   16896   5123
(root)                                round       37  818969  793600   18188  19607
37de156f-9764-4f70-bf65-a9618c6d3c5f  round        7   85309   82176    6854   3482
4355dd6b-4e62-4aac-82b2-d2b5ae3ee707  round        7   84455   75392    9053   4822
51b6c879-d8a1-40b4-8733-615a57f01027  round       12  146664  142464   13017   6180
706c26fc-410b-421c-9fbc-746f3f744427  round        8  100899   92672   10771   5422
a3d6ce73-ca53-4cce-bef1-b74103c5c6ba  round       13  161759  158080    7832   4884
e76a7fc9-8988-4859-8cf2-5974f06870bc  round       10  125282  120960    8032   4514

cache hit rate 96.0%       rows_without_cached_usage 0       rows_without_cost 0
```

Six sub-agents, 57 of the 98 calls, every one of them a row. In Phase C the
same position held zero rows.

### Reconciliation

```
                     rows          runtime ledger        delta
tokens            1,528,273           1,507,127         21,146
cost (micros)        52,523              47,400          5,123

advisory rows: tokens 21,146 · cost 5,123
```

**Both deltas are exactly the advisory lane, to the token.** That is the whole
difference, and it has a name.

```
DIFFERENCE_CATEGORY=advisory_lane_absent_from_runtime_cumulative_counters
REASON=`model_tokens_spent` and `cost_spent_micros` accumulate inside the round
       loop only. Contract derivation and the reconciliation judge are provider
       calls that get billed, are written as rows, and never reach either
       counter.
AMOUNT=21,146 tokens · 5,123 micro-USD in this session
DIRECTION=the rows are MORE complete than the ledger, not less
```

Folding the advisory lane into those counters would change what a token budget
binds on, which is an agent-behaviour change and out of this work package.
Recorded as a finding for a later one.

Before the pricing correction the cost delta on a comparable session was
**104,421 micro-USD** — the ledger charging 140,056 where the true
cache-aware bill was 35,635. After it, cost reconciles to the same single
category as tokens.

### Backward compatibility

Verified against a real pre-0022 database: the `sessions.db` a Phase C formal
run left behind, 113 rows, migration 0021 schema.

```
before   113 rows · id … kind provider_request_id
after    113 rows · + cached_input_tokens cost_usd_micros agent_id
         all 113 read NULL in all three — unknown, not zero
         6,392,866 input tokens intact · old session still lists
DATABASE_BACKWARD_COMPATIBILITY=PASS
```

No user has to delete `~/.leveler`.

## 4. Tests

| | | |
| --- | --- | --- |
| T1/T2 | usage and cached share round-trip exactly | `a_request_round_trips_its_usage_exactly` |
| T3/T4 | a child's call is attributable to its agent | `a_child_request_is_attributed_to_its_agent` |
| T5/T9 | root + agents sum without double counting | `reconciliation_sums_root_and_children_without_double_counting` |
| T6 | a pre-0022 file upgrades in place, rows readable | `a_pre_0022_database_upgrades_in_place_and_keeps_its_rows` |
| T7 | an unrecorded row is unknown, not zero | `a_pre_migration_row_is_unknown_usage_not_zero_usage` |
| T8 | cache-aware cost; unknown rate invents no discount; cached > input clamps | three tests in `profile.rs` |
| — | the child sink emits a record stamped with its id | `a_child_sink_emits_the_record_stamped_with_its_agent_id` |
| — | every child call is its own record | `every_child_call_produces_its_own_record` |
| — | pricing applied once, from the usage carried | `a_record_is_priced_from_the_usage_it_carries` |

```
FMT=PASS
CLIPPY=PASS   (workspace, all-targets, all-features, -D warnings)
FULL_PRODUCT_TEST_GATE=PASS   3705 tests, 135 result lines, 0 failures
GOOD_NON_REGRESSION=PASS      dogfood fixed the bug, added a test, spawned a
                              reviewer, ended CompletedUnverified as before;
                              no new false Verified
```

## 5. Limitations

**The advisory lane is missing from the runtime's cumulative counters.**
Quantified above, direction known, fix deferred because it moves a budget
boundary.

**Historical cost is not recomputed.** A row carries the cost computed at the
time it ran, from the pricing then configured. Rows written before 0022 have
none and will not acquire one; re-deriving them from today's price table would
be a guess dressed as a record.

**Cached-token semantics are canonical, not per-provider-verified.** The
harness treats `cached_input_tokens` as a subset of `input_tokens`, which is
how DeepSeek's `prompt_cache_hit_tokens` and Anthropic's
`cache_read_input_tokens` both report. A provider that reports it as an
addition would be double-counted; none of the currently supported ones does.
Anthropic's cache *creation* tokens are not modelled separately.

**Cost is an estimate, not a bill.** It is usage times configured pricing. No
provider invoice is reconciled against it.

## 6. What this does and does not license

```
CHEAP_MEASUREMENT_READY=YES
CHEAP_IMPROVEMENT_CLAIM=NOT_YET_ALLOWED
```

Nothing here reduces token use. It is now possible to say what a run cost and
where the cost went, which is the precondition for the next two work packages
claiming anything at all — a reviewer optimisation that saves "X tokens" was
unverifiable before this, and is verifiable after it.

The 92–96% cache hit rate is worth carrying forward. Phase C left CHEAP as
`NO_VALID_RANKING` partly because CodeLeveler's 6–10M raw input tokens might
have meant a bill four to twenty times AtomCode's, or roughly the same. On this
evidence the effective rate is in the same band as AtomCode's 90–97%, so the
cost gap is much closer to the model-request-count ratio than to the raw token
ratio. That is a measurement, not yet a competitive claim: it comes from a
scratch dogfood, not from a frozen three-way cohort.

```
NEW_FORMAL_TREATMENT_FROZEN=NO
NEXT_ENGINEERING_WORK_PACKAGE=REVIEWER_EFFICIENCY_POLICY_COMPOSITION_CLOSURE
```
