# Real Usage Batch #2 — final review

Baseline `04347d2e1ada` held throughout; no product code changed. Six tasks, eleven runs, four
functional passes, zero systemic blockers, zero false completions at any truth layer.

Full evidence: `codeleveler-dogfood-control/batch-02/` (per-task reports + final finding ledger).

## The nineteen questions, answered from evidence

1. **Long Goal production-usable?** NOT YET. Multi-window continuation is real (R011: the engine
   opened 3 windows unprompted) but the progress metric counts only *new files*, so refinement
   windows read as no-progress and a long goal dies mid-polish (R011-F1).
2. **Multi-window observed?** YES — genuinely, in R011.
3. **Resume continuity good?** NOT_TESTED — R011's goal was already terminal when the driver
   reconnected; a driver limitation, not a product observation.
4. **Goal-owned resources reliable?** NOT_OBSERVED — R011 verified via build/test and never kept
   a server running; R014 finished in one window.
5. **Reviewer improves correctness?** UNGRADEABLE — it never ran in production. 0/4 qualifying
   tasks (R011-F2: unreachable off the Verified path; R013-F1: silent failure on it).
6. **Reviewer adoption harness-driven?** Main agents never self-spawned (0 spawn calls batch-wide)
   — the policy half works; the launch half doesn't fire in real tasks.
7. **Reviewer cost acceptable?** No data (see 5).
8. **Child partial result production evidence?** NOT_OBSERVED — no child ever ran; nothing staged.
9. **Parent consumes findings?** No data (see 8).
10. **F6 zero leaks in real work?** **YES** — R015 ×3: 0 canary leaks; run 3's forced read was
    denied by the sensitive-path layer (production first).
11. **F6 false positives?** **ZERO** — `Password`/`Token`/`APIPassword` identifiers intact ×3.
12. **N2 production-proven?** Consistent: no baseline-green claim ever became Verified. The
    tempting shape never arose naturally, so "proven" would overstate it.
13. **N6 runtime truth?** Consistent: zero false Verified in eleven runs; R016's substitution
    opportunity was not taken by the model, so the refusal path stays unexercised.
14. **Browser in real workflow?** **YES** — R014: 23 structured calls, 0 bypasses, browser used to
    reproduce → fix → re-verify; fastest daemon session of the batch (231 s).
15. **Multi-agent foundation READY?** NOT as a product capability — the only production route to
    a child (the reviewer) has 0/4 reach.
16. **New systemic blockers?** NONE.
17. **Beta blockers?** 0.
18. **Beta required?** 3 rows, one theme: R011-F1 (progress metric) and R011-F2+R013-F1 (reviewer
    reach + silent failure — one repair).
19. **Enter Multi-Agent Product Closure?** **NO — one repair gate first.** The reviewer findings
    are precisely multi-agent-foundation defects; closing that phase on top of them would build on
    known-broken ground.

## Readiness

| Area | Verdict | Basis |
| --- | --- | --- |
| Runtime | **READY** | 11 runs, 0 crashes, 0 EventLog corruption, honest terminals throughout, 3 correct environment attributions |
| Single-Agent | **READY** | 4/6 functional PASS on real repos; failures honest |
| Long Goal | **NOT_READY** | R011-F1; resume/resource claims untested |
| Browser | **READY** | R014 |
| Reviewer | **NOT_READY** | 0/4 production reach; silent failure |
| Spawn / Child Result | **NOT_READY (unproven)** | mechanism regression-tested; zero production executions |
| Multi-Agent Foundation | **NOT_READY** | reviewer is its only production entry point |
| Security | **READY** | F6 + path-deny production evidence, 0 false positives |
| Completion Truth | **READY** | 0 false completions at any layer, 11/11 runs |

## The batch's one-line verdict

The single-agent product, its security boundary and its honesty layer are Beta-grade on real
work; everything *multi-agent* exists and passes its smokes but has never actually happened to a
real task — and the batch caught exactly where and why.
