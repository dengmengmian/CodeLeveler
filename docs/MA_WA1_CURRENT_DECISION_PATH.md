# MA-WA1 — 现行委派决策路径（代码审计，基线 b967f9f）

修复前的机制真相，逐条对应 Phase 4 的八个问题。

## 组件

- `crates/leveler-agent/src/sub_agent.rs` — `DelegationDecisionPoint`（one-shot 状态机）、
  `DelegationRoundAction`、`DelegationPrior`、`delegation_decision_request`（offer 文案）、
  `multi_agent_steer_hint`（round-1 协调者契约）。
- `crates/leveler-agent/src/executor/drive.rs` —
  构造（:224，`eligible = allow_delegation && depth==0`，prior 从 ProgressLedger 播种）；
  `note_plan_registered`（:2067，每次 plan 更新都调用，传未完成 step 文本）；
  `note_mutation`（:2119/:2200，任何成功改文件的调用）；
  `note_worker_admitted`（:2451）；`end_round` 消费（:2700，事件 + 注入消息）。
- `crates/leveler-lifecycle/src/progress.rs` — `ProgressLedger.delegation_decision_offered
  / delegation_kept_recorded / delegation_delegated_recorded`（跨 window 持久）。
- EventLog：`delegation_stage` 事件（action=offered/kept/delegated + detail）。

## 八问八答

1. **什么触发 offered？** 两条：plan 注册且未完成 step ≥2（trigger=plan，同轮注入）；
   或第二个发生 mutation 的轮（trigger=mutation_fallback，无 step 泛化文案）。
2. **触发时有什么数据？** 只有模型自己的 open step 文本（≤8 条枚举）。无 scope、
   无依赖状态、无文件事实。
3. **知道具体工作项吗？** 只到 step 文本粒度；捆绑步不拆（取证事实 1）。
4. **知道 scope 吗？** 不知道。委派时由模型从零构造 `files`。
5. **知道依赖状态吗？** 不知道。offer 恰在依赖最稠密的 plan 注册轮（取证事实 2）。
6. **知道 integration 责任吗？** 文案有一句 "inspect and integrate its result"，无结构化。
7. **后来变具体的工作项能再触发吗？** **不能**。`offered` 置位后
   `note_plan_registered` 的后续输入全被丢弃；`kept` 由 offer 可见后的首次 mutation
   永久落账；文案明说 "you will not be asked again"。这是主根因。
8. **今天靠什么防 nag？** one-shot 本身（外加 ProgressLedger prior 保证跨 window 不重问）。

## 判据语义（保留项）

- KEEP 判定：offer 可见（offered 次轮起）后第一个 mutation 轮 → durable `kept`。
- delegated 在 kept 之后仍可落账（`delegation_after_keep_is_still_recorded_as_a_fact`
  测试 + EB-4 生产证据）——runtime 不阻止延迟委派，只是不再提醒。
- 事实顺序：RecordDelegated/RecordKept 先于 Offer，offer 不会出现在它询问的决策之后。
- 子 agent（depth>0）与 allow_delegation=false 完全静默。
