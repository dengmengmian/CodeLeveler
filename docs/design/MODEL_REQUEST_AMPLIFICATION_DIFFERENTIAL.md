# Why CodeLeveler needs more model requests than DeepSeek Harness

Trajectory differential, not another guessed lever. Both harnesses ran the same
three frozen Phase C tasks on `deepseek-v4-flash` through the same gateway, and
both produced mechanically correct results. DSH's own `session.jsonl.zstd`
gives per-step tool calls; CodeLeveler's `session_messages` gives the same.

**Verdict: the amplification is real and consistent in magnitude, and it does
not localise to any mechanism that holds across more than one task.**

```
MODEL_REQUEST_AMPLIFICATION_ROOT_CAUSE = NOT_ISOLATED
ROOT_CAUSE_CONFIDENCE                  = LOW
PRODUCT_FAST_FIX_IMPLEMENTED           = NO
```

## The differential

post-closure cohort, all six runs mechanically PASS:

| task | CL steps | DSH steps | ratio | CL calls | DSH calls | CL tools/step | DSH tools/step |
|---|---:|---:|---:|---:|---:|---:|---:|
| C1 | 110 | 63 | 1.75× | 127 | 74 | 1.15 | 1.17 |
| C2 | 106 | 72 | 1.47× | 134 | 81 | 1.26 | 1.13 |
| C3 | 190 | 55 | **3.45×** | 193 | 63 | 1.02 | 1.15 |

**Tools per step are the same in both harnesses.** CodeLeveler is not failing to
batch — it is issuing 1.6–3.1× as many tool calls to reach the same fix. That
retroactively confirms the L1 batching rejection: there was never a batching gap
to close.

## The amplification does not sit in one phase

| task | CL pre-first-edit | DSH pre-first-edit |
|---|---:|---:|
| C1 | 22 / 110 = **20%** | 39 / 63 = **62%** |
| C2 | 25 / 106 = **24%** | 21 / 72 = **29%** |
| C3 | 157 / 190 = **83%** | 28 / 55 = **51%** |

C3 looked decisive on its own — 129 of its 135 extra steps fall before the first
edit, and it starts editing at turn 158 of 190 against DSH's step 29 of 55. But
C1 and C2 invert it: CodeLeveler reaches its first edit *sooner* than DSH, at
20% and 24% of the run against DSH's 62% and 29%.

On C1 and C2 the extra steps sit in implementation and iteration instead. There
is no stable phase, so by the ≥2-task standard this is:

```
EARLIEST_STABLE_DIVERGENCE = none found
ROOT_CAUSE_TASK_COVERAGE   = C3 only, and C3 contradicts C1/C2
```

## The one consistent difference, and why it is smaller than it looks

CodeLeveler marks far more tool results as errors:

| task | CL | DSH |
|---|---|---|
| C1 | 18/127 = 14.2% | 1/74 = 1.4% |
| C2 | 15/134 = 11.2% | 1/81 = 1.2% |
| C3 | 18/193 = 9.3% | 1/63 = 1.6% |

51 against 3, consistent across all three tasks. But classified:

| kind | n | share |
|---|---:|---:|
| COMMAND_NONZERO_EXIT | 30 | 58.8% |
| POLICY_REFUSAL | 7 | 13.7% |
| PLAN_GATE_REFUSAL | 6 | 11.8% |
| WORKSPACE_SCOPE_REFUSAL | 3 | 5.9% |
| PATH_NOT_FOUND | 2 | 3.9% |
| apply_patch failure | 2 | 3.9% |
| TOOL_TIMEOUT | 1 | 2.0% |

**Most of it is a labelling difference, not a reliability difference.** A `grep`
with no match exits 1. A failing test exits non-zero. Both are useful
information about the repository, and CodeLeveler files them as tool errors
while DSH does not.

Excluding those, the genuinely runtime-induced refusals are 21 across three runs
— 4.6% against DSH's 1.4%, about five per run:

- **PLAN_GATE_REFUSAL (6).** The model calls a tool, the runtime refuses with
  "call update_plan first", the model complies, and the original call is
  reissued. Two wasted round trips per run, and the gate fires reactively rather
  than being stated before the first attempt.
- **WORKSPACE_SCOPE_REFUSAL (3).** `read_file` and `list_files` refuse an
  out-of-workspace path; the model falls back to shell and succeeds. Same
  asymmetry recorded in `SHELL_INVESTIGATION_ERGONOMICS.md`.
- **POLICY_REFUSAL (7).** Backgrounding refused in a foreground shell,
  `update_goal` refused over an incomplete plan.

Five wasted rounds per run against amplification of +47, +34 and +135 steps.
That is 4–10%, which is real and worth fixing on its own terms but is not the
answer to this package's question.

## What was checked and found not to explain it

- **Tool result truncation.** Zero truncation markers across 193 C3 results;
  median result 1.2 KB, max 18 KB. Nothing forces a re-query.
- **Structured-tool capability gaps.** Already closed: 87% of inspection shell
  is compositional.
- **Batching.** Tools/step is equal in both harnesses.
- **Context growth.** 98.6% cached; latency flat in context size.
- **Environment sandboxing.** Both arms get the same isolated `HOME`, and
  `go env GOMODCACHE` inside a CodeLeveler run correctly returns the sandboxed
  path. The sandbox is not leaking dependency resolution.

## Honest conclusion

CodeLeveler takes 1.5–3.5× the model steps DSH does on the same task with the
same model, and after a full trajectory differential across three tasks there is
no single product-owned mechanism that accounts for the bulk of it. Where the
extra steps go changes per task. The only cross-task product finding — reactive
refusals — accounts for well under a tenth.

```
NO_PRODUCT_DEFECT_FOUND_WITH_CURRENT_EVIDENCE = YES
FAST_LEVER_SEARCH_STATUS = CLOSED_PENDING_NEW_EVIDENCE
```

That is not "every round is necessary" and not "FAST is impossible". It is that
the difference now looks like trajectory rather than mechanism, and trajectory
is not something to change by guessing at it — this investigation has already
rejected six such guesses.

## What would reopen this

1. **A per-step semantic diff on one task**, aligning CL and DSH round by round
   to find where the two agents' understanding of the same repository first
   diverges. Expensive to do properly and the only route that has not been
   tried.
2. **Fixing the reactive refusals** (plan gate, workspace scope) — small, safe,
   and worth doing for their own sake, but not expected to move the ratio.
3. **Error labelling.** Filing a no-match grep and a failing test as tool errors
   may or may not affect the model's next move. That is a behavioural
   hypothesis, and this investigation's record with those is 0 for 6.
