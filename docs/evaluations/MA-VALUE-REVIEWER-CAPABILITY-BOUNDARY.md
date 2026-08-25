# Reviewer Capability Boundary — study

**Status:** three calibration rounds complete · **G4 not cleared** · 2026-08-25
**Arm:** control only · **No Reviewer has been run in any round**

Three rounds tried to find coding tasks where an independent Reviewer has room
to change the outcome. None found any.

That is a result about the product, not only about the eval harness, and it is
worth stating before the method:

> On self-contained tasks of this size, `deepseek-v4-flash` does not leave the
> kind of defect an independent Reviewer would catch. **Where a Reviewer earns
> its cost is not in this task population, and the burden is now on finding a
> population where it does — not on building a better trap.**

## The three rounds

| Round | Hypothesis | Cases | Control pass | Outcome |
| --- | --- | ---: | ---: | --- |
| [V1](MA-VALUE-REVIEWER-TASK-CALIBRATION.md) | known bug patterns are a blind spot | 3 | 7/9 | falsified |
| [V2](MA-VALUE-REVIEWER-TASK-CALIBRATION-V2.md) | cross-step coordination is a blind spot | 5 | 17/18 | falsified |
| V3 (this round) | specification/intent gaps are a blind spot | 5 | **15/15** | falsified |

**Cumulative: 13 cases authored, 37 of 42 control runs passed (88 %).**

Every one of the 13 cleared a discriminance gate first: a hand-written trap
implementation that passes the visible tests and fails the hidden ones. The
traps are real. The model does not fall into them.

## V3 design, and why it was a fair test

V1 and V2 each ended with a plausible excuse. V3 removed both.

| V1/V2 weakness | V3 |
| --- | --- |
| Contract doc comments enumerated the invariant the hidden test checked | Task text states a **product request only**. Ownership, tenancy and the frozen API contract are never mentioned. |
| Single file, under 100 lines | Multi-package repos: `internal/tenant`, `internal/store`, `internal/cache`, `internal/service`, plus `docs/API.md` and `docs/MIGRATIONS.md` |
| Trap = implementation error | Trap = **implementation correct, intent violated** |

### Results

| Case | Hidden intent gap | Pass | Rounds | Reads | Searches |
| --- | --- | ---: | ---: | ---: | ---: |
| `v3-tenant-cache` | cache key omits the tenant → cross-customer leak | 3/3 | 6.7 | 5.3 | 1.0 |
| `v3-nplus1-listing` | per-row author fetch → 300 queries for 300 rows | 3/3 | 6.0 | 3.0 | 2.0 |
| `v3-ownership-authz` | `Rename` without the ownership check its siblings have | 3/3 | 9.0 | 4.3 | 1.3 |
| `v3-api-contract-drift` | `count` becomes the total, `[]` becomes `null`, error code renamed | 3/3 | 9.0 | 4.3 | 1.0 |
| `v3-migration-rollback` | `Down` drops the new columns without rebuilding `name` | 3/3 | 8.7 | 4.0 | 3.3 |

### What the model actually did

Not luck. In each case it found the constraint and said so unprompted.

`v3-tenant-cache` — the task says only *"use the cache to serve repeated
lookups"*. Tenancy appears in a package comment two directories away:

```go
// User ids are only unique per tenant, so the cache key must be
// tenant-scoped — "acme"/"u-1" and "globex"/"u-1" are distinct rows.
key := tenant.From(ctx) + "\x00" + userID
```

`v3-ownership-authz` — the task says only *"let members rename their own
documents"* and never mentions authorization. The model inferred the rule from
`Archive` and `Body` in the same file:

```go
if !p.IsAdmin() && d.OwnerID != p.UserID {
    return ErrForbidden
}
```

`v3-api-contract-drift` — kept `bad_request` as the error code, kept the empty
page as `[]Event{}` rather than `null`, and kept `count` as the page size. All
three are stated only in `docs/API.md`.

The exploration numbers say the same thing: 3–5 file reads and 1–3 searches per
run. It reads the repository before it writes.

## What this means for the Reviewer

The product hypothesis has now failed in three distinct forms:

```
Reviewer finds bugs the Worker missed          ✗ V1
Reviewer finds coordination the Worker missed  ✗ V2
Reviewer finds intent the Worker missed        ✗ V3
```

The honest reading is not *"Reviewer has no value"*. It is:

> **At this task size, a competent model's self-verification is not the
> bottleneck.** A second pair of eyes reviewing 100 lines of code that one
> agent just wrote, and got right, adds cost and no quality.

That is a real product finding and it argues for something concrete: a Reviewer
that fires on *every* mutation is a tax. If a Reviewer belongs in the product at
all, it belongs where the conditions that make review valuable actually hold.

## Where the boundary plausibly is — untested

None of the following is claimed. Each is a hypothesis the three rounds point
at but do not test.

| Condition | Why this study could not reach it |
| --- | --- |
| **Task size beyond one context** | Every case here fits comfortably in one window. Review value classically comes from the reviewer holding context the author lost. |
| **Long sessions with accumulated drift** | Each case is one turn, one edit. `icg-5` (four asks in one batch) is the closest existing shape and is also at ceiling — but it scores with visible tests. |
| **A weaker model** | The single-variable experiment nobody has run: same cases, `glm-5.2`. If the ceiling is model capability, it moves. If it is task size, it does not. **This is the cheapest next experiment in the whole programme.** |
| **Genuinely ambiguous requirements** | Every case here has one right answer. Real requests do not. |
| **Unfamiliar or in-house conventions** | The one case that ever failed for a real reason — `go-maplimit-first-error`, which asks for lowest-index error where every library returns time-first — is exactly this shape. |

That last row is the strongest signal in 42 runs, and it is a sample of one.

## Recommendation

**Stop authoring cases.** Three rounds, thirteen cases, one signal. A fourth
round of hand-designed traps is the same bet at the same odds.

Run the two cheap discriminating experiments instead:

1. **Model swap — 15 runs.** The V3 suite against `glm-5.2`, control only.
   Separates *"the tasks are too easy"* from *"this model is too strong"*. If
   the weaker model drops into the band, the Reviewer experiment is runnable
   today with a documented model caveat. If it does not, the constraint is task
   size and every future case must be bigger, not cleverer.
2. **Anti-idiom variant — 6 runs.** Take `v3-tenant-cache` and require the
   *opposite* of the idiom (a deliberately global cache with an explicit
   allow-list, say). Tests the one hypothesis with any supporting evidence.

Twenty-one runs to settle what another authoring round would only guess at.

## Consequence for the product

Two things follow regardless of how those experiments land.

**The Reviewer trigger should stay `auto`, and `auto` should stay
conservative.** Three rounds found no small-task population where review pays.
Firing on every mutation would spend tokens for measured-zero return.

**The Multi-Agent UI should not be built around Reviewer finding count.** The
[UX design](../design/MULTI_AGENT_UX.md) already avoids this — it renders
contribution, and *"reviewed, nothing to flag"* is a first-class outcome. This
study is the evidence for that choice: on 15 of 15 V3 runs, nothing to flag is
the correct answer.

## Formal experiment readiness

**Not ready.** G4 (control arm has headroom) is not cleared and cannot be
cleared by this task population.

G1–G3 remain green and are unaffected: contribution is observable on the
reviewer path, unmeasured is distinguished from zero, and the independent
`expect` reaches the run record.

## Artifacts

| Path | Contents |
| --- | --- |
| `evals/reviewer-v3/` | 5 V3 cases, all past the discriminance and stability gates |
| `evals/reviewer/` | V2 cases + `go-maplimit-first-error` (race fixed, needs re-measurement) |
| `evals/reviewer-rejected/` | V1 rejects, kept with reasons |
| `eval/runs/MA-VALUE-REVIEWER-CALIBRATION-self-*/` | three calibration batches |

The suite is not wasted. Thirteen cases with hidden acceptance criteria and a
verified discriminance gate are a reusable instrument — for the model swap
above, for regression against future models, and for any experiment that needs
tasks whose score is not the test the agent can read.

## Method notes worth keeping

- **The discriminance gate paid for itself.** Reference-passes / trap-fails,
  run before any model call, caught two defects in V1's Task A and a false
  positive in V2's Task 1 for zero tokens.
- **The stability gate exists because V2 shipped a flaky case.**
  `go-maplimit-first-error` scored 4-pass/2-fail on identical code; a paired
  experiment would have read scheduler noise as Reviewer value. Every case is
  now run 4× on fixed code before it is admitted.
- **`expect` must not swallow output.** V1's scripts sent everything to
  `/dev/null`; the first failure took a full manual re-run to diagnose.

## Related

- [MA-VALUE-REVIEWER-TASK-CALIBRATION](MA-VALUE-REVIEWER-TASK-CALIBRATION.md) — V1
- [MA-VALUE-REVIEWER-TASK-CALIBRATION-V2](MA-VALUE-REVIEWER-TASK-CALIBRATION-V2.md) — V2
- [MA-VALUE-REVIEWER-FORMAL](MA-VALUE-REVIEWER-FORMAL.md) — the protocol G4 gates
- [MULTI_AGENT_UX](../design/MULTI_AGENT_UX.md)
