# Micro tool usage

Not a new harness. Tool-call shape is already collected by
`crates/leveler-cli/src/eval_signals.rs` and `leveler eval run` on `evals/smoke`
and `evals/core`.

This folder exists so the eval-gate map has a place for E002. Until a dedicated
micro set lands, run:

```sh
leveler eval run --cases evals/smoke --model <model>
```

Adoption micro batches also record `edits.parent_tool_calls` and
`edits.first_edit_round` from EventLog for the same run that produced a
delegation decision.
