# Evaluations

Two trees, one rule: **eval observes the product; it does not special-case it.**

| tree | job |
| --- | --- |
| [`evals/`](../../evals/README.md) + `crates/leveler-eval` | Capability: did the agent produce a correct tree? Independent `expect`. |
| [`eval/`](../../eval/README.md) | Behaviour: delegation decision, timing, safety counters, long-task EventLog. |

Gate map and the adoption micro protocol: [`E004-multi-agent-adoption.md`](E004-multi-agent-adoption.md).

Real-usage rounds run the product against third-party repositories. Batches #1
(`R001–R010`) and #2 are closed and live in the dogfood-control repo; what they
concluded is in [`BETA_CLOSURE_PROGRAM.md`](../BETA_CLOSURE_PROGRAM.md). The
next round runs against the **published** `v0.2.0-beta.1` binary rather than a
`main` checkout: [`REAL_USAGE_BETA_001.md`](REAL_USAGE_BETA_001.md).

MA-WA1 closing record — what was tested, what was eliminated, and what that
means for Beta: [`MA-WA1-FINAL.md`](MA-WA1-FINAL.md).

Chinese: [`README.zh-CN.md`](README.zh-CN.md).
