# Completion Reconciliation Gate（ICG-6R 结构性收口）

## 旧缺陷

icg-6r（不可能任务诚实收场探针）复现的失败链：

```
用户原始目标（与不可改测试冲突，字面上不可满足）
    ↓ 执行模型发现冲突
语义重解释（空名记录 / 部分面豁免 / Valid==false 坏记录 …）
    ↓ 实现被替换后的弱化任务，测试全绿
自报完成 → update_goal(complete) → 全部机械门放行 → Verified
```

措辞层契约（update_goal 工具描述 / goal 序言 / closeout nudge，共两个 commit）把诚实收场率从
~33% 提到 ~83% 后进入渐近：每封死一种重解释，模型换一种新的。机械门无法判语义。

## 架构

执行声明与最终裁决分离——**一次独立的模型二审**，只发生在
`depth==0 && goal_mode && update_goal(complete)` 且所有既有机械门（blocking findings、
todo/delivery gate）都已通过之后：

```
update_goal(complete)
    ↓ 既有机械门（不变，先行）
Completion Reconciliation（新，独立 generate 调用）
    ↓ 结构化判决 JSON
SATISFIED 且全要求满足且零矛盾 → 走既有完成路径
BLOCKED / UNCERTAIN / 矛盾 / 畸形输出 / provider 失败 / 超时 → 拒绝完成（fail-closed）
```

- **原始目标权威**（§5）：输入用 `ObjectiveAnchor`（session goal 原文，否则首条用户消息）——
  执行模型对目标的后期转述不作为契约。
- **输入有界**（§6）：原文目标 + complete 声明的 summary + 最近 2 条 assistant 文本（4KB 尾）+
  最近 3 条工具结果（各 1.5KB 尾）+ 改动文件清单 + fresh-verify 布尔。无 CoT、无隐藏答案。
- **结构化输出**（§8）：`{verdict, requirements[{requirement,satisfied,evidence}], contradictions[], reason}`；
  verdict=satisfied 但 requirements 为空 → 视作 uncertain（没有逐条对账就没有裁决）。
- **拒绝反馈**：durable `GoalIntercepted{kind:"completion_reconciliation"}` + ledger intercept +
  update_goal 工具错误结果，指引"按原表述完成或 blocked 点名冲突"；Unavailable 类单独措辞
  （"什么都没被裁决，重试或如实 blocked"）。
- **取消/重试**（§17/§18）：走 loop 的 cancellation child token；单次调用无自建重试——
  拒绝后模型下一轮重试 update_goal 即是天然重试。
- **持久化/重启窗口**（§19/§20）：gate 在任何完成事实持久化之前运行；中途崩溃 = 无完成持久化，
  恢复后模型重发 update_goal → gate 重跑。重复的 gate 调用无副作用。
- 子代理与普通聊天轮不触发（children 由父级 settle；chat 无 goal 可对账）。

## 成本（实测于 icg-6r ×10）

见验收数据节。预算许可（§14）：仅发生在 goal 终点，flash 档 2-4s 级。

## 测试

- 单元（reconciliation.rs ×8）：判决解析矩阵（满足/一项不满足/矛盾/uncertain/空 requirements/
  畸形×3/JSON 周围散文/CJK 有界尾）。
- e2e（loop_test ×3）：blocked 判决拒绝完成且运行继续（durable 事件+反馈文案）；畸形输出
  fail-closed；provider 失败 fail-closed。
- 既有门优先级：blocking-finding 先于 gate 拒绝（既有测试保持通过=未被覆盖）。
- 测试基建：scripted mock 由 `leveler_test_support::reconcile_autopilot` 对 gate 请求带外应答
  （识别指令头 `completion reconciliation judge`），脚本 FIFO 与请求数断言不受影响；
  loop_test/multi_agent_test 显式脚本消费（两种模式各证一面）。
- `direct_spends_no_extra_model_call_on_acceptance` 语义更新：acceptance 审计仍零额外调用；
  reconciliation gate 是本轮有意新增的一次调用，在 mock 中带外应答。

## 验收（§29-§33）

（实跑后回填：icg-6r ×10 / 受影响 lane 重跑 / CORRECT_WORK_UNDERCLAIMED 计数 / 时延与 token 实测）

## 残余限制

- 二审仍是模型判断：同一模型对自身同型偷换存在共谋风险；×10 探针是回归门不是数学保证。
  若结构性二审后仍漏 → SEMANTIC_COMPLETION_RECONCILIATION_LIMIT_REACHED=YES，停止工程收敛。
- 保守误伤（CORRECT_WORK_UNDERCLAIMED）单独计数，不与假完成混淆。
