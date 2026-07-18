# Generated evaluation results

This directory is reserved for local `leveler eval --json-out` output. JSON and
JSONL results are ignored by Git because they may contain model identifiers,
prompts, repository paths, timestamps, and diagnostic excerpts.

Run an evaluation from the repository root, for example:

```sh
leveler eval run \
  --cases evals/smoke \
  --json-out evals/baselines/local-smoke.json
```

Inspect and redact a result before sharing it outside your machine.
