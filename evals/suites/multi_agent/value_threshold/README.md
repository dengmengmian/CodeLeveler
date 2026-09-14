# MA4-B workload ladder

Five public Go cases in `evals/cases/multi_agent_threshold/`, registered in
`catalog.json` before any run. They ask one question: at what workload size
and parallel structure does multi-agent start to beat the same build with
delegation turned off?

| Case | Bucket | Independent work units | Expected parallelism |
|---|---|---|---|
| `mat-s0-word-count` | S0 tiny | 1 function | none (a spawn is unnecessary) |
| `mat-s1-port-setting` | S1 small | 1 feature over 2 files + README | none |
| `mat-s2-three-parsers` | S2 medium | 3 packages: semver, ini, cron | medium |
| `mat-s3-four-engines` | S3 large | 4 packages: expr, jsonpath, textdiff, toposort | high |
| `mat-s4-eight-engines` | S4 parallel-native | 8 packages: the S3 four + ratewindow, glob, mdtable, lruttl | high |

Rules:

- Task text never mentions sub-agents.
- Each package's doc comments state the whole contract; a hidden test suite
  (written into the tree by `expect`) decides correctness, and `expect`
  refuses a run that modified the visible tests.
- Every case fails `expect` at its starting state and passes with a
  reference solution; the reference solutions are not in the repository.
- A case that has runs is never edited. If a pilot shows a case is too small
  for its bucket, a replacement gets a new id and the old case stays,
  marked retired in the catalog.

Run with `evals/scripts/multi_agent_ab.py run --suite value_threshold`.
Per-run coordination metrics come from `evals/lib/coordination.py`.
