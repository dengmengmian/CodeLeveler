# Context-cost measurement plan (items 6, 7, 9)

Date: 2026-09-02. Baseline: the context-cost batch on top of `878c60e`.
**Design only — no run has been made against this plan yet.**

Three open questions came out of the context-cost audit. They are grouped
here because they share instrumentation and a run budget, **not** because they
are one experiment: two are model-behaviour questions that need a real
provider, and one is a latency question that needs no model at all. Mixing
those would bury a millisecond-scale effect under model variance.

| Item | Question | Kind | Cost |
| --- | --- | --- | --- |
| 6 | Does deterministic tool-result trimming pay for the information it drops? | Model behaviour, A/B | Paid |
| 7 | Is re-sending the reasoning chain worth its own re-billing? | Model behaviour, A/B | Paid + one implementation |
| 9 | How much turn-start latency does the full-transcript load cost? | Latency, benchmark | Free |

## 0. Shared instrumentation (prerequisite, unpaid)

`model_requests` records one row per completed request with a summary
`retry_count`, so a logical round and the HTTP traffic behind it cannot be
told apart. Neither A/B below is readable without that split:

| Counter | Counts |
| --- | --- |
| `logical_rounds` | Model turns the drive loop took |
| `physical_attempts` | Requests actually sent, retries included |
| `advisory_calls` | Contract derivation, completion judge, closeout nudge |
| `compaction_calls` | Summarization for a fold |

Item 6's whole claim is "fewer `compaction_calls` for the same result". Today
that is only observable by counting mock calls in a test.

## 1. Item 6 — deterministic trimming

Mechanism shipped, default off, behind the `prune_tool_results` eval knob.

| Arm | `prune_tool_results` |
| --- | --- |
| control | false (production default) |
| ablated | true |

Single variable, through `leveler eval ablate`. Cases are chosen for the shape
the mechanism targets — old tool results that have left the working set:

| Case | Why | Reads as share of final request (C2.1) |
| --- | --- | --- |
| `evals/realrepo/yq-doc-count` | Breadth: 24 paths read once each, accumulating | 57.4 % |
| `evals/scale/scale-s800` | Long task on a synthetic 800-file tree | — |
| `evals/realrepo` + ripgrep | Only if the first two pass | 72.2 % |

Ripgrep is deliberately held back: its trajectory is broad-then-narrow, and
C2.2 established that its old reads get referred back to. It is the least
favourable case, and paying for it first buys the least information.

**Promotion rule.** Ablated is promoted to the production default only if,
against control: task success does not fall, provider `input_tokens` falls
materially, and `compaction_calls` falls. A tie on tokens is a failure — the
mechanism costs information and must earn it back.

## 2. Item 7 — reasoning pass-back

The ablated arm does not exist yet. `stream_round` discards reasoning
deltas after showing them, so the pass-back contract that
`deepseek-v4-pro` / `deepseek-v4-flash` declare always carries `""`. See
`docs/REASONING_CAPABILITY_AUDIT.md` §9 for the mechanism and the gap.

| Arm | Assistant message keeps reasoning | Wire |
| --- | --- | --- |
| control | no (production default) | `reasoning_content: ""` |
| ablated | yes, within the current tool loop | the captured chain |

Implementation first (capture in `stream_round`, bounded to the live tool
loop), then the same single-variable ablation. Thinking model only.

**Promotion rule.** Completion rate or rounds-to-first-edit must improve
enough to pay for the extra input tokens the chain adds to every later
request in the turn. If the arms tie, keep today's behaviour and delete
`passback_reasoning_content` from both profiles rather than leave a contract
carrying nothing.

## 3. Item 9 — transcript load (unpaid benchmark)

Every chat, resume and goal-continuation turn loads and deserializes the whole
`session_messages` table, even when a watermarked `ContextSnapshot` means only
the tail is needed. This is pure latency: no tokens, no behaviour change.

Measured locally on a synthetic session at 100 / 1 000 / 5 000 messages,
before and after the change:

| Metric | Source |
| --- | --- |
| Wall clock, turn start → first model request | Harness timer |
| Messages loaded | Counting `MessageStore` |
| Bytes deserialized | Same |

No provider is involved, so this runs in minutes and costs nothing. It is
listed with 6 and 7 because the same runs will show it as wall-clock once
implemented, not because it needs the paid lane.

## 4. Order

1. Instrumentation (§0) — unpaid, unblocks reading both A/Bs.
2. Item 6 A/B — the mechanism already exists; the first money is spent on
   something finished.
3. Item 9 implementation + benchmark — unpaid.
4. Item 7 implementation + A/B — paid, last, because its ablated arm has to be
   built first.

## 5. Where runs happen

The Dogfood Lab (`DOGFOOD_ROOT`), under its own rules: a frozen
`CODELEVELER_EVAL_BASELINE` SHA, a clean build at that SHA under
`bin/codeleveler/<sha>/`, isolated per-run `HOME`/XDG, and the key from
`secrets/`. A dirty build is not a valid baseline. This is an internal
single-variable ablation, so it uses `leveler eval ablate` directly rather
than the three-harness comparative runner.
