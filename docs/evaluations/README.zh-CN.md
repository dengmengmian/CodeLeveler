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

## Multi-Agent 价值评测

Adoption 证明的是「会不会 spawn」。价值评测问的是「spawn 之后编码任务有没有变得更好」。

协议：[`MA-VALUE-001.md`](MA-VALUE-001.md)。对照臂 `--mode single`（隔离 home 里写已上线的 `agents.delegation = false`），处理臂 `--mode multi`（产品默认）。成功指标不是 spawn 率。

Child Profile 能力契约（Explorer / Reviewer / Worker；评测按 profile 归因）：[`../design/CHILD_PROFILE.md`](../design/CHILD_PROFILE.md)。

独立 Reviewer 价值（自检 vs harness Reviewer；发现数量不是成功指标）：[`MA-VALUE-REVIEWER-PILOT.md`](MA-VALUE-REVIEWER-PILOT.md)。
