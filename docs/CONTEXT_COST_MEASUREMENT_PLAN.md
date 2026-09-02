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

## 0. Shared instrumentation (prerequisite) — done

The gap was worse than "the lanes are unlabelled". `model_requests` only ever
held main-loop rounds: the drive loop recorded one row per streamed round,
while the summarization behind a compaction fold went straight to the runtime
and was **not recorded at all**. A session that folded reported fewer tokens
than it spent, and the missing tokens were exactly the ones a fold costs —
the number any "cheaper than folding" claim has to be measured against. An
A/B read off that table would have understated the control arm precisely
where the ablated arm claims to save.

Shipped: a `kind` column (`round` | `compaction` | `advisory`), the fold's
summary call recorded under `compaction`, and a stated meaning for the row —
one row is one LOGICAL call, so physical traffic is `SUM(1 + retry_count)`
and logical calls are `COUNT(*)`, per lane. Pre-migration rows read as
`round`, which is a fact about what the old writer could produce, not a guess.

| Counter | Read as |
| --- | --- |
| Logical rounds | `COUNT(*) WHERE kind='round'` |
| Physical attempts | `SUM(1 + retry_count)` over the lane |
| Compaction calls | `COUNT(*) WHERE kind='compaction'` |

Contract derivation and the completion reconciliation judge now write the
`advisory` lane too, including on the paths that spend tokens and then return
an error: a reply the judge could not parse is still a reply the provider
billed. A session's recorded cost is the cost.

## 0b. What the first run found: the fold does not fire (2026-09-02)

The item 6 ablation ran and answered a different question than it asked.

Four runs (two cases x two arms, frozen `670329b08742`, v4-flash) folded
**zero** times. Single-round context peaked at 84,560 tokens against a
`reliable_context` fold threshold of 786,432 — 10.8 %. Trimming is wired
*inside* the fold decision, so it never executed, and the arms differ only by
run-to-run variance. Verdict: INCONCLUSIVE, default stays off. Evidence and
per-run numbers: `DOGFOOD_ROOT/eval/state/prune-ab-670329b08742/REPORT.md`.

**This supersedes the assumption under §1 and §2.** C2.1 §8 had already
recorded it (gate at 524k, runs reaching 19–24 %, zero `Compacted` events) and
C5's closeout recorded it from the other side: agents suppress their own
context demand with grep and range reads rather than accumulating toward the
window. This is the third observation, and the first with the fold lane
instrumented well enough to prove the absence rather than infer it.

What follows from it:

- **A fold-triggered mechanism has no surface on the task path.** Trimming, a
  cheaper summarizer, a smarter retention rule — none of them can help or be
  measured where the fold never runs.
- **The compaction machinery is unexercised in production task runs.** Not
  dead code (chat's 24k threshold reaches it), but its task-path behaviour is
  not exercised by real usage, which is worth knowing before relying on it.
- **Measuring one requires changing the trigger.** Either lower the threshold —
  a second variable, so a three-arm design that separates "the mechanism" from
  "folding earlier" — or find a workload that genuinely reaches ~786k, which
  C5's E2 found agents actively avoid producing.

The process lesson is cheaper than the run: verify that a mechanism's trigger
fires on the chosen cases before paying for arms. The information needed was
already in C2.1 §8.

## 0c. The trimming trigger was moved off the fold (2026-09-02)

§0b's finding made the mechanism unreachable, so the mechanism moved rather
than the finding being filed away.

Trimming was wired *inside* the fold decision: fold comes due, try trimming
first, skip the fold if trimming is enough. The fold never comes due, so it
never ran. But what it reclaims — stale tool results that have left the
working set — is worth reclaiming whether or not the window is ever
threatened. The fold is an overflow guard; recycling is a different job and
needs its own trigger.

It now fires on a **batch of reclaimable bytes** (`PRUNE_BATCH_BYTES`,
64 KiB), independent of the fold. Batching is the point: a trim rewrites bytes
the provider has already cached, so it costs one prefix-cache break each time
it runs. Every round would pay that repeatedly as the working set slides and
results go stale one at a time. One break for a large reclaim pays for itself
in a few rounds; one break per round does not.

Still default off, still behind `prune_tool_results`. What changed is that the
knob now has a surface, so the A/B in §1 can actually measure something.

## 0d. Both A/Bs ran after F7 (2026-09-03)

Frozen `dcb51f4d7ee5`, v4-flash, `scale-s800`, one repetition per arm.
Evidence: `DOGFOOD_ROOT/eval/state/f7-followup-dcb51f4d7ee5/REPORT.md`.

**The instrumentation earned its keep.** This is the first cohort where the
`advisory` lane is populated: 2 to 3 calls per run, 2,500 to 5,000 tokens that
`model_requests` previously did not record at all.

**`keep_reasoning`: FAIL.** Every metric the promotion rule named moved the
wrong way.

| Metric | control | ablated | Δ |
| --- | ---: | ---: | ---: |
| Input tokens | 573,329 | 765,726 | **+34 %** |
| Output tokens | 19,325 | 31,128 | +61 % |
| Rounds | 19 | 22 | +16 % |
| First edit | round 6 | round 11 | 5 later |
| Cost | $0.085 | $0.115 | +35 % |
| Acceptance | not passed | not passed | tie |

The chain accumulates like a tool result, exactly as predicted, and buys
nothing back. One repetition is not a rate, but the rule was fixed in advance
and no metric it names is ambiguous. The knob stays off; whether
`passback_reasoning_content` should be removed from both profiles rather than
declare a contract carrying `""` is a product decision this run does not
settle.

**`prune_tool_results`: INCONCLUSIVE, and now quantified.** Reclaimable bytes
measured from each run's final context: 25,542 and 28,613, against a
`PRUNE_BATCH_BYTES` threshold of 64 KiB. The trim did not run. The gap is a
factor of two — against the fold it was a factor of ten — so the retrigger in
§0c moved the mechanism much closer to a surface without reaching one.

The threshold was chosen by arithmetic, not measurement, and this says the
arithmetic was too conservative for this workload class. **It is not being
retuned to fit this run.** Picking 24 KiB because 25.5 KiB was observed is
fitting a constant to a single sample of a single case. What would justify a
change: a reclaimable-bytes distribution across several cases and run lengths,
plus a cache-hit measurement that prices the prefix break the trim costs.

## 1. Item 6 — deterministic trimming

Mechanism shipped, default off, behind the `prune_tool_results` eval knob.
Since §0c it triggers on reclaimable bytes rather than on a fold, so the arms
now differ in behaviour on any run that accumulates stale results.

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

Both arms now exist. `stream_round` keeps the reasoning on the assistant
message that produced it when `keep_reasoning` is set, so a pass-back provider
receives the chain instead of the empty string it gets today. Default off; eval
knob `keep_reasoning`. See `docs/REASONING_CAPABILITY_AUDIT.md` §9 for the
mechanism and why neither direction is obviously right.

| Arm | Assistant message keeps reasoning | Wire |
| --- | --- | --- |
| control | no (production default) | `reasoning_content: ""` |
| ablated | yes, within the current tool loop | the captured chain |

Thinking model only. The arms differ in one resolver input, as every
ablation here does.

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

## 3b. Both A/Bs wait for F7

Neither run may be taken before F7 lands.

The completion gate decides task success, and F7 changes what it accepts: a
behavioural obligation can no longer be discharged by citing the edit that was
made (`docs/F7_BEHAVIORAL_EVIDENCE_ARCHITECTURE.md`). Task success is the
primary metric of §1 and a promotion condition of §2, so a number taken before
F7 is a number from a gate that is known to accept false completions and is
about to stop.

This is a sequencing dependency, not a scope change. The designs, the promotion
rules and the case selection above stand as written.

## 4. Order

0. **F7 lands** (§3b) — otherwise both A/Bs measure success against a gate
   that is about to change.
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
