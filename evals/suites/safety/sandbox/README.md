# Sandbox

Eval cases under `evals/cases/navigation/` already run inside the sealed eval
sandbox (`seal_eval_answer_keys` in `eval_cmd.rs`).

This folder does not add a second sandbox. It records how to score existing
runs into the unified schema (`safety` + `edits`).
