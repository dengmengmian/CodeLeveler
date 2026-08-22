# Sandbox

In-repo integrity work: `docs/C2_3C_S_EVAL_SANDBOX_INTEGRITY.md`.
Eval cases under `evals/navigation/` already run inside the sealed eval
sandbox (`seal_eval_answer_keys` in `eval_cmd.rs`).

This folder does not add a second sandbox. It records how to score existing
runs into the unified schema (`safety` + `edits`).
