# Goal 长任务资源治理（R6-A2）

日期：2026-11-28
结论：**PARTIAL**。已有结构化计量与硬限制链路可用；本轮补上全局预算回退与预算终态文案。
仍缺 Desktop 预算编辑器、软阈值提醒、持久化 goal 级总量与结构化终态字段。

## 1. 现状（代码事实）

| 维度 | 现状 |
| --- | --- |
| 每次模型调用 | `model_requests` 行持久化 input/output/cached/reasoning tokens、cost micros、latency、retry、kind、budget_scope |
| 每轮 | `TurnUsage` 由 durable rows 聚合；`ProgressLedger` 保存 model steps、commands、duration、modified files、tokens、cost |
| 重启 | tokens/cost 从 `model_requests` 按 `budget_scope` 重建；`reconcile_model_spend` 避免重复累计；同 scope 保留计数，换 scope 重置 |
| 硬限制 | token / cost / duration / commands / modified files；在模型调用准入与工具边界检查；`Some(0)` 是硬耗尽，不是无限 |
| 默认 | 项目 `.leveler/config.yaml` 三个 cap 默认 `None`；交互/daemon 的 model-step ceiling 默认 `None`；无全局默认硬预算 |
| 失控保护 | 2 轮全拒绝/静默 Goal 轮、2 个连续无工作 yield 窗口、transient provider 重试上限 3；不是硬编码低预算 |
| 暂停/恢复 | 预算停止 → `TaskOutcome::BudgetLimited`，Goal 不 settle，可 `继续`/恢复；恢复后仍受同一 scope 的 cap |
| 用户可见 | Desktop 显示 live tokens（输入/输出/cache，明细含 reasoning）；TUI 有 footer 与 `/usage`；无 Desktop cost 显示，无预算编辑器 |

## 2. 本轮改动

1. **全局 `[limits]` 回退**（`GlobalConfig`）：
   - `max_model_tokens`、`max_cost_usd_micros`、`max_duration_seconds`；
   - 项目 `.leveler/config.yaml` 的显式值优先，全局只填补空缺；
   - 两者都未设置 → 仍然没有硬预算，不发明默认上限；
   - 单位与项目配置一致（tokens / micro-USD / seconds）。
2. **预算终态文案**：
   - `TaskReport`/`AgentOutcome` 保留 `budget_exhaustion`（此前 app 映射成 `None`）；
   - live 路径用结构化 `BudgetExhaustion` 生成产品文案；replay 路径解析运行时自己的
     `budget_exhausted dimension=… spent=… cap=…`，两类路径给出相同维度与数值；
   - 维度映射：模型 token / 费用 / 时长 / 命令调用 / 修改文件；
   - 用户不再看到 `budget_exhausted dimension=tokens` 机器串。

## 3. 验证

- `cargo test -p leveler-app a_budget_cutoff --lib`：2/2 PASS（机器串不出现、维度与金额正确）。
- `project_limits_win_and_global_fills_only_omissions` PASS。
- `global_limits_are_optional_and_do_not_invent_a_budget` PASS。
- 既有 kernel/agent budget 测试未改：限额等值触发、零 residual、duration、restart 不重置等由原测试覆盖。

## 4. 缺口（未伪装为完成）

| 缺口 | 影响 | 状态 |
| --- | --- | --- |
| Desktop 预算编辑器 | 用户只能编辑 `config.toml` | NOT IMPLEMENTED |
| 软阈值提醒 | 超过 80% 无 UI 提示；只有 duration 的 finalize 宽限 | NOT IMPLEMENTED |
| Goal 级总量查询 | 需要恢复/回放才能得到完整 goal 总量；无独立 goal usage 协议 | NOT IMPLEMENTED |
| 结构化终态持久化 | `TaskFinished` 只存机器 `reason`，未存 `BudgetExhaustion`；live 有结构，replay 靠解析 | PARTIAL |
| Desktop cost 显示 | 只显示 tokens；`query_usage` 不在 Desktop allow-list | NOT IMPLEMENTED |
| 价格缺失 | 无 pricing 的模型不能启用 cost cap；DeepSeek 配置故意不带价格 | 明确产品约束 |
| “低价值但持续有工具”的失控 | 只有资源 cap/步数/无进展守卫；没有“花费 vs 进展”启发式 | NOT IMPLEMENTED（不伪造） |

## 5. 结论

正常长任务在未设置 cap 时不会被硬停；无进展有既有守卫；用户可通过项目或全局配置设置
硬限制并得到准确的产品级停止文案。R6-A2 的“完整资源治理产品面”尚未达到，发布说明不得
宣称已具备 Desktop 预算管理与提醒。
