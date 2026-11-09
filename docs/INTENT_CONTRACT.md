# User Intent Contract

How one user message changes a goal's execution authorization. The contract
exists so that "继续", "go ahead", "続けて" and "等一下，先别做阶段 2" are all
read by the model that is already running the turn — never by a keyword list,
a second classifier call, or a client-side route.

## Ownership

| Decision | Owner |
| --- | --- |
| What the user meant (continue, pause, amend, cancel, question, new task) | The model, in the current turn |
| That the declaration quotes the user's own words | Runtime (`quote_is_in_input`) |
| Which goal it applies to | Runtime (bound goal, explicit id, or the single running goal) |
| Whether the transition is legal and ordered | `leveler_lifecycle::project_intent` |
| Which plan step a boundary names | The model's declared plan, by stable step id |
| The durable result | `leveler-storage` (`goals`, ordered `goal_intents` log) |
| What the client shows | Runtime facts projected by TUI / Web / CLI |

A declaration is model output. It can never create authority the user did not
state, and it is not evidence that the work was done. Only the quote ties it to
the user; only the runtime's validation admits it.

## Declaration

The model calls the `declare_intent` tool (offered at depth 0 on any writable
turn that has a goal lifecycle attached, including a Chat turn):

```json
{
  "source_quote": "继续执行，但第三阶段完成后暂停。",
  "goal_id": "…optional when the target is unambiguous…",
  "operations": [
    {"kind": "continue_task"},
    {"kind": "set_pause_boundary",
     "boundary": {"kind": "after_step", "step": {"step_id": "stage-3"}}}
  ]
}
```

Operations: `continue_task`, `pause_task`, `set_pause_boundary`,
`replace_pause_boundary`, `amend_current_task`, `start_independent_task`,
`cancel_task`, `answer_only`. They are applied in array order, so a compound
instruction is one declaration, not two decisions.

## Validation

1. **Provenance.** `source_quote` must be non-empty and occur in the newest
   user-authored input of the transcript (exact, or with whitespace runs
   collapsed). A quote from an older message is refused: an authorization that
   a newer message may already have reversed cannot be reused.
2. **Identity.** The target is the turn's bound goal, an explicitly named
   `goal_id`, or the single running goal of the task. Zero running goals and an
   unnamed declaration among several both refuse; the refusal lists the ids.
   The runtime never guesses which goal the user meant.
3. **Boundary.** `after_step` names a plan step id the model itself declared
   (`update_plan` with stable ids). A boundary over no model plan, or over an
   unknown id, is refused with the fix. Renaming a step does not move a
   boundary; the label is display only.
4. **Order and terminals.** `cancel_task`, `answer_only` and
   `start_independent_task` must be the only operation in their declaration.
   Everything else composes.
5. **State.** A settled goal accepts no transition except an explicit cancel
   acknowledgement; a paused goal accepts `continue_task`; a boundary is
   "reached" only when its step carries an explicit `completed` status. A step
   that was deleted, renamed without its id, replaced, split or merged is still
   outstanding — a plan edit is not a completion. The model may adjust the plan
   freely and re-declare the boundary when the stage really moved.

## State writers

`user_paused_at` / `pause_boundary` have one writer path per direction:

- a validated declaration (`continue_task` clears, a boundary sets it when
  already reached) records the state atomically with the ordered log;
- a window that closes on a reached declared boundary sets the pause (it never
  clears one);
- settling the goal clears both in the same write.

No reader of natural language can set or clear a pause. A status question is
answered without driving the goal (`answer_only`); a supplement is recorded
without releasing a pause (`amend_current_task`); an unrelated task needs an
explicit `task_relation: "new"` commit.

## Plan continuity

The plan belongs to the goal that declared it. A turn that continues the same
goal — an explicit resume or a Goal turn continuing a running goal — seeds the
prior plan before the model acts, so a new window sees the stages it already
finished. A fresh goal gets no seeded plan.

## Durable history

`goal_intents` (migrations 0053 and 0054) stores, per goal, a monotonic `seq`,
the `turn_id` whose transcript holds the raw user message, the verbatim
`source_quote`, the accepted operations in order, the resulting pause state and
boundary, and the timestamp. The log answers "who authorized this, with which
words, in which order" after a restart; the goal row remains the current
authority.

## Not in this contract

- No keyword table, language detection or per-message model call.
- No client-side continuation routing: clients submit the user's text
  unchanged and project what the runtime publishes.
- No model-generated completion proof: intent answers authorization only.
