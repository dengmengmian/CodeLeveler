# Beta readiness after Batch #2

**ENTER_MULTI_AGENT_PRODUCT_CLOSURE = NO. NEXT STEP = Beta Repair Gate (one, narrow).**

## Why not

Three OPEN_BETA_REQUIRED rows, all in scope of a single repair gate:

1. **R011-F1 — long-goal progress metric.** Count write operations to already-modified files as
   progress (or any equivalent that survives the refinement phase). One predicate, one test
   shaped like R011's measured turn profile (9 new / 0 new+2 writes / 0 new+3 writes).
2. **R011-F2 + R013-F1 + reopened N7 — reviewer reach and observability.** (a) Launch the
   required review on qualifying non-Verified terminals too (a failed wide diff needs eyes more,
   not less); (b) replace `unwrap_or(false)` with a persisted `ReviewAttemptFailed`-class event so
   an unlaunchable review is diagnosable; (c) find and fix why `run_review` dies before
   `SubAgentStarted` on the eval in-process path — the failure that today leaves zero trace.

Plus one instrument row (OPEN_EVIDENCE_NEEDED, may ride along or defer): R012-F1 durable
argument truncation.

## What must NOT be done

No second scheduler, no SubAgentEngine, no reviewer redesign, no round-ceiling changes to mask
R011-F1, no spawn-count KPIs. The mechanisms are built and smoke-proven; the gate is about reach,
metric honesty, and observability.

## After the gate

Re-run the two cheapest probes on the repaired binary (an R013-shaped eval task for reviewer
launch + an R011-shaped window profile), then proceed to Multi-Agent Product Closure with the
production evidence this batch could not produce.
