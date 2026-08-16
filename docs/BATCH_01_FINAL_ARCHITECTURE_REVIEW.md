# Real Usage Batch #1 · Final Architecture Review

Product baseline `c3bf11ba01c3d5e5ef66244dcc3a6ae787036268`. Evidence: `dengmengmian/codeleveler-dogfood-control`, branch `prep/batch-01-r005-r010`. Companion documents: `docs/BETA_CLOSURE_PROGRAM.md`, `batch-01/BATCH_01_FINAL_FINDING_LEDGER.md`, `batch-01/BATCH_01_FINAL_REVIEW.md`.

**No CodeLeveler product code was modified during this review.**

## Executive Summary

Batch #1 ran ten real tasks against unmodified upstream repositories with hidden acceptance criteria and no hints. Final outcome: **5 PASS / 2 INCOMPLETE**, where both INCOMPLETEs are the same Hoppscotch task run twice.

Its value is not the pass rate. It is that three consecutive repair gates plus a controlled rerun **separated harness failure from model failure with evidence**, and in doing so overturned several of the assumptions the roadmap had been built on:

- **Harness reliability is no longer the primary failure driver.** It was in R004, R006 and R007; after those gates, R007b/R008/R009/R010 recorded zero harness-induced interruptions across four tasks.
- **Search and discovery were never the bottleneck.** First relevant hit landed at step 2–5 in every Large task, and the fastest success (R010) read the *least*.
- **Delegation is nearly nonexistent, and its one appearance made things worse.** Six of seven Large/Long tasks never spawned; the seventh spawned twice and lost the decisive knowledge when a child died on its budget.
- **Browser V1 is mature.** ~160 structured calls across the batch with zero shell/Playwright workarounds.
- **The remaining Beta risk is concentrated in truth-telling and long-goal lifecycle**, not in capability.

Exactly one Beta blocker remains, and it is security: **F6**.

## Batch Scope and Outcomes

| Task | Target | Functional | Harness | Notes |
| --- | --- | --- | --- | --- |
| R001 / R001b | sharkdp/fd | PASS / PASS | — | R001b tested recovery from behavioural review feedback: the agent located its own wrong abstraction and generalised the fix without being told the code-level cause. |
| R002 | johnkerl/miller | PASS | — | |
| R003 | gohugoio/hugo | CONDITIONAL_PASS | ISSUE_FOUND | First appearance of both "environment failure recorded as code failure" and "prose contradicts recorded outcome". |
| R004 | plait-board/drawnix | PASS | ISSUE_FOUND → Repair Gate CLOSED | 7 product findings including goal-text corruption and false completion display. |
| R005 | rust-lang/cargo | PASS | ISSUE_FOUND | Verification-toolchain provenance. |
| R006 | casdoor/casdoor | PASS | ISSUE_FOUND → Repair Gate CLOSED | Four reliability defects; ~50 min of a 72-min run lost to harness interruption. |
| R007 | hoppscotch | **INCOMPLETE** | ISSUE_FOUND · **Systemic Blocker** → Repair Gate CLOSED | Durable EventLog corrupted by secret redaction; session unrecoverable. |
| R007b | hoppscotch (clean rerun) | **INCOMPLETE** | **PASS** | Zero harness interruption; still no product fix. The controlled experiment. |
| R008 | BurntSushi/ripgrep | PASS | PASS | Verifier 10/0. |
| R009 | go-task/task | PASS | PASS | Verifier 3/0 in one 54-min window; `-race` green; found a latent second defect. |
| R010 | TailAdmin | PASS | PASS | One 28-min window; 47 structured browser calls; runtime terminal `verified`. |

## Historical Audit Chain (preserved, never rewritten)

```
R004  Functional PASS → Harness ISSUE_FOUND → R004 Repair Gate PASS/CLOSED
R006  Functional PASS → Harness ISSUE_FOUND → R006 Repair Gate PASS/CLOSED
R007  Functional INCOMPLETE → Harness ISSUE_FOUND → Systemic Blocker YES
      → R007 Repair Gate PASS/CLOSED → merged as c3bf11b
R007b Functional INCOMPLETE · Harness PASS   (independent result, not a retry of R007's verdict)
R008/R009/R010  Functional PASS · Harness PASS
```

A repair gate closing does **not** convert the original task's verdict to PASS. R007 remains INCOMPLETE with a systemic blocker on its record.

## Harness Reliability — Final Assessment

**Verdict: ready for Beta closure work; no correctness, recovery, side-effect, ownership or completion blocker remains at `c3bf11b`.**

| Question | Answer | Proof |
| --- | --- | --- |
| Correctness blockers? | None | R6-P1/F1 guard composition fixed and re-proved across 4 tasks |
| Recovery blockers? | None | F2 fixed; durable logs parseable and gapless across 5 resumes in 4 tasks |
| Side-effect truth blockers? | None | R004-F3 workspace boundary re-proved live in R007b |
| Process ownership blockers? | None | R6-P4/R004-F7: task-owned leftovers 0 and unknown 0 at every terminal of R005–R010 |
| Completion truth blockers? | None at runtime level | Honest `completed_unverified` in R007b/R008/R009; supervisor-confirmed `verified` in R010 |
| Cross-task production proof? | Yes | R004-F1/F4/F7, R6-P1..P4, R007-F1/F2 all at proof level D or E |

The residual truth risk is **not** in the runtime — it is in the model's closing prose (N6 + R003-F2) and in verification inputs (R003-F1, N2). That is Gate 2, not a runtime repair.

## Single-Agent Quality

**Verdict: Beta-ready on evidence.** Five clean passes on unmodified upstream repositories across Rust, Go, TypeScript CLI, concurrency and frontend work, with hidden acceptance and no hints. The engineering quality is not merely "tests pass":

- **R009** found the actual race (a shared AST matrix mutated concurrently), fixed it by deep-copying before resolution, **and** discovered that `DeepCopy()` itself was shallow — without which the first fix would have been inert. It also refused the shortcut the task forbade: deps still run concurrently (0.674 s vs ~1.2 s serial).
- **R008** fixed the real defect site (`matches_possible` returning `self.invert_match` for zero patterns) and added four tests in the repository's own `rgtest!` idiom.
- **R010** extracted filter/sort logic into a pure module and unit-tested it — the structural choice that made independent verification possible — with lint delta 0.

## Long-running Goal

**Verdict: NOT closed. The largest remaining Beta gap after security.**

Unresolved: goal-owned resource lifetime (R007-F3 — a dev server dies at a window boundary and the next window rebuilds it), resume affordance (R6-P5 — every resume in this batch used a supervisor-supplied UUID), and work-window economics.

On the ceiling, the evidence is specific: **(C) amplified by low throughput + (D) missing work-window abstraction**, not (A) too small. R009 finished a Long task in 63 rounds; R010 finished in one short window. R007's ceiling hits were polluted by guard kills and provider outages that no longer exist. **Raising 100 → 200 would be a fix aimed at the wrong cause.**

## Browser

**Verdict: CLOSURE_ONLY — do not rewrite.**

| Signal | Evidence |
| --- | --- |
| Capability maturity | Mature: ~160 structured calls, **0** workarounds |
| Workaround rate | 0% (R004-era bypass pattern eliminated) |
| Structured call reliability | High — R010's 47 calls all functioned; failures elsewhere were app-boot failures |
| Task utility | Bimodal: POSITIVE where the app booted (R006 22, R010 47), NEGATIVE where it did not (R007 72, R007b 19) |
| Real UI verification | Achieved in R010 (`browser_console` used to assert no page exceptions) |

Both negative cases share one cause — an unbootable dev server in a large monorepo — which belongs to Gate 3. **Separating capability from utility is what prevents a wasted rewrite here.**

## Spawn / Multi-Agent

**Verdict: the least evidenced area in the product, and the batch explains why.**

| Task | Difficulty | REQUIRED_REVIEWER | Organic spawn |
| --- | --- | --- | --- |
| R005 / R006 / R007 | Large | no | 0 / 0 / 0 |
| R007b | Large | no | **2** |
| R008 | Medium | **yes** | **0** |
| R009 | Long | **yes** | **0** |
| R010 | Long | no | 0 |

- **Adoption:** ~1 in 7 Large/Long tasks. Difficulty does not predict it — R009 is *Long* and delegated nothing.
- **Utility:** of two children ever created, one returned a precise map; the other (Newton) burned 68 rounds and 1200 s, **had the defect site open**, and returned only "budget exhausted". Delegation, on its single trial, **caused** the batch's only functional failure.
- **Consumption:** the parent then re-read 14–16 files the children had already covered.
- **Mechanism:** `REQUIRED_REVIEWER` never existed in the product (N7). The two tasks carrying it ignored it and passed.

**This is the finding that reorders the roadmap.** The planned next step was Structured Child Result. But N1 rests on one child in one task, and three subsequent tasks demonstrated the model does not delegate at all. Building the child-result contract now would harden a path nothing takes. **Reviewer mechanism first (Gate 4); child-result contract after it produces real traffic (Gate 5).**

## Verification / Completion Truth

**Verdict: Beta-required work remains, and it is now cross-task confirmed.**

- **Prose vs record** (R003-F2 + N6): in both, the runtime label was honest and the model's narrative was not — R003 claimed "tests pass" against `passed=false`; R007b claimed "全部通过了" for an incidental fix while the original requirement was untouched.
- **Environment vs code** (R003-F1 + R005-F-P1 + R009's toolchain wall): partially fixed. MSRV refusal is classified; **disk exhaustion and missing global tooling are not**, and R003's actual trigger would still be recorded as a code failure today.
- **Reproduction validity** (N2): a repro that passes on unmodified code proves nothing, yet was accepted as proof of absence.

## Efficiency

**Throughput diagnosis: 1 tool per assistant step, 7 tasks running, mean 1.10–1.41, median 1 in every task.** This is the most stable measurement in the batch. It is not a harness scheduling limit — parallel batches exist and were used (R007b ran 12-call batches) — so it reads as predominantly **model behaviour**, with the tool-call protocol permitting more than the model emits. **Not a Beta blocker:** R009 and R010 both finished well inside budget at this rate. It is a cost multiplier that makes the round ceiling *look* like the problem.

**Action transition** is the metric that actually separates success from failure:

| Task | First effective product edit | Outcome |
| --- | --- | --- |
| R007b | never | INCOMPLETE |
| R008 | 48% of run | PASS |
| R009 | 28% | PASS |
| R010 | **14%** | PASS |

Recommend adopting `FIRST_EFFECTIVE_PRODUCT_EDIT_FRACTION` as a formal Efficiency Telemetry V1 extension (`NEVER` when no product edit occurs). Repro assets and helper scripts must not count as product edits — that distinction is exactly what exposed R007b.

**Reading volume inversely correlates with success.** R010 passed after 27 read+search calls; R007b failed after ~360 tool calls across parent and children. Repeated reads track outcome quality directly: R007 23 EXACT, R007b 19, R008 6, R009 **0**, R010 **0**.

## Security

**Verdict: one Beta blocker — F6.**

A credential the agent reads is redacted on the way to storage but reaches the model **and the provider** in plaintext; once the model paraphrases it into prose, no key/value shape remains for storage-side redaction to catch, and the plaintext persists (4 durable rows observed). Pre-existing, systemic, not a regression. Contained during the batch only by forbidding real credentials.

Phase 0 is complete and produced a decisive constraint: **the existing redactor cannot simply be moved earlier.** It destroys ordinary code (`let password = config.password;`) while missing the commonest real shapes (`API_PASSWORD=…`, `export TOKEN=…`, URL-embedded passwords). A value-position-aware detector is a precondition.

## Finding Closure Summary

| Status | Count |
| --- | --- |
| VERIFIED_FIXED | 12 |
| FIXED_NEEDS_PRODUCTION_PROOF | 0 |
| **OPEN_BETA_BLOCKER** | **1** (F6) |
| OPEN_BETA_REQUIRED | 9 |
| OPEN_EVIDENCE_NEEDED | 4 |
| DEFER_POST_BETA | 2 |
| OBSOLETE | 6 |
| INVALIDATED_BY_EVIDENCE | 1 (N3 as a general defect) |

**P0: EMPTY.**

## Architecture Decisions

| # | Question | Decision |
| --- | --- | --- |
| A | Rewrite Browser? | **No — closure only.** Zero workarounds in ~160 structured calls; both negative cases were unbootable apps. |
| B | Prioritise Explorer/search? | **No — deprioritise.** Discovery hit at step 2–5 in every Large task; the fastest success read the least. |
| C | Reviewer as an explicit product stage? | **Yes (Gate 4).** Beta requires reviewer semantics, and the current label is inert. |
| D | Is organic spawn enough to support multi-agent? | **No.** ~1 in 7, and its only appearance was net-negative. Multi-agent needs a policy-driven mechanism, not more hope. |
| E | Build Structured Child Result now? | **No — after Gate 4.** One child, one task; three later tasks never delegated. |
| F | Biggest Long-Goal gap? | **Goal-owned resource lifetime**, then resume affordance. Not the round ceiling. |
| G | Requirement ledger for goal closure? | **Yes, minimal** — reuse Engine/EventLog/turn lifecycle; no second state machine. |
| H | Is N2 a Beta item? | **Yes, Beta-required**, folded into Gate 2 as verification-evidence semantics. |
| I | Is F6 a Beta blocker? | **Yes — the only one.** |
| J | Is 1-tool-per-step a Beta blocker? | **No.** Cost multiplier, not a correctness or capability limit. |

## Release Risks

| Risk | User impact | Frequency | Severity | Evidence confidence | Beta impact | Architecture cost | Regression risk |
| --- | --- | --- | --- | --- | --- | --- | --- |
| F6 secret propagation | HIGH | MEDIUM | HIGH | HIGH | **BLOCKER** | MEDIUM | MEDIUM (detector must not mangle code) |
| Closeout prose vs reality (N6/R003-F2) | HIGH (trust) | MEDIUM | MEDIUM | **HIGH** (cross-task) | REQUIRED | MEDIUM | LOW |
| Env-vs-code misclassification (R003-F1/R005-F-P2) | MEDIUM | MEDIUM | MEDIUM | HIGH | REQUIRED | LOW | LOW |
| Green-on-baseline repro (N2) | HIGH when it fires | LOW | HIGH | MEDIUM (1 task) | REQUIRED | LOW | LOW |
| Long-goal resource lifetime (F3) | MEDIUM | MEDIUM | MEDIUM | MEDIUM | REQUIRED | MEDIUM | MEDIUM (must not weaken R6-P4) |
| Reviewer mechanism absent (N7) | MEDIUM | — | MEDIUM | HIGH | REQUIRED | HIGH | LOW |
| Child result loss (N1) | HIGH when it fires | LOW | HIGH | **LOW (n=1)** | EVIDENCE | MEDIUM | LOW |
| Resume discoverability (R6-P5) | MEDIUM | HIGH | LOW | HIGH | REQUIRED | LOW | LOW |
| TUI contrast (F4) | LOW | HIGH | LOW | HIGH | REQUIRED | LOW | LOW |

## Batch Lessons

1. **Real usage exposed defects no test suite would have.** Goal-text corruption, a guard composition that killed a healthy agent, and a redactor that corrupted its own durable log all appeared only under real tasks — the last one had shipped through multiple green test runs.
2. **Assumptions overturned:** search was not the bottleneck; the round ceiling was not the constraint; delegation was not the missing ingredient (its one use hurt); Browser did not need a rewrite.
3. **Assumptions confirmed:** honest terminals matter and held; ownership-aware process truth held at every terminal; bounded provider retry proved itself against a real outage.
4. **The controlled rerun was the highest-value instrument.** R007b — same goal bytes, same model, same repo, only the baseline changed — is what made "harness vs model" a measurement rather than an argument. Keep that pattern.
5. **Fixing mid-batch would have destroyed the experiment.** Holding N1–N7 open through the overnight run is why we now know N3 is task-shaped and N1 is a single-task datapoint.
6. **Real Usage Batch is worth keeping as a pre-Beta mechanism** — but Batch #2 must target the mechanisms Gates 1–5 create, not repeat this one.

## Next

See `docs/BETA_CLOSURE_PROGRAM.md`. **Next gate: Gate 1 · Secret Propagation Safety (F6).**
