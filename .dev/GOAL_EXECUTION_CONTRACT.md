# Goal 执行与完成契约

本文描述修复后的产品契约，以及它复用了哪些现有机制。目标不是新增一套状态机，而是让 `/goal` 在任何客户端都进入现有的 Goal 执行路径，并让“完成”有可机械检查的依据。

## 1. Chat 与 Goal

| | Chat | Goal |
|---|---|---|
| 默认 | 交互式新会话默认 Chat（`CollaborationMode::interactive_session()`） | 显式 `/goal <任务>`、`--collaboration goal`、headless 默认值 |
| 结束 | 模型给出 final 即 `Answered`，回合结束 | 必须 `update_goal(complete\|blocked)`；静默不算完成 |
| 阶段总结 | 合法结束 | 只是阶段总结；closeout 继续同一目标 |
| 持久化 | `sessions.collaboration='chat'` | `sessions.collaboration='goal'`，恢复/重启后仍生效 |
| 工具面 | 无 `update_goal` | 有 `update_goal` |

Chat 的行为没有改变：`DF-9` 用一个普通回答验证只发 1 次模型请求就结束。

## 2. `/goal` 的唯一入口

解析函数只有一处：`leveler-client-protocol::parse_goal_command`。

| 输入 | 行为 |
|---|---|
| `/goal <任务>` | 写 `sessions.collaboration='goal'`，以该任务启动 Goal 回合 |
| 运行中 `/goal <任务>` | 先写轴，再把任务作为 Goal 声明交给 **正在运行** 的 executor；下一轮起启用 Goal 契约 |
| `/goal clear` / `cancel` / `stop` | 写回 `chat`，清掉在途 Goal 声明 |
| `/goal status` | 只报告当前轴，不启动回合 |
| `/goal` | 返回用法错误，不产生空目标 |

路径覆盖：

- TUI：`/goal` → `ClientCommand::RunGoal`（原路径保留）。
- 任意文本客户端：`SubmitMessage` 中以 `/goal` 开头的文本由 Runtime 解析（新增）。Desktop 现在把 `/goal <任务>` 作为文本发送；它不再需要自己的 Goal 命令。
- 运行中：`SteerCurrentTurn` 中的 `/goal` 由 Runtime 解析（新增）。TUI 忙碌时正是发送 `SteerCurrentTurn { content: "/goal …" }`；Desktop/旧客户端同样落在这里。
- 大段粘贴：`submit.rs` 用 canonical 文本识别命令，`[Pasted: N lines]` 占位符不会再让粘贴的 `/goal` 退化成 Chat（新增回归测试）。
- `/collab chat`（`SetProductAxes`）：除了写轴，还会清掉在途 Goal 声明，避免一个已关闭的 Goal 在后续 Chat 回合里“复活”。

没有第二份轴状态：持久化只写 `sessions.collaboration`；客户端只镜像显示。

## 3. 运行中的 Goal 升级

Goal 不再要求“先空闲、再输入”。运行中的 `/goal` 走现有 `SteeringSource`，不新开回合：

```text
SteerCurrentTurn("/goal X")
  → persist sessions.collaboration = goal
  → GoalSteer { requested = true, pending = [X] }
  → 当前 executor 在下一轮 on_round_start:
       take_goal_declaration() = X
       request_tool_definitions() 重新生成（出现 update_goal）
       X 作为普通 user_input 追加并持久化
  → closeout 从该轮起使用 Goal 语义
```

实现位置：

- `crates/leveler-app/src/interactive.rs`：`steer_into_goal`、`GoalSteer`、`SessionSteering::{goal_requested,take_goal_declaration}`。
- `crates/leveler-agent/src/executor.rs`：`Executor::goal_mode()` = 静态 policy 或 steering 的 Goal 请求。
- `crates/leveler-agent/src/executor/drive.rs`：round 起点注入声明并重建工具面。
- System prompt 每轮重新生成，因此 Goal 契约从升级后的下一轮出现，不需要额外模型调用。

## 4. 完成判定

`update_goal` 的 schema 新增可选 `verification`：

```json
{
  "status": "complete",
  "summary": "…",
  "verification": {
    "status": "passed | not_applicable",
    "detail": "passed: 一行一条实际运行过的命令；not_applicable: 为什么没有可执行检查"
  }
}
```

判定是**机械事实检查**，不是第二个模型判决：

| 情况 | 结果 |
|---|---|
| `passed` 引用的命令没跑过 | 拒绝完成，并指出哪条没跑 |
| 最近一次引用命令失败 | 拒绝完成 |
| 引用命令成功，但早于最后一次文件修改 | 拒绝完成，要求重跑 |
| 任务中最近一次“测试/构建类”命令失败且之后没有成功 | 拒绝完成；`not_applicable` 也不能覆盖 |
| 改过文件、没有任何验证 | 接受完成，但 `TaskFinished.warnings` 记录“未做可执行验证”，UI 显示带警告的完成 |
| 改过文件、有新鲜的成功验证 | 正常完成 |
| `not_applicable` + 原因 | 接受，并持久化警告 |
| 没有改文件 | 正常完成（只读目标没有可验证产物） |

“测试/构建类”只用于**失败硬证据**的识别（`is_verification_command`），覆盖 go/cargo/npm/pnpm/yarn/make/pytest/python -m unittest 等；无法识别的命令仍可作为 `passed` 引用，因为引用检查只要求它真的运行并成功。

结果落点复用现有通道：

- `TaskOutcome::Completed / Blocked / BudgetLimited / Failed / Interrupted / Cancelled`（没有新增枚举）。
- `StopReason::{Completed, Answered, Blocked, Stalled, BudgetExhausted, TurnLimitReached, Incomplete}`。
- `TaskFinished.warnings`：未验证完成、独立评审问题等都在这里，不伪装成 Verified。
- `GoalIntercepted` 事件：记录 `verification_refused` / `invalid_verification`，可审计、可统计。

### 与 2026-09-08 “Runtime 权责边界”的关系

那次减法删除的是 **LLM 判 LLM**（Completion Contract、judge、auto-repair、按文件名触发 reviewer）。本次没有恢复任何一条：

- 没有语义模型调用；
- 没有关键词解析用户目标；
- 不判断改动“好不好”；
- 只检查模型自己声明的命令是否真的执行、是否成功、是否晚于最后一次修改。

新增的 +215 tokens 工具描述（初始 Goal schema 6682 → 6897）就是这套机械契约的全部固定成本；详情见 `PERFORMANCE.md`。

## 5. 有界执行

| 边界 | 行为 |
|---|---|
| Goal 回合静默（无 `update_goal`） | closeout 继续；连续无进展达到 `ProgressCaps::no_progress_rounds = 2` 后 `Stalled` |
| `Stalled` | `TaskOutcome::Interrupted`，目标仍未结算，可恢复；不是成功 |
| 预算/回合上限 | `BudgetLimited`，保留未完成状态与真实原因 |
| 用户取消 | `Cancelled`（逻辑任务终止，不可被“继续”悄悄重开） |
| 用户拒绝权限 | 不再追买模型轮次；Goal 以 `Blocked` 或如实停止 |
| 重复 Final | 除非 `update_goal`，否则每次静默只买一轮有界续跑；不做无限注入 |

不通过“反复追加用户消息”实现持续执行；续跑轮次是既有 closeout 机制，不是新的隐藏循环。

## 6. Compaction 与 Resume

- 轴：`sessions.collaboration`，进程重启后仍为 `goal`。
- 目标：`goals.objective` 由首回合打开；恢复时按 lineage 绑定同一个 goal，不按文本猜。
- 记录：transcript 从 `session_messages` 重放；Compaction 只折叠模型上下文，不改写会话首条用户消息。
- 验证：`DF-4` 用真实 `/compact` + 中断/重启证明目标文本与 Goal 工具面仍在；`DF-5` 用真实 Esc 中断 + 重启证明绑定同一 `goals` 行、目标只出现一次（另有 host-pinned objective 块）。
- 运行中 `/goal` 的声明在下一次 round 以普通用户消息持久化；若进程在声明尚未注入时退出，轴已持久化为 Goal，声明本身只在内存中（见剩余风险）。

## 7. 明确不做的事

- 不把模型自然语言中的「完成」「本轮」「下一步」当作完成/未完成判据。
- 不要求用户再次输入「继续」才推进 Goal（阶段结束由 closeout 继续）。
- 不把 Runtime 内部指令伪装成用户消息；Goal 升级注入的是用户自己的文本，`update_goal` 提示来自工具契约。
- 不改变 Full / Auto 权限契约。
- 不把 `update_goal(complete)` 变成不可停止的无限循环：预算、无进展、取消、阻塞都优先。

## 8. 残余限制

1. **语义验收仍属模型责任。** 运行时能检查“命令跑过且成功”，不能检查“这条命令是否真的覆盖了用户目标”。模型可以引用一条真实通过的无关命令；这是机械检查的上限，已在完成契约中明确。
2. **子 Agent 的验证不自动计入父 Agent。** 父 Agent 必须自己运行验收命令，或声明 `not_applicable`。这是保守选择，避免父级把子级报告当成自己的执行事实。
3. **Blocked 后的「继续」是新 Goal。** Blocked 是终态；用户说「继续」会以该续接文本作为新目标。若需要保留旧目标文本，应使用 Resume（中断/暂停），而不是 Blocked。
4. **运行中 `/goal` 的声明窗口。** 轴先持久化；声明文本在下一轮才落库。若进程恰在此窗口崩溃，轴保留、声明文本丢失，需要用户再描述一次目标。
5. **验证事实按回合记录。** `verification_runs` 属于当前 Drive；恢复/续接后的新回合不会自动继承上一回合的命令记录。因此恢复后的 `complete` 需要重新运行验收命令，或显式 `not_applicable`。这会产生一次额外验证回合，但不会把未验证状态伪装成通过；把验证事实提升为跨回合的持久账本需要新的耐久真相源，本轮不做。
