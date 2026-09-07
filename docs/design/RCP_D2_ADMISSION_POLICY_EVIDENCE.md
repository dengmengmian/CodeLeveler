# RCP-D2 — what the logs actually say about continuation

Measured, not argued. Every number below is read off durable event logs with
`dogfood/eval/phase-c/round_delta_report.py`, which reconstructs per-round
control-plane deltas — mutations, verification outcomes, refused closes — and
nothing else. No model output, no semantics.

## The question

D2 may change whether the next main-task model call happens. To do that safely
it needs a mechanical predicate that separates *a loop that is getting
somewhere* from *a loop that is not*, using facts the runtime already has.

## What a "control-plane delta" is

A round has one when at least one of these advanced:

- the modified-file set grew, or mutation operations increased
- a verification ran green
- a close attempt was refused (the runtime told the model it is not done)

A round without one is not a round where the model learned nothing. It is a
round where **the runtime observed nothing it could act on**. That distinction
is the whole difficulty.

## Post-closure cohort, CodeLeveler arm

| | C1 | C2 | C3 |
|---|---|---|---|
| terminal state | CompletedUnverified | Unknown | CompletedUnverified |
| rounds | 110 | 77 | 190 |
| rounds with a control-plane delta | 30 | — | 23 |
| rounds without one | 80 | — | **167** |
| longest unbroken quiet run | 26 | 37 | **129** |
| total cost | $0.94 | $0.70 | $2.02 |
| cost inside quiet rounds | — | — | **$1.68 (83%)** |

C3's quiet run is rounds 26 → 154, unbroken. Inside it: 104 `shell_command`,
23 `read_file`, 2 `update_plan`. Zero mutations. Zero verifications. Zero
refused closes. 9.38M tokens.

## Why the existing guards never fired

`no_progress_streak` peaked at **0** and `stagnation_streak` at **2** across all
three runs. Both are reset by a novel read or search, and the model was reading
novel things: 115 of those 132 shell commands were distinct. By the drive
loop's definition of progress, nothing was wrong for 129 rounds.

The guards are not broken. They answer a different question — "is the model
repeating itself?" — and the answer was honestly no. Nobody was asking "has the
runtime learned anything in a hundred rounds?"

That is the gap, and it is now measured rather than inferred.

## Why D2 does not ship a threshold

To turn that gap into a stopping rule, the rule needs a bound. Calibrating one
requires knowing how long a *successful* run legitimately goes quiet.

**No run in this cohort reached `Verified`.** C1 and C3 are
`CompletedUnverified`; C2's terminal state is `Unknown`. The observed quiet-run
envelope — 26, 37, 129 — is entirely drawn from runs that did not succeed. A
threshold picked from it would be calibrated against failure alone, with
nothing to say whether it also cuts the investigation a successful run needs.

That is exactly the arbitrary no-progress limit RCP-D2 is instructed not to
introduce, and dressing it in this evidence would not make it less arbitrary.

```
RCP_D2_POLICY_CHANGE = NOT_PROVEN
POLICY_EVIDENCE_INSUFFICIENT = calibration data, not signal
```

The signal is real. The bound is not yet earned.

## The one candidate, stated so it can be judged

If the user wants a first rule, this is the least arbitrary one available,
because its only constant is a fraction of a budget the product already chose:

> When half the task's total round budget has been spent with **zero**
> control-plane delta, and a close has already been refused, the next admission
> returns `EnterCloseout` rather than `ContinueCurrentWindow`.

Properties worth noting:

- It introduces no round count of its own; it scales with `DEFAULT_TASK_ROUND_BUDGET`.
- Against this cohort it fires **only on C3**, at roughly round 126 — cutting
  about 28 of 190 rounds (~15%). C1 (26) and C2 (37) are far below it.
- `EnterCloseout` is not a verdict. The terminal state is still decided by
  `completion_debt()` and the reconciliation gate, so the rule cannot turn
  anything into `Verified`, and cannot turn a real completion into a failure —
  it can only stop the runtime paying for rounds it cannot see.

What it needs before shipping: at least one `Verified` run to bound the quiet
envelope from the success side. Until then it is a proposal with a measurement
behind it, not a policy.

## What would make this decidable

1. A cohort with at least one `Verified` CodeLeveler run, so the quiet envelope
   has a success side.
2. Or a task whose reference fix is known to be reachable in few rounds, run to
   `Verified`, to establish the floor directly.

Either one turns the fraction above from a guess into a bound.
