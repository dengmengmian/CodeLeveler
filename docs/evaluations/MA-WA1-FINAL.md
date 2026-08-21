# MA-WA1 — Final Report

**Status:** `MA-WA1 = OPEN_BETA_REQUIRED` · `MULTI_AGENT_PRODUCT = NOT_ACCEPTED`
**Scope of this document:** the closing record of the MA-WA1 investigation. It
changes no runtime behaviour and proposes none.

Raw evidence lives in the dogfood-control repository; every claim below names
the experiment directory that holds its data.

## 1. Executive summary

Multi-agent **execution** is finished and safe. Multi-agent **adoption** is not,
and after eight independent hypotheses the cause is not in CodeLeveler.

What works, validated and frozen:

| capability | evidence |
| --- | --- |
| `spawn_agent`, child lifecycle, `claim_write_scope`, ownership registry, settlement, durable provenance | `ownership-final`, `late-bound-ownership` |
| ownership safety | FS1–FS16 **PASS**, 13 safety counters **0**, across every scored run in the programme |
| engagement reliability | `never-engaged-exploration` — repaired and revalidated, 18/18 |
| measurement | corrected spawn extractor **VALIDATED**; Gate V2 **ACCEPTED** |

What does not: on qualified long tasks the model elects to delegate roughly
**10–30 %** of the time, and nothing CodeLeveler controls moves that number.

The single most informative result is the cross-model arm C. `deepseek-v4-pro`
against `deepseek-v4-flash`, same family, same binary, same task:

```
verifier   7/12  ->  10/12      (clearly better at the work)
rounds     98    ->  66         (clearly more efficient)
spawn      1/12  ->  1/12       (p = 1.00, no change at all)
```

Coding ability rises; the delegation decision does not respond. That is the
signature of a decision the model is not equipped to take from what it is
shown — not of a model that cannot do the work.

## 2. Experimental timeline

| # | round | design | n | outcome |
| --- | --- | --- | ---: | --- |
| 1 | Ownership / safety closure | production probes | 33 | FS1–FS16 PASS, 0 violations |
| 2 | Adoption causal diagnosis | A/B on two candidates | 33 | candidate regression **refuted**; variance identified |
| 3 | Metric audit + Gate V2 | recompute every recorded run | 129 | old extractor invalid; Gate V2 accepted |
| 4 | Engagement spiral repair | RED→GREEN + same-day paired control | 48 | spiral **CLOSED**, long-b 6/6 vs 1/4, p = 0.033 |
| 5 | Delegation timing (H-C) | one binary, one config line | 24 | flat, p = 1.00 |
| 6 | Adoption behaviour analysis | 20 features × 122 offers | 135 | no decision-point feature separates spawn from no-spawn |
| 7 | Baseline variance + difficulty ladder | 30 identical + 2/4/6/8 deliverables | 70 | ladder flat while verifier fell 100 %→30 % |
| 8 | Cross-model | flash / k3 / pro, one binary | 36 | no model effect; capability↑ with adoption flat |

Roughly **500 recorded runs** across the programme.

## 3. Hypothesis elimination

| # | hypothesis | test | result | key number |
| ---: | --- | --- | --- | --- |
| 1 | Runtime blocks or rejects delegation | in-vivo context + code-path audit | **Rejected** | `spawn_agent` never rejected; no reachable path difference before a child exists |
| 2 | Ownership / safety refuses children | FS matrix | **Rejected** | FS1–FS16 PASS, 0 violations, 0 ownership denials |
| 3 | The offer never reaches the model | offer coverage on engaged runs | **Rejected** | **54/54 = 100 %** |
| 4 | Offer timing (H-C) | RCT, offer held until first edit | **Rejected** | spawn p = 1.00; `offer − first_edit` p = 0.91 |
| 5 | Context size / salience | context at the decision point | **Rejected** | 31 105 vs 31 311 chars, p = 0.97 |
| 6 | Reoffer frequency | with vs without reoffer | **Rejected** | 21 % vs 16 %, p = 0.50 |
| 7 | Task magnitude / economics (H-1) | ladder 2/4/6/8 deliverables | **Rejected** | spawn 0.30/0.30/0.30/0.40, p = 1.00, while verifier fell 100 %→30 % (p = 0.0031) |
| 8 | Model choice / capability | three models, two families | **Rejected** | pro vs flash p = 1.00 with verifier 7/12→10/12 |
| — | *Never-engaged exploration spiral* | RED→GREEN + paired control | **Real defect, FIXED** | 14 % of qualified runs; long-b 6/6 vs 1/4, p = 0.033 |
| — | *The measurement itself* | 129-run recompute | **Was broken, FIXED** | old extractor returned 0 on all 37 delegating runs |

Two entries are not hypotheses about adoption but defects the investigation
found on the way. Both were real and both were closed. Neither changed adoption.

### H-1 was tested twice and failed twice

Its first formulation — *harness budget pressure* — was discarded **before
spending any runs**, on a finding that survives on its own: the parent agent is
never told its round budget. Verified across every `context_snapshot` of four
runs from three experiments, and confirmed in code (`drive.rs`: the turn ends at
`round >= round_ceiling` with no prior warning; the ceiling is a constant, not
settable from an eval case). A budget the model cannot see cannot drive its
decision, so varying `max_rounds` would have measured nothing.

Its second formulation — *perceived task magnitude* — was tested directly and is
flat.

## 4. Final conclusion

```
ADOPTION_ROOT_CAUSE        = MODEL_INTRINSIC_ADOPTION_LIMIT (leading)
DELEGATION_ADOPTION        = NOT_ACCEPTED
MA-WA1                     = OPEN_BETA_REQUIRED
MULTI_AGENT_PRODUCT        = NOT_ACCEPTED
```

The model completes qualified coding work well and does not reliably judge
*"this piece should go to another agent."* Every lever CodeLeveler owns has been
measured and none moves the decision.

### The variance that bounds every claim here

One unchanged model on one unchanged frozen task, measured four times:

| measurement | rate |
| --- | --- |
| historical, 10 binaries, weeks | 13/47 = 0.277 |
| micro-eval Phase 1, n=30 | 4/30 = 0.133 |
| difficulty ladder L3, n=10 | 3/10 = 0.300 |
| cross-model arm A, n=12 | 1/12 = 0.083 |

**0.08 → 0.30 on identical inputs.** This spread is wider than any
between-condition difference the programme ever measured, and it is why the
practical rule from Gate V2 matters: at these rates, twelve runs per arm cannot
resolve anything short of a tripling. Every gate before Gate V2 was under-powered
against noise nobody had measured.

### What is honestly not established

- A *moderate* model effect is not excluded. The cross-model design was powered
  for a large one only, and the Kimi arm stopped at n=7 when the account hit its
  billing-cycle quota.
- `MODEL_INTRINSIC_ADOPTION_LIMIT` is the leading hypothesis, not a located
  cause. It is what remains after eight eliminations, which is weaker evidence
  than a positive demonstration.

## 5. Beta implication

**Execution is Beta-ready; adoption is not, and the two should be separated.**

Ownership safety, claim/settlement, durable provenance and engagement
reliability are validated and can ship. What cannot ship as advertised is a
*dependable* multi-agent capability, because dependability was defined as
"delegation happens on the majority of qualified tasks" and the product achieves
10–30 %.

That leaves a decision, and it is a product decision rather than an experiment:

**Option A — re-derive the gate.** If delegation stays model-elected, MA-WA1's
`p_min = 0.50` is a threshold no model-elected system in this evidence base can
meet. The honest move is to re-derive the threshold from what such a system
achieves, and describe multi-agent as opportunistic rather than dependable.

**Option B — change the decision surface.** Replace an availability notice the
model must act on unaided with a concrete proposal built from work items the
runtime already tracks. This is a real product change with its own RED tests,
KEEP controls and safety re-run. Design: `docs/design/DELEGATION_ADVISOR_DESIGN.md`.

Both are post-Beta. Neither is a reason to hold ownership safety or engagement
reliability, which are done.

### Standing constraint

Nothing in this programme justifies raising the spawn rate by construction.
Forced delegation, prompt steering toward spawning, and `ToolChoice::Required`
were excluded throughout and remain excluded. The goal was never more agents; it
is agents used at the right time — and on that, `OVER_DELEGATION` also has no
support: KEEP controls stayed at 0/6, and the smallest ladder rung (two
deliverables, completed by every run) still delegated 3/10, the same rate seen
everywhere.

## Evidence index

| experiment | directory in `codeleveler-dogfood-control` |
| --- | --- |
| metric audit + Gate V2 | `delegation-metric-audit/`, `delegation-gate-v2/` |
| offer mechanism | `delegation-opportunity/` |
| engagement spiral | `never-engaged-exploration/` |
| timing RCT | `delegation-timing-experiment/` |
| behaviour analysis | `adoption-behavior-analysis/` |
| variance + ladder | `adoption-micro-eval/` |
| cross-model | `cross-model-delegation/` |

Frozen extractor: `delegation-metric-audit/scripts/spawn_metric.py`
(`--selftest` runs EX1–EX7). Every adoption number in this document comes from
it.
