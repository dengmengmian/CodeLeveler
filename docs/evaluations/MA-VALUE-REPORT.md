# MA-VALUE — Round 1: INVALID, and what it taught

**Ran:** 2026-08-23 · 40 runs (2 tasks × 2 arms × 10) · `deepseek-v4-flash` ·
frozen tasks/rubric committed before execution (`5308541`)

## Executive Summary

```
Result:  INVALID — no treatment occurred
```

**The treatment arm never delegated. Zero spawns in 20 of 20 runs.** The
experiment compared single-agent against single-agent, so every arm-vs-arm number
it produced is meaningless as a statement about Multi-Agent value.

Reported as a failure rather than dressed up as "no significant difference
found", which is what these numbers would look like to a reader who was not told
this.

Two design faults, both in the task design, both mine.

## Fault 1 — the tasks did not invite delegation

| Task shape | Delegation observed |
| --- | --- |
| *"Is this project up to standard? Investigate thoroughly."* | **3–4 explorers, every run** (5/5 across earlier sessions) |
| *"Investigate the sandbox/command-execution path and answer 3 questions"* | **0 spawns, 10/10 runs** |
| *"Investigate the event-log path and answer 3 questions"* | **0 spawns, 10/10 runs** |

Delegation is opportunity-based: the model elects it. A broad, decomposable
question invites splitting; a narrow single-chain investigation does not, because
there is nothing to split. The agent read the files itself and answered.

**How I caused it.** The tasks were built to be *gradeable* — narrow scope,
checkable facts, one subsystem each — and narrowing them is precisely what
removed the parallelism the treatment needed. The two goals pulled in opposite
directions and I optimised for one without noticing it destroyed the other.

## Fault 2 — the rubric had no headroom

| | Task A | Task B |
| --- | --- | --- |
| single | **8.00 / 8** (sd 0.00) | **8.00 / 8** (sd 0.00) |
| multi | 7.90 / 8 (sd 0.32) | 7.70 / 8 (sd 0.48) |

Single-agent scored a **perfect 8/8 on all 20 runs**. Even if delegation had
occurred there was nothing left to win. A rubric that everyone maxes out cannot
discriminate.

The paired differences (−0.10 and −0.30) are not evidence that Multi-Agent is
worse. They are single-agent runs of the same product scoring 8 and 7 on a
ceiling.

## What went right, and is worth keeping

- **The blind-scoring harness works.** Answers are written under opaque ids, the
  arm lives in a separate mapping, and the scorer reads only answer text — blind
  by construction, not by promise.
- **Quality is a far lower-noise measure than cost, as predicted.** Score sd was
  0.0–0.48 on an 8-point scale; cost CV was ~55 %. The plan's central argument —
  lead with quality, not cost — survives.
- **The runs were cheap.** Median 60 s, vs ~20 min for the earlier baseline,
  because a pure-investigation task never triggers a cold `cargo test`.

## A scorer bug, found and fixed before it reached a conclusion

The first scoring pass marked item A3 as *stated incorrectly* in several answers.
Inspection showed the answers were right: the "wrong claim" pattern was
word-distance based and fired on the filename `windows_sandbox.rs` and on
comparison tables listing the three platforms together. Tightened to require an
actual claim (*Windows uses seatbelt/bwrap*), not proximity.

Recorded because an automated grader that is wrong produces confident numbers,
and this one would have reported a quality deficit that did not exist.

## Also invalidated: the earlier ceiling reading

An earlier note in this session said Task B was at ceiling and Task A had spread.
That spread was the scorer bug, not the answers. After the fix, **both** tasks
sit at ceiling.

## Blinding: one sample compromised

While checking that answers were gradeable at all, I read `mapping.tsv` to obtain
an id, which revealed run `925e344db1e1` as task A / single / rep 1. One of 40
de-blinded. It changes nothing here — the run is invalid on other grounds — but
it is recorded rather than quietly dropped. Sampling should use `ls`, never the
mapping.

## What a valid round 2 needs

The two faults have one fix each, and they are in tension, which is the actual
design problem:

1. **Tasks broad enough to invite splitting.** Not "investigate subsystem X", but
   something with genuinely independent parts — *"investigate these three
   unrelated subsystems and report on each"*. Decomposable by construction, so
   the model has something to delegate.
2. **A rubric with headroom.** Enough items, and hard enough ones, that single
   agent lands well short of maximum. If the control scores 8/8 the experiment is
   over before it starts. Calibrate on control runs *first*, and only freeze the
   rubric once the control mean sits nearer 60 % than 100 %.
3. **A treatment check that runs before scoring.** If the treatment arm did not
   spawn, the round is void — that should be asserted automatically, not
   discovered afterwards.

Point 3 is the cheap lesson: **verify the treatment happened before spending
anything on measuring its effect.**

## Product recommendation

None from this round. It measured nothing about Multi-Agent value, and saying
otherwise would be worse than saying nothing.

The one product-relevant observation is not about value but about **when
delegation occurs**: broad, decomposable questions produced 3–4 explorers every
time, while focused single-chain investigations produced none in 20 attempts.
That is consistent with delegation being used where it fits rather than
uniformly — which is the behaviour the roadmap says to want. It is an
observation from an invalid experiment, and should be re-measured deliberately
before it is relied on.
