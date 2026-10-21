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

## 6. 生命周期一致性（2026-10-21 收口）

### 6.1 轴与声明是一个持久事实

`/goal <任务>` 的两次写（协作轴、目标文本）已合并为一次数据库写入：
`sessions.collaboration='goal'` 与 `sessions.pending_goal='<任务>'` 在同一个事务里落库
（`SessionEdit::GoalDeclaration`）。任何显式轴变更（`/goal clear`、`SetProductAxes`）
在同一个 `UPDATE` 里把 `pending_goal` 置空。

`pending_goal` 由真正绑定目标的回合消费：

| 路径 | 消费者 | 绑定位置 |
|---|---|---|
| 空闲 `/goal X` | `open_goal_for_turn` + TurnStart | 以声明为目标打开/复用 goal；`start_owned_consuming_declaration` 在 turn WAL 的**同一事务**里 CAS 清空声明 |
| 运行中 `/goal X`（Chat 回合） | 引擎的 `GoalBinder::bind` | 打开/复用 goal + 把 goal_id 写进当前 turn 的 WAL payload，再 CAS 清空声明 |
| 运行中 `/goal X`（已是 Goal 回合） | `GoalBinder::discard_declaration` | 旧目标不变，X 作为本轮指令；持久声明被消费，不会留给后续回合 |
| 纯消息（会话已在 Goal 轴，无新 `/goal`） | `open_goal_for_turn` | 若没有声明，先把 fallback 目标写成声明（CAS-if-absent），再打开/复用 goal；由 TurnStart 原子消费 |

崩溃语义：

- 声明写入后、任何回合绑定前崩溃：轴 + 声明都在，下一个 Goal 回合（包括用户输入
  `继续`）以声明文本为目标；模型收到 host-pinned objective 行，而不是把 `继续` 当目标。
- goal 已打开、turn payload 已绑定后崩溃：`turns.payload.goal_id` 是恢复身份，
  `resume_lineage` 直接绑定同一 goal。
- 绑定与清空之间崩溃：声明仍在；下一次打开按 objective 复用同一个 running goal，幂等。
- goal 已打开、TurnStart 尚未落库：声明在 goal 之前写入，且不会被提前清空，所以下一次 Goal 回合
  （`继续` 或普通消息）以声明为目标复用同一个 running goal；**不会**出现“goal 已 running、objective 却无人能命名”的永久孤儿。
- TurnStart 与声明清空是一个事务：要么 turn 行与消费都生效，要么都不生效。

### 6.2 Continue / Resume / Retry Blocked / New Goal

| 输入 | 持久终态 | 行为 |
|---|---|---|
| `继续` | `interrupted` / `budget_limited` / 可恢复 `failed` | 沿用既有 resume：同一 goal、原目标 + 修正文本 |
| `继续` | `blocked` | **Retry**：reopen 同一个 `goals` 行，原 objective 不变；不新建目标 |
| `继续` | `completed`（声明完成） | 终态；`继续` 不重开 |
| `继续` | `cancelled` | 拒绝，并提示任务已取消 |
| `/goal Y` | 任意 | 显式新目标：替换 pending 声明，打开独立 goal；旧 goal 行保持不变 |
| 运行中 `/goal Y` | 当前回合已是 Goal | Y 是当前目标内的指令；原 objective 不被改写 |

### 6.3 验证事实属于 Goal，不属于窗口

`verification_runs` 现在按 goal 的 turn lineage 从持久事件重建
（`ToolCallStarted` + `ToolCallFinished` + `EvidenceLedgerUpdated`），在 resume 和
retry 时作为种子注入 Drive。因此：

- 上一窗口跑过、成功、且晚于最后一次文件修改的验收命令，可以在新窗口被引用；
- 上一窗口失败且之后没有成功的验收命令，仍然阻止 `complete`，包括 `not_applicable`；
- 作用域是 goal 的 turns，另一个 goal 的命令不能为本 goal 作证。

`DF-11`（真实 DeepSeek）与 `verification_facts_survive_the_next_work_window` /
`a_failed_verification_survives_into_the_retry` 覆盖这三条。

### 6.4 Resume 的 profile 判定

- 中断回合本身就是 Goal：沿用 Goal profile（原有行为）。
- 中断回合是 Chat，但该回合已被 `/goal` 绑定（turn payload 有 goal_id）或有待绑定声明，
  且会话轴仍为 `goal`：以 Goal profile 恢复，并绑定该 goal。
- 其他 Chat 中断：仍是 Chat resume；普通聊天回归不受影响。

### 6.5 升级窗口的预算归属（2026-10-21 Final）

一次运行中的 Chat → Goal 升级会把**同一执行窗口**的两段花费并到一个预算所有者：

| 事实 | 归属 |
|---|---|
| 升级前的 `model_requests` 行（scope = Chat turn/lineage root） | 绑定后迁移到 goal id（`UPDATE ... SET budget_scope`，不复制行） |
| 升级后的 model 调用、子 Agent 调用、闭环 reviewer | 执行器按 `GoalBinder::bound()` 动态取 goal id |
| `ProgressLedger` 的 model steps / duration | 绑定当轮把 ledger 换到 goal scope 并持久化一条 `progress_updated` |
| 恢复窗口 | `reconcile_model_spend` 从 goal scope 的行重建 token/cost；Chat 窗口的 scope 由 resume 幂等迁移 |

新建独立 goal 使用新的 goal id/scope；恢复已有 goal 读取同一 scope。二者不会互相继承 spend。
预算耗尽仍映射为 `BudgetLimited` + goal 保持 owed，不会写成 `Completed`。

谱系规则：如果 Goal 是在一个**被 resume 过的 Chat 窗口**中声明的，该绑定窗口就是 Goal 谱系的根，
而不是更早的 Chat 根；后续 Resume 都以它为准（重复 Resume 已覆盖）。

## 7. Chat 显式委托：`commit_task`（2026-10-21）

### 7.1 问题与决定

普通 Chat 的 Final 是合法终局。真实事故中用户明确要求「全部接入、读写都做、完成后测试」，但会话没有 `/goal`，Runtime 12/12 个回合都以 `answered` 结束。修复不是让 Runtime 去猜用户文本，也不是把每个 Chat 都变成 Goal，而是给 Chat 一个**结构化任务委托入口**：

- Chat 回合的模型工具面新增 `commit_task(objective, acceptance)`。
- 模型只在用户明确委托执行、且工作超过一次短回合时调用；讨论、问答、只读要求不调用。
- 调用成功后走既有 `GoalBinder::bind`，不新建第二套状态机。
- `goals.objective` 写当前 turn 的用户原话（`ObjectiveAnchor`），不是模型传入的 `objective` 字段；模型的声明不能把用户目标缩窄。测试 `a_declared_objective_cannot_narrow_the_users_request` 锁定这条。

### 7.2 生命周期

```text
Chat turn
  model: commit_task(objective, acceptance)
    → GoalBinder::bind
        → goals 行（Running，objective = 用户目标）
        → turns.payload.goal_id（当前 Chat turn）
        → model_requests.budget_scope 迁移到 goal id
    → Executor::goal_mode() 变为 true（binder.bound() 是真实事实）
    → 下一轮请求：
        - 工具面出现 update_goal，commit_task 消失
        - system prompt 出现 GOAL MODE 契约
        - closeout 对阶段 Final 继续，对连续无进展有界停止
        - update_goal(complete) 走既有机械验证门
    → task 完成/blocked/预算/取消 → 既有 TaskOutcome 与 goal settle
Session axis 始终是 chat；任务结束后下一条消息仍是普通 Chat。
```

关键实现位置：

| 事实 | 位置 |
|---|---|
| 工具定义与规则 | `crates/leveler-agent/src/injected_tools.rs`（`commit_task`、`TASK_COMMIT_GUIDANCE`） |
| 工具面 | `crates/leveler-agent/src/executor/drive.rs::request_tool_definitions`：仅顶层、可写、未绑定 goal 的 Chat 回合 |
| 处理与绑定 | `drive.rs` 的 `COMMIT_TASK_TOOL` 分支：校验参数、`binder.bind`、重建工具面、返回契约 |
| Goal 判定 | `crates/leveler-agent/src/executor.rs::goal_mode()`：静态 Goal policy ∨ steering `/goal` ∨ `binder.bound()` |
| Resume | `crates/leveler-agent/src/coding/run.rs::resume_inner`：turn payload 的 `goal_id` 本身就是 Goal 身份，即使会话轴是 chat |
| 取消结算 | `run.rs::latest_bound_goal`：回合在报告前中止时，从 turn WAL 恢复 goal 身份，显式取消仍能 settle |
| TUI | `crates/leveler-tui/src/reducer/runtime_apply.rs`：成功的 `commit_task` 置 `goal_mode_active`/`execution_task_active`；暂停任务保留 badge |
| 重连 | `UiSessionSnapshot.execution_task`：从 Running goal 投影 goal_id/objective/running，客户端恢复 badge 与暂停态 |

### 7.3 为什么不在 Runtime 里做意图分类

- 关键词（「完成」「全部」「继续」）会出现在代码、引用、讨论里，不能作为授权。
- 每个 Chat 都追加一次模型分类调用，会让普通问答的成本和延迟翻倍，违反「普通 Chat 不加无谓请求」。
- 让 Runtime 自行启动长任务，等于用低置信度推断消耗用户预算；本次保留了「模型声明 → Runtime 持久化 → 证据验证」的权责边界。
- 模型声明是结构化工具调用，不是自然语言 Final；Runtime 只校验参数与持久化，不判断语义好坏。

### 7.4 边界

| 场景 | 行为 |
|---|---|
| 普通问答 | 不调用 `commit_task`；1 次请求；无 goal 行 |
| 讨论 / 「先不要改」 | 不调用；不写文件；不建任务 |
| 小任务 | 模型可直接在一个回合内完成；不强制升级 |
| 长任务 | 调用一次；阶段 Final 不结束；自动推进到 `update_goal` |
| 执行中调整 | 用户 steer 进入当前回合；goal objective 不变，进度/budget 不重置 |
| 用户取消窗口（Esc） | `TaskOutcome::Interrupted`，goal 仍 Running，可 `继续` |
| 用户取消逻辑任务（`/goal clear`） | goal settle，后续 `继续` 拒绝 |
| 预算耗尽 | `BudgetLimited`，goal 仍 owed |
| 连续无进展 | `Stalled` → `Interrupted`，有界停止，不冒充完成 |
| 只读 / Plan | 不暴露 `commit_task` |
| 子 Agent | 不暴露 `commit_task` |

### 7.5 残余限制

1. **是否委托仍由模型判断。** Runtime 不解析用户文本；模型可能在该委托时没有调用，或把不该委托的请求当成任务。缓解是工具描述与 Chat 契约 prompt，而不是运行时关键词。
2. **`acceptance` 是声明，不是机械判据。** 完成仍由 `update_goal` 的证据检查决定；acceptance 只告诉模型和用户「要证明什么」。
3. **升级当轮的 compaction checkpoint 缺口仍在**（既有分析：Chat 起步的回合没有 checkpoint port）：Chat 起步的回合没有安装 goal checkpoint port；中途绑定 goal 不会补装。回合结束后的 resume 仍能按 turn payload 的 goal_id 生成/读取 checkpoint。
4. **TUI 重连的 badge 来自 snapshot 的 Running goal。** 如果 goal 已 settle 但会话仍有未完成的其他工作，badge 不显示；这是既有 goal 语义，不是新任务状态。

## 8. 明确不做的事

- 不把模型自然语言中的「完成」「本轮」「下一步」当作完成/未完成判据。
- 不要求用户再次输入「继续」才推进 Goal（阶段结束由 closeout 继续）。
- 不把 Runtime 内部指令伪装成用户消息；Goal 升级注入的是用户自己的文本，`update_goal` 提示来自工具契约。
- 不改变 Full / Auto 权限契约。
- 不把 `update_goal(complete)` 变成不可停止的无限循环：预算、无进展、取消、阻塞都优先。
- 不按关键词、消息长度或模型名称推断执行意图；委托只能由结构化 `commit_task` 声明。
- 不把 `commit_task` 当成完成证明；完成仍由 `update_goal` 的机械验证门决定。
- 不让普通问答、讨论或只读请求自动创建持久 Goal；`commit_task` 只在可写的顶层 Chat 回合暴露。

## 9. 残余限制

1. **语义验收仍属模型责任。** 运行时能检查“命令跑过且成功”，不能检查“这条命令是否真的覆盖了用户目标”。模型可以引用一条真实通过的无关命令；这是机械检查的上限，已在完成契约中明确。
2. **子 Agent 的验证不自动计入父 Agent。** 父 Agent 必须自己运行验收命令，或声明 `not_applicable`。这是保守选择，避免父级把子级报告当成自己的执行事实。
3. **验证事实重建是全事件扫描。** 每次 goal 回合/恢复读取该会话的事件日志并按 turn lineage 过滤；对超长会话是 O(events)。正确性优先，未做增量索引。
4. **Goal 回合内的 `/goal Y` 不改写目标。** 运行中已经绑定 goal 的回合，后续声明只作为指令；要换目标需先 `/goal clear` 再声明（或结束本轮）。这是“阶段总结不得改写用户目标”的另一面。
5. **声明、goal、turn 仍是三次写入，但顺序保证可恢复。** 声明先于 goal 落库，turn 与声明消费同事务；
   每个崩溃窗口都留下“声明 + 至多一个 running goal”，下一次回合按声明复用而不是新开。没有跨事务的
   原子 goal+turn 写入（那需要把 goal 语义塞进 storage 的 turn 事务），但不再存在永久孤立的 running goal。
6. **折叠后的 resume 可能不重放旧工具结果。** 长 goal 的 resume 使用 goal checkpoint 时，旧 tool result 不在请求里（DF-7 因此改为在持久 transcript 与首窗 wire 上验证脱敏）；需要旧细节时由 checkpoint 的 findings/workspace 摘要承担。
7. **升级窗口的预算归属已闭合，但历史行不可追溯。** 绑定发生时，`GoalBinder` 把本回合
   `model_requests.budget_scope` 从 Chat scope 迁移到 goal id（移动，不复制）；执行器此后按
   `bound()` 动态取 scope，progress ledger 同步换 scope；resume 再次幂等迁移（覆盖绑定后、
   迁移前崩溃）。升级前的调用因此计入 Goal 的 token/cost/model-step 上限，重复升级、Resume、
   Retry 不会重置。旧版本已写入的无 scope 历史行不在本次回填范围。
