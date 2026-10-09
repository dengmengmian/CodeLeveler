# Context Statistics Contract v1

One frozen corpus for the **compaction-axis** context statistics every client
renders. The terminal, the Web client and the desktop read these cases; the
layout differs per surface, the semantics do not.

## What each case carries

| Field | Meaning |
| --- | --- |
| `accounting` | The runtime's `ContextAccounting` for the next request, or `null` when the runtime holds none (a fresh process with no assembled request). |
| `provider.input` / `provider.output` | The provider-reported usage of the last request. A DIFFERENT measurement from the projection. |
| `model_window` | The declared model window from `SessionBootstrap`, when known. |
| `expect.axis` | `compaction` (effective input capacity resolved), `window` (no capacity, projected input), `window_provider` (no accounting, provider usage), `none` (nothing but a bare provider number). |
| `expect.used_tokens` | The projected input the surface must show as the numerator on the compaction axis. |
| `expect.input_capacity_tokens` | The effective input capacity (`window − reservation − headroom`). |
| `expect.compact_at_tokens` | The runtime's resolved soft threshold. The UI must never re-derive it. |
| `expect.hard_capacity` | The hard bound the fold classifier compares against. |
| `expect.fold_state` | `FoldRequirement::classify`'s decision, taken verbatim. |
| `expect.provider_actual` | Provider actual usage; it may be named beside the projection, never spliced into it. |
| `expect.render` | Surface-specific expectations: the exact terminal footer chip and Web label, the axis word, and whether the surface must name the provider number or “statistics unavailable”. |

## Invariants

1. Compaction utilization is **projected input ÷ effective input capacity**.
   Never `provider input + output ÷ model window`.
2. The soft and hard numbers are the runtime's own resolved policy values; the
   client computes no threshold, constant or percentage policy.
3. With no resolved capacity the model window is shown as its **own named
   axis**, never as a compaction percentage.
4. When the runtime holds no accounting, statistics are shown as unavailable;
   the provider's actual usage may only be labelled as provider usage on the
   model-window axis.

## Coverage

Stateless accounting semantics (cases CS1–CS9): different windows, different
output reservations, the 85 % and 95 % soft thresholds, the hard capacity, a
provider/projection difference, and statistics unavailable with and without a
declared window.

Lifecycle semantics are about state transitions rather than one snapshot, so
each surface pins them in its own event tests:

| Scenario | Terminal | Web | Desktop |
| --- | --- | --- | --- |
| Model switch drops the old policy's accounting | `reducer::context_refresh_tests` | `controller.test.ts` | stateless (no session-scoped meter) |
| Compaction drops the accounting and the fallback | `reducer::context_refresh_tests` | `controller.test.ts` | stateless |
| Resume asks the runtime at once | `reducer::context_refresh_tests` | `controller.test.ts` | reads on opening the Context page |
| A delayed/foreign answer is ignored | `reducer::context_refresh_tests` | `controller.test.ts` | `state.mjs::applyReadResult` (`query_id`) |

## Consumers

- `crates/leveler-tui/src/context_statistics_contract.rs` — the footer chip.
- `crates/leveler-web/web/src/lib/contextStatisticsContract.test.ts` — the meter.
- `apps/leveler-desktop/test/contextStatistics.test.mjs` — the context page.
