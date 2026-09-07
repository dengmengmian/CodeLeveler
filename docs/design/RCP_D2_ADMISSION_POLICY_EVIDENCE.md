# RCP-D2 — the quiet-interval hypothesis, calibrated and refuted

Measured, not argued. Every number is read off durable event logs with
`dogfood/eval/phase-c/round_delta_report.py`, `quiet_envelope.py` and
`policy_replay.py`, which reconstruct per-round control-plane deltas and
nothing else: no model output, no semantics.

**Result: the hypothesis is refuted with sufficient data, not blocked for lack
of it.** An earlier draft of this document said the evidence was insufficient.
That was wrong, and it was wrong for an instructive reason — see "What the
first pass got wrong".

## The hypothesis

A round has a **control-plane delta** when at least one of these advanced: the
modified-file set or mutation-operation count, a verification outcome, or a
refused close. A round without one is not a round where the model learned
nothing; it is a round where **the runtime observed nothing it could act on**.

C3 spent 129 consecutive rounds that way — 104 shell commands, 23 reads, zero
mutations, zero verifications, zero refused closes, 9.38M tokens, 83% of the
session's cost. The existing guards never fired: `no_progress_streak` peaked at
0 and `stagnation_streak` at 2, because both reset on a novel read and 115 of
those 132 shell commands were distinct.

The hypothesis that follows is obvious, and wrong: *a long control-plane
silence means the loop is not converging, so the runtime should stop paying
for it.*

## What the first pass got wrong

It used `Verified` as the definition of a successful run. No run in the cohort
reached `Verified`, so it concluded there was no success-side data to calibrate
against.

`Verified` means the runtime holds authoritative proof. It is not the same
question as *was the changeset correct*. The frozen cohorts already answer that
question with a mechanical oracle and a blind human review, and the answer had
been sitting in `mechanical-summary.json` and `human-decisions.json` the whole
time.

## The successful cohort

Seven CodeLeveler runs, all mechanically correct. The formal three were also
ACCEPTed under blind review (`blind_mapping.json`: C1-C, C2-B, C3-A are all
codeleveler); the post-closure three carry mechanical pass only.

| run | terminal state | mechanical | human | main rounds | Q0 max quiet | Q1 quiet that recovered | Q3 quiet before last mutation | last mutation |
|---|---|---|---|---:|---:|---:|---:|---:|
| formal C1 | — | PASS | ACCEPT | 100 | 33 | 33 | 33 | 94 |
| formal C2 | — | PASS | ACCEPT | 185 | 41 | 41 | 41 | 171 |
| formal C3 | — | PASS | ACCEPT | 159 | 87 | 87 | 87 | 148 |
| post-closure C1 | CompletedUnverified | PASS | — | 123 | 26 | 26 | 26 | 100 |
| post-closure C2 | Unknown | PASS | — | 78 | 37 | 37 | 37 | 77 |
| post-closure C3 | CompletedUnverified | PASS | — | 200 | **129** | **129** | **129** | **183** |
| pre-D C3 @47c2d7d | CompletedUnverified | PASS | — | 145 | 26 | 26 | 26 | 123 |

```
SUCCESS_QUIET_MAX                        = 129
SUCCESS_RECOVERABLE_QUIET_MAX            = 129
SUCCESS_POST_CLOSEOUT_RECOVERABLE_QUIET  = 129
```

Every one of those longest streaks **recovered**. In post-closure C3 the last
real change landed at round 183, after the 129-round silence that ended at 154.
The run the earlier draft held up as the waste case is a mechanically correct
run whose fix arrived on the far side of the silence.

Failure-side samples (`prune-ab`, 100 rounds each) show quiet streaks of 50 and
74 — squarely inside the success envelope.

```
QUIET_ENVELOPE_SEPARATION = NONE
```

## Replay — the hard gate

`policy_replay.py` sweeps the rule *N consecutive quiet rounds → EnterCloseout*
over the successful cohort and asks, for each N, whether it fires before that
run's last real progress.

| N | fires on | truncates a successful run |
|---:|---:|---:|
| 20 | 6 | 6 |
| 30 | 5 | 5 |
| 40 | 3 | 3 |
| 50 | 2 | 2 |
| 80 | 2 | 2 |
| 100 | 1 | 1 |
| 120 | 1 | 1 |
| 130 | 0 | 0 |
| 150 | 0 | 0 |

Conditioning on "only after a close has been refused" changes which runs are
hit, not the shape: 20→4/4, 40→1/1, 120→1/1, 130→0/0.

**There is no N that fires and is safe.** Below 130 every firing is a
truncation; at 130 and above the rule is inert inside a 200-round budget.

```
CANDIDATE_POLICY_TRUNCATES_SUCCESSFUL_RECOVERY = YES  (every candidate)
D2_EVIDENCE_GATE = FAIL
RCP_D2_POLICY_CHANGE = NOT_PROVEN
```

The candidate proposed in the previous draft — half the round budget,
conditioned on a refused close — fires on post-closure C3 at round 125 and
would have cut it 58 rounds before its fix. It is withdrawn.

## What this actually establishes

Both halves of the naive reading are wrong:

```
EXPLORATION_NOVELTY        != CONTROL_PLANE_PROGRESS   (true, and known)
CONTROL_PLANE_QUIET        != NOT_CONVERGING           (this is the new part)
```

Long silent investigation is not a pathology in this runtime on these tasks; it
is how correct runs reach their fix. The runtime's blindness during that
stretch is real, and RCP-A/B/C were right to make what it *can* see durable and
single-authority. But that blindness is blindness to investigation, not a
signal of failure, and it cannot carry a stopping rule.

## Where FAST has to come from instead

Not from stopping earlier on the signals the runtime already has — that is what
was just refuted. The remaining directions, in the order the evidence supports:

1. **Make investigation cheaper rather than shorter.** post-closure C3 spent
   9.38M tokens on 129 rounds of reading. Prompt-cache hit rate is already
   ~98% on input; the cost is the sheer number of round trips over a growing
   transcript. Batching, or letting one round carry more of the search, attacks
   the same waste without touching convergence.
2. **Give the runtime a signal it does not have.** Every rule tested here reads
   facts the runtime already holds. A materially better admission decision
   needs a fact that does not exist yet — and inventing one is a new subsystem,
   which this program has consistently refused for good reasons. It would need
   its own justification, not a footnote here.
3. **Leave the ceiling where it is.** The 200-round budget already bounds the
   worst case, and post-closure C3 shows the boundary being used productively.

## Reproducing this

```
python3 round_delta_report.py <sessions.db>          # per-round deltas
python3 quiet_envelope.py     <sessions.db> [label]  # Q0–Q3 for one run
python3 policy_replay.py      <label>=<db> [...]     # the hard gate sweep
```

All three live in `dogfood/eval/phase-c/` and read only durable logs.
