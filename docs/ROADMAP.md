# CodeLeveler Roadmap

Two horizons: what Beta needs, and what follows it. Everything here is derived
from recorded evidence; the "why" column names the finding that demands the
work. Nothing speculative.

## Where the project is

```
Beta Candidate            v0.2.0-beta.1 · on main · pre-release channel ready
        ↓
Real Usage Batch #1       six tasks against the PUBLISHED binary
        ↓                 docs/evaluations/REAL_USAGE_BETA_001.md
Capability Closure        driven by what that round finds
```

The current step is **Real Usage**, not feature work. The Post-Beta phases below
(Delegation Advisor, SubAgentProvider, Durable Child Session, Background-first
UX, Capability Negotiation) stay post-Beta and do not start early — including
Remote Worker, which is not on this roadmap at all yet because nothing has asked
for it.

## Now — Beta

Gate-by-gate execution lives in
[`BETA_CLOSURE_PROGRAM.md`](BETA_CLOSURE_PROGRAM.md). The multi-agent portion of
it is settled and splits in two:

| area | state | why |
| --- | --- | --- |
| Ownership safety, claim/settlement, durable provenance | **Beta-ready** | FS1–FS16 PASS, 13 safety counters 0, across every scored run |
| Engagement reliability | **Beta-ready** | never-engaged spiral closed and revalidated ([evidence](evaluations/MA-WA1-FINAL.md)) |
| Delegation adoption | **NOT_GUARANTEED — accepted** | 10–30 % on qualified tasks; eight hypotheses eliminated, cause is not in CodeLeveler |

**Decision taken (2026-08-22): Opportunity-based Delegation.** MA-WA1 closes with
`BETA_DECISION = ACCEPT`. `p_min = 0.50` is withdrawn as a threshold no
model-elected system in this evidence base can meet, and the Beta claim moves
from behaviour to runtime:

> Beta ships a **secure Multi-Agent Runtime** — reliable execution whenever the
> model elects to collaborate. It does **not** promise that the agent splits
> tasks on its own.

Adoption is therefore not a Beta blocker and not a release gate. Changing the
decision surface — the one lever never tested — is post-Beta Phase 1 below.

## Post Beta Multi-Agent Product Closure

Ordered by dependency, not by appeal. Each phase states the evidence that
justifies it and what would prove it wrong.

### Phase 1 · Delegation Advisor

Move from announcing that delegation exists to proposing a specific,
declinable delegation built from work items the runtime already tracks.

Design: [`design/DELEGATION_ADVISOR_DESIGN.md`](design/DELEGATION_ADVISOR_DESIGN.md).

- **Why now:** it is the only untested lever. Offer coverage, timing, context
  size, reoffer frequency, task magnitude and model choice were each measured
  and none moved adoption.
- **Success:** `appropriate_delegation_rate` rises with `unnecessary = 0` and no
  verifier regression. **Not** spawn rate.
- **Falsified by:** correct path-level proposals declined at the same rate. That
  outcome makes Opportunity-based Delegation permanent rather than provisional,
  and is worth knowing.
- **Blocked on:** nothing technical. Should not start before the Beta gate.

### Phase 2 · SubAgentProvider

Make the child-construction path a named seam so a child can be produced by
something other than the in-process executor.

- **Why:** Phase 1 produces proposals; accepting one must not hard-code how the
  child is built. This is also the precondition for Phases 3–5.
- **Constraint:** a refactor of an existing path, not a second orchestrator. The
  spawn schema, ownership registry and settlement contract stay as they are.

### Phase 3 · Durable Child Session

Children currently do not survive a restart: a resumed run reports them lost,
releases their scopes, and tells the model to re-delegate.

- **Why:** measured behaviour today, and it caps the value of any adoption
  improvement — a delegation that cannot outlive a restart is worth less than
  one that can.
- **Depends on:** Phase 2.
- **Care:** this touches provenance and recovery, both of which are currently
  clean. It needs the FS matrix re-run, not just its own tests.

### Phase 4 · Background-first UX

Background delegation is the runtime default; the surfaces do not yet make a
running child, its scope and its settlement legible enough to supervise.

- **Why:** dogfooding repeatedly showed the parent redoing work a child had
  already done. That is a visibility failure, not a runtime one.
- **Depends on:** Phase 2, ideally Phase 3.

### Phase 5 · Capability Negotiation

Model-declared capabilities already differ in ways that change delegation shape:
`k3` fans out to three children in one message; DeepSeek issues one, because it
declares no parallel tool calls. Measured in the cross-model experiment.

- **Why:** this is the one genuine model-dependent difference the programme
  found. It affects the *breadth* of a delegation, not whether one happens.
- **Explicitly not:** a delegation capability profile used to gate or select
  models. Arm C refuted that — a materially stronger model of the same family
  delegated at exactly the same rate.

## Standing constraints

These are not preferences; each is the product of a measured failure.

1. **No forced delegation.** No `ToolChoice::Required`, no auto-dispatch, no
   planner spawning on the model's behalf. KEEP stays a first-class outcome.
2. **No spawn-rate targets.** The goal is delegation used at the right time. An
   intervention that raises spawning while raising unnecessary delegation is a
   regression.
3. **No prompt tuning as a mechanism.** Wording changes require a proven
   semantic defect, not a low number.
4. **Power before runs.** Adoption on a fixed task varies 0.08–0.30 across
   batches. Twelve runs per arm cannot resolve anything short of a tripling.
   State the detectable effect before spending the runs.
5. **Eval observes; it does not special-case.** No `eval_mode`, no runtime
   branch on model identity, no scripted spawn counted as adoption.

## Evidence index

| topic | document |
| --- | --- |
| MA-WA1 closing record and hypothesis elimination | [`evaluations/MA-WA1-FINAL.md`](evaluations/MA-WA1-FINAL.md) |
| adoption micro-eval protocol | [`evaluations/E004-multi-agent-adoption.md`](evaluations/E004-multi-agent-adoption.md) |
| Delegation Advisor design | [`design/DELEGATION_ADVISOR_DESIGN.md`](design/DELEGATION_ADVISOR_DESIGN.md) |
| Beta gate execution | [`BETA_CLOSURE_PROGRAM.md`](BETA_CLOSURE_PROGRAM.md) |
| multi-agent architecture as shipped | [`MULTI_AGENT_PRODUCT_CLOSURE.md`](MULTI_AGENT_PRODUCT_CLOSURE.md) |
