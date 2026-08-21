# 评测

两条树，一条原则：**评测是观察层，不给产品加 eval 特殊行为。**

| 树 | 职责 |
| --- | --- |
| [`evals/`](../../evals/README.md) + `crates/leveler-eval` | 能力：代码是否做对。独立 `expect`。 |
| [`eval/`](../../eval/README.md) | 行为：委派决策、offer 时机、安全计数、长任务 EventLog。 |

Adoption 微评测协议见 [`E004-multi-agent-adoption.md`](E004-multi-agent-adoption.md)。

## 怎么快速判断 spawn 有没有被推动

只改一个因子，两边都用产品默认 runtime，对比 `batch.json` 的 spawn 率（Wilson 90% + Fisher）。单臂有效 run < 6 记 `insufficient_n`，不下结论。

| 问题 | 对照 | 处理 |
| --- | --- | --- |
| prompt 改动？ | git SHA A，`--arm control` | git SHA B，`--arm control` |
| timing 改动？ | `--arm control` | `--arm timing.after_first_edit` |
| 模型/供应商？ | `--model M1` | `--model M2` |

timing 臂只在隔离 `LEVELER_HOME` 里写已上线的 `agents.offer_timing`，不改默认产品行为。
