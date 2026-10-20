# 长任务提前结束：真实会话根因

- 会话：`39f7e922-bf4d-4346-b5d2-8c09eceb1fd3`
- 项目：`/Users/mengmian/Develop/app/dengmengmian/devorder`
- 原始记录：`~/.leveler/state/projects/-Users-mengmian-Develop-app-dengmengmian-devorder-87163aab4fea4f77/sessions.db`
- 对话导出：`devorder/leveler-chat-20261021-000850.md`
- 运行版本：`leveler 1.0.20 (8537610c85c9)`
- 结论：**主要根因是 `/goal` 没有进入运行时，会话全程是 Chat；不是模型上下文耗尽，也不是预算或超时。**

## 结论摘要

| 问题 | 事实 | 判定 |
|---|---|---|
| `/goal` 是否切换模式 | 否。`sessions.collaboration = 'chat'`；该会话 `goals` 表 0 行 | 已证实 |
| `/goal` 是否被解析 | 否。`/goal ...` 原文作为普通用户消息持久化在 turn 6 | 已证实 |
| 每次阶段结束的 StopReason | 12 个 turn 全部 `answered` | 已证实 |
| Runtime 是否知道目标未完成 | 不知道。没有 Goal、没有验收条件、没有目标状态 | 已证实 |
| 是否发生 Compaction | 否，0 个 `compacted` 事件 | 已证实 |
| 是否 Resume | 否。用户输入的是「继续」，走的是续接/新回合，不是进程重启后的 resume | 已证实 |
| 模型是否过早 Final | 12/12 个回合都以模型 final 结束；在 Chat 契约下这是合法终止，但不是用户要求的 Goal 契约 | 已证实 |
| 工具结果是否损坏 | 是。会话级脱敏把项目名 `devorder` 注册成密文，持久化时把 Go import 路径改写成 `[REDACTED]`，下一回合从存储重放给模型 | 已证实（代码 + 记录 + 回归测试） |
| Patch 是否谎报成功 | 工具允许 `-X/+X` 同文补丁，返回 `Applied patch` 而文件未变；模型据此认为补丁生效 | 已证实（代码 + 会话文本） |

## 会话规模

| 指标 | 实际值 |
|---|---:|
| 时间范围 | 2026-10-20 15:39:09 → 16:16:35 UTC（37m26s） |
| Turn 数 | 12（全部 `kind = chat`） |
| 模型请求 | 226（round 189：177 次 tool_calls + 12 次 stop；away_summary 12；memory_extraction 12；prompt_suggestion 13） |
| 输入 tokens | 36,333,683（缓存 34,224,768，94.2%） |
| 输出 tokens | 204,317 |
| Tool calls | 256 |
| Compaction | 0 |
| Goal 行 | 0 |

每回合（按 turn 时间窗口统计）：

| Turn | 模型轮次 | 输入 tokens | 输出 tokens | Tool calls | 结束 |
|---:|---:|---:|---:|---:|---|
| 1 | 4 | 40,458 | 641 | 6 | answered |
| 2 | 2 | 21,546 | 119 | 1 | answered |
| 3 | 15 | 425,934 | 10,741 | 32 | answered |
| 4 | 1 | 50,732 | 1,020 | 0 | answered |
| 5 | 43 | 4,533,913 | 44,769 | 67 | answered |
| 6 | 27 | 4,456,913 | 24,823 | 37 | answered |
| 7 | 15 | 3,022,819 | 16,008 | 18 | answered |
| 8 | 13 | 2,876,948 | 15,869 | 13 | answered |
| 9 | 14 | 3,392,339 | 18,912 | 14 | answered |
| 10 | 21 | 6,097,454 | 34,812 | 31 | answered |
| 11 | 13 | 4,121,762 | 9,264 | 14 | answered |
| 12 | 21 | 7,248,978 | 23,980 | 23 | answered |

## 关键时间线

| 时间（UTC） | 事件 | 证据 |
|---|---|---|
| 15:39:09 | 会话创建 | `sessions.created_at`，`collaboration='chat'` |
| 15:41–15:49 | Turn 1–5：正常 Chat 回合 | 每个 turn 的 `task_finished.stop='answered'` |
| 15:54:21 | Turn 6 开始：「继续做订单详情和财务登记/更正这两个模块」 | `turns.ordinal=6`，`kind=chat` |
| 15:55:07.472897 | 一次 tool result 落库 | `session_messages.ordinal=143` |
| **15:55:07.476718** | 用户输入 `/goal 反正我的目标是最后做完。打通。测试MCP能过。且保持跟页面操作一样。` | `session_messages.ordinal=144`，与上一行相差 **3.8ms**，位于同一 turn 内 |
| 15:55:23 | 模型继续同一回合，把 `/goal ...` 当作补充说明 | `ordinal=145` 起为 assistant 继续 |
| 15:57:21 | Turn 6 以 `answered` 结束 | `task_finished` |
| 15:58:07 | Turn 7「继续，按你说的顺序来。直到完成」 | payload 含 `continuation_root_turn_id = a2a1cea8…`（turn 6） |
| 16:00:12 / 16:02:02 | Turn 8「继续。全部做完……」/ Turn 9「继续。」 | 同上，继续复用 turn 6 的 objective |
| 16:04:48 | Turn 10「把剩下的全部完成。中间不在断开了」 | **没有** `continuation_root_turn_id`，objective 重置为这句话 |
| 16:10:41 / 16:12:23 | Turn 11「那就配置上」/ Turn 12「先这样吧。你全面验证下看有没有问题」 | 新 objective |
| 16:16:35 | Turn 12 `answered`，会话 `status=completed` | `sessions.outcome='completed'` |

`/goal` 落库时的上下文：

```text
ordinal=143 role=tool        15:55:07.472897
ordinal=144 role=user       15:55:07.476718
  {"text":"/goal 反正我的目标是最后做完。打通。测试MCP能过。且保持跟页面操作一样。","type":"text"}
ordinal=145 role=assistant  15:55:23.863405
```

这不是一个独立 turn，而是在 **turn 6 运行中** 到达的 steer/queued input。运行时 `SteerCurrentTurn` 当时只把字符串放进 steering 队列；下一条模型请求把它当作用户补充说明，原样注入。

## Phase 1 十二问

1. **是否真正切换到 Goal？** 否。会话行 `collaboration='chat'`，目标表为空。
2. **Slash Command 是否正确解析和持久化？** 否。原文含 `/goal` 前缀进入 `session_messages`；解析过的命令不会以原文发给模型。
3. **Collaboration Mode 是否变化？** 否。12 个 turn 全部 `kind='chat'`。
4. **每次阶段性结束模型返回了什么？** 阶段总结与「本轮先到这里」「下一步建议」；最后 turn 10 的报告在「全部做完了」的同时列出 NOT verified 与剩余缺口。导出行 468、620、757、790 可对照。
5. **Runtime 收到什么 StopReason？** 12 次全部 `answered`。
6. **模型 Final 还是 Runtime 结束？** 模型输出 final；Runtime 在 Chat 契约下把 final 接受为 `Answered`。没有预算、超时或取消触发。
7. **是否存在未完成 Task / Plan？** 没有运行时 Goal/Plan 可判定；模型自己的文字列出了剩余项。turn 7–9 的 objective anchor 是「继续做订单详情和财务登记/更正这两个模块」，不是用户的总体目标；turn 10 又重置为「把剩下的全部完成……」。
8. **Runtime 是否知道最终任务未完成？** 不知道。它只记录了 `answered`；没有 Goal、验收条件或未完成状态可供比较。
9. **Compaction 是否发生/丢失目标？** 本会话没有 Compaction（0 事件）。目标丢失发生在入口，不是 Compaction。
10. **Resume 是否恢复 Goal？** 本会话没有进程重启后的 Resume。用户输入「继续」时，turn 7–9 通过关键词命中续接并复用 turn 6 的 objective；turn 10 起不再续接。
11. **是否存在模型过早 Final 被直接接受？** 是，12/12 次。Chat 下这是设计行为；但用户显式要求的是 Goal。
12. **Completion / Closeout / Delivery Convergence 是否错误？** Goal closeout 从未运行（不是 Goal 模式）。Chat 没有 Goal 完成判定，因此不存在「Goal 被错误标记完成」——存在的是「从未进入 Goal」。

## 根因分类

### 已证实

| 类别 | 根因 | 证据 |
|---|---|---|
| Slash Command / 客户端入口 | `/goal` 在 TUI 忙碌时被当作 steer 原文发送；Desktop 不解析 `/goal`（命令表标记未接入，bridge 拒绝 `run_goal`）；Runtime 的 `SteerCurrentTurn`/`SubmitMessage` 当时只认 `ClientCommand::RunGoal` | 会话记录 + 1.0.20 源码 `submit.rs` busy 分支 + `desktop_bridge.rs::validate_command` + `renderer.mjs` 命令表 |
| Execution Lifecycle | 会话是 Chat，普通 final 合法结束；没有 Goal/验收条件，后续「继续」只产生新 turn | `task_finished.stop='answered'` ×12；`turns.kind='chat'` ×12 |
| Goal / Objective 锚点 | turn 7–9 复用 turn 6 的小目标；turn 10 重置为新 objective；用户的总体目标从未成为 objective | `turns.payload.objective` |
| Tool Result 脱敏 | `config.dev.yaml` 的 `password=devorder` 让会话注册 `devorder` 为密文；持久化按精确值替换，改写了 `github.com/dengmengmian/devorder/...` 等 import 路径；下一 turn 从 `session_messages` 重放给模型 | 配置第 40 行；`session_messages.ordinal=166/172/188/190`；编译错误 `ordinal=234/258`；模型自述 `ordinal=235/259` |
| Patch 语义 | `*** Update File` 的 `-X/+X` 同文补丁被接受，汇总写 `Applied patch`，文件字节未变；没有写入后回读 | `apply_patch.rs` plan/commit 路径；模型在 `ordinal=263` 之后改为整文件重写 |

### 模型行为（贡献项，不是唯一根因）

- 多次以「这一轮已经很长了」「本轮先到这里」收束回合，符合 Chat 契约，但不是用户要求的持续执行。
- 最终报告存在「全部做完了」与「NOT verified / 仍有缺口」并存；这是模型对完成度的自我表述问题。运行时在 Chat 下没有、也无法据此判定。
- 在被脱敏上下文污染后，模型反复用错误路径修补，产生额外 3–4 个回合与多次编译失败；这放大了耗时，但不是提前结束的起始原因。

### 合理怀疑 / 证据不足

- 会话记录经持久化脱敏后，无法区分模型当时写进文件的是字面 `[REDACTED]` 还是另一个已注册值（两者都会被存成 `[REDACTED]`）。编译器的 `invalid import path` 证明写进去的不是合法模块路径；回归测试证明该机制可稳定复现。
- 本会话没有 Compaction 与 Resume 记录，因此不能从这次会话判断长任务在压缩/重启后的表现；该项由新的 Dogfood DF-4/DF-5 覆盖。
- 会话没有记录桌面端的具体构建；`/goal` 原文落库与 1.0.20 的 TUI busy 路径完全吻合，且 Desktop 当前版本同样不具备可用入口。入口缺陷对两种客户端都成立。

## 修复后的验证

- 回归测试：`collaboration_entry.rs` 覆盖 `SubmitMessage("/goal …")`、运行中 `SteerCurrentTurn("/goal …")` 升级同一回合；`reducer.rs` 覆盖忙碌 `/goal` 原文 steer 与大段粘贴 `/goal` 不退化为 Chat。
- Dogfood：`DF-1`、`DF-5` 在真实 PTY 上验证轴持久化与重启续接；`DF-7` 验证路径不再被改写；`DF-8` 验证空补丁如实报告。
- 详见 `DOGFOOD_REPORT.md` 与 dogfood 仓 `.dev/evidence/goal_contracts_20261021.json`。
