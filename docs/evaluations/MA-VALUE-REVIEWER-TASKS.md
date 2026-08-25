# MA-VALUE-REVIEWER-TASKS — a case set with room to improve

**Status:** design · **Opened:** 2026-08-24 · **Not executed**

Design input for the Reviewer formal experiment. Nothing here runs a model.
No product behavior changes.

## Why a new set is required

The pilot's five cases returned a control arm at **5/5, `quality_score_100 = 100`**.
A saturated control makes "Reviewer raises final correctness" unmeasurable: the
treatment has nowhere to go.

This is not specific to those five. Every recorded baseline for
`deepseek/deepseek-v4-flash` is at ceiling:

| Baseline | Cases | Result |
| --- | --- | ---: |
| `icg-integrated.json` | icg-1 … icg-5 | **15/15** |
| `c4-self-healing.json` | c4-r1 … c4-r8 | **24/24** |
| `c2-bv2-navigation.json` | navigation | **24/24** |
| `c3-edit-reliability.json` | edit | **24/24** |
| `icg6r-honest-failure.json` | icg-6r | 1/3 |

Source: `evals/baselines/*.json`, 3 repetitions each, same model as the pilot.

**The existing corpus cannot host this experiment.** The only sub-ceiling case
is `icg-6`/`icg-6r`, which is an unsolvable-task honesty probe — 0/3 and 1/3
are the *intended* outcome there, not headroom. Selecting a different subset of
`evals/` does not fix the problem; the corpus has to be extended.

That is the finding of this phase. Everything below is the design for the
extension.

## Target

Control (single agent, self-verify) pass rate **50–70 %** per case, measured at
n ≥ 3 before the case is admitted.

Above 70 % there is no room for the treatment to show a difference. Below 50 %
the task is failing for reasons a reviewer cannot address (spec ambiguity,
context limits) and the experiment measures task difficulty instead of review
value.

**Calibration is a gate, not a formality.** A case whose measured control rate
lands outside the band is dropped, not adjusted until it fits — adjusting the
task after seeing the treatment arm is how a null result gets tuned into a
positive one.

## What makes a task reviewable

The pilot's cases failed a second way that pass rate alone does not show:
`rust-race-counter` and `ts-concurrency-limit` have a single correct answer
that the test either catches or does not. There is no state in which the
implementation passes the visible bar and a reviewer still has something true
and useful to say.

A task can only measure review value when:

1. **A plausible-but-wrong implementation exists** that passes the obvious
   check. If every wrong answer fails the first test, self-verification is
   sufficient by construction.
2. **The defect is visible in the diff**, not only at runtime. The reviewer is
   read-only; it sees code, not a profiler.
3. **The verification is independent and hidden** — the agent cannot read the
   check it will be scored against.
4. **The fix is small once seen.** Review value is detection. A task where
   knowing the defect still costs a rewrite measures implementation capacity.

Condition 1 is the one the current corpus lacks.

## Categories

Four, chosen because each admits a wrong-but-passing implementation.

### C1 · Concurrency

The defect survives the test that was written for it.

| Shape | Plausible wrong answer that passes |
| --- | --- |
| Race under contention | Correct under the test's thread count, torn above it |
| Cancellation | Cleanup on the happy path, leak on cancel |
| Async lifecycle | Await ordering that only deadlocks when a queue fills |

Reviewer's lever: lock ordering, missed `Drop`, an `await` inside a critical
section — all visible in a diff.

### C2 · Security

The check exists and is incomplete.

| Shape | Plausible wrong answer that passes |
| --- | --- |
| Validation bypass | Validates the documented shape, misses an alternate encoding |
| Permission check | Checks on the primary path, not the batch/retry path |
| Unsafe handling | Redacts the named keys, misses nesting or case |

`ts-redact-secrets` is close to this shape but its visible test already covers
recursion, so the wrong answer does not survive. The variant needed is one
where the visible test covers the flat case and the hidden `expect` covers the
nested one.

### C3 · State machine

The transition table is right for the states the test drives.

| Shape | Plausible wrong answer that passes |
| --- | --- |
| Retry | Retries the failure, resets a counter it should keep |
| Recovery | Restores state, drops one field that only matters on second recovery |
| Lifecycle | Correct order, no idempotence on repeat entry |

### C4 · Refactoring / migration

Behavior preserved for the callers the test exercises.

| Shape | Plausible wrong answer that passes |
| --- | --- |
| Compatibility | New path correct, old serialized form no longer parses |
| API migration | All in-repo callers updated, public signature silently narrowed |

`icg-3-cross-module` is this family and is at ceiling; the variant needed adds
a caller the agent must find without being told it exists.

## Case definition schema

Each case is a vendored `EvaluationCase` YAML plus a calibration record.

```yaml
id: <stable-kebab-id>
name: <one line, what the user asked for>
repo: fixtures/repos/<name>        # or inline `files:`
category: concurrency | security | state_machine | refactoring
max_rounds: <int>

task: |
  <what the user asked, in product terms. Never names the defect.
   Never names the file to change unless a real user would.>

# The trap: an implementation a competent agent plausibly writes that
# satisfies every visible signal. Prose, for the case author and reviewer
# of this suite — never rendered into any agent's prompt.
hidden_defect_opportunity: |
  <the wrong-but-passing implementation, and why it is tempting>

# Independent, hidden, run after the agent stops.
expect:
  program: bash
  args: ["-c", "<covers the trap; the agent never sees this>"]

gold:
  must_hold:
    - <behavior the correct answer has>
  must_not:
    - <what the trap answer does>

calibration:
  control_pass_rate: <measured, n>=3>
  measured_at: <date>
  admitted: true | false
```

`hidden_defect_opportunity` and `gold` are documentation for whoever audits the
suite. They do not reach a prompt, and no scoring reads them — `expect` is the
score.

## Scoring

Unchanged from the pilot, and deliberately so:

- **Task success** — the independent `expect`, never an agent's summary.
- **Reviewer contribution** — `ChildResultProjection` on `sub_agent_finished`,
  now available on the independent-review path (Phase 1).
- **Finding count is not a success metric.**

One addition this case design makes possible: because each case has a named
trap, a finding can be checked against it. A reviewer that finds the trap and a
reviewer that finds three unrelated style issues are not the same contribution,
and `findings_verified` alone does not separate them. That check is a human read
of the finding text against `hidden_defect_opportunity`, recorded as
`trap_found: true | false | null`. It is a secondary metric and it is not
automated — an LLM judge scoring its own family's output is not independent.

## Build order

1. **Calibrate before authoring at scale.** Take three candidate cases, one
   each from C1/C2/C4, run control-only at n=3, keep what lands in 50–70 %.
   Authoring twelve cases before measuring one is how the pilot's set happened.
2. Extend to 6–8 admitted cases, ≥ 1 per category.
3. Vendor under `evals/reviewer/`, pointers in
   `eval/suites/multi_agent/reviewer_value/cases/`.
4. Record every calibration run, including the rejects, in this document.

Step 1 requires real model runs and is therefore not done here.

## Open question for the owner

The band assumes the control arm should fail sometimes. An alternative framing
is to keep tasks the control passes and score **defect density in the passing
implementation** — a reviewer that catches a latent bug in code that ships
green is real value the pass/fail estimand cannot see.

That needs a different primary metric, not a different case set. Recorded here
rather than decided: it changes what the experiment measures.

## Related

- [MA-VALUE-REVIEWER-FINAL](MA-VALUE-REVIEWER-FINAL.md) — why the pilot stopped
- [MA-VALUE-REVIEWER-PILOT](MA-VALUE-REVIEWER-PILOT.md) — original protocol
