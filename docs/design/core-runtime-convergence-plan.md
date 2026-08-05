# CodeLeveler 核心收敛改造计划

> 状态：架构方向已确认，代码改造尚未完成。  
> 目标：把 CodeLeveler 收敛成职责清楚、行为可预测、可恢复、可长期稳定运行且适合开源扩展的 Agent 核心，同时不牺牲工具执行、安全边界、会话恢复、复杂任务与模型兼容能力。

## 一、成功定义

精简不以 crate 数量或删除行数衡量。完成时应同时满足：

1. 一种状态只有一个权威写入者，不存在 engine 与 agent 的重复续跑或完成状态机。
2. 所有工具副作用都经过同一个 ToolHost 协议，并与可靠持久化顺序绑定。
3. 任意进程崩溃点之后，系统都能明确恢复、停止或请求人工裁决，不会盲目重放副作用。
4. direct loop 只负责模型—工具—结果循环；复杂任务能力通过 policy / workflow 组合。
5. 现有工具、审批、沙箱、取消、goal、验证、子 Agent、resume 与 provider 能力有迁移期和回归保护。
6. 新 provider、工具、工作流或 NPC 不需要复制核心循环、权限系统或会话恢复逻辑。
7. 公开接口、数据库与规范事件有版本、迁移和兼容策略，适合外部贡献者依赖。
8. TUI、Web 和手机端始终接入同一个 runtime；任一端断开、重连或接力不会改变任务事实。

## 二、目标边界

| 层 | 拥有 | 对外契约 | 禁止 |
| --- | --- | --- | --- |
| Engine | task/turn 生命周期、监督、恢复、预算硬上限、明确终止 | 接收任务，发布规范事件，返回类型化停止原因 | 实现工具；重复 Agent 内部策略；根据 UI 连接决定任务存活 |
| Agent Loop | 单次模型请求、工具调用批次、结果回灌、上下文推进 | 输入上下文与可用工具，输出下一动作或终止信号 | 直接访问文件/进程；持久化策略；自动扩大预算；内置产品工作流 |
| ToolHost | schema、风险、审批、路径、可靠起止记录、执行、取消 | 一个不可绕过的 `call` 边界及明确调用结果 | 对话编排；任务完成判断；隐藏副作用失败 |
| Storage | 消息、规范事件、快照水位、迁移、查询投影 | 追加有序事实，按水位恢复上下文 | 推断 Agent 意图；执行策略；修补未知事件语义 |
| Model / Provider | 中立模型类型、流式归一化、线协议适配 | 稳定的模型请求/事件/工具调用语义 | 会话、权限、工作流或工具执行状态 |
| Client Protocol / Transport | TUI、Web、手机端共享的命令、规范事件、snapshot/resync、能力协商与认证 | 可版本化的多端运行时协议 | Agent 决策；工具执行；按客户端复制任务状态机 |
| Policy / Workflow | 规划、证据、阶段检查、修复、重试、委派策略 | 基于核心事件驱动任务，不拥有安全边界 | 绕过 ToolHost；创造第二套持久化或终止语义 |
| NPC Runtime（未来） | 身份、长期记忆、唤醒、收件箱、世界状态、角色策略 | 在同一个 Engine 上调度工作 | 复制 direct loop；绕过权限与恢复协议 |

## 三、实施原则

- 每个阶段只解决一种边界问题，可独立合并、验证和回退。
- 先写最小失败测试并确认失败原因，再写最小实现；阶段结束运行仓库规定的完整检查。
- 先增加新路径并双读或对照，再迁移默认路径，最后删除旧路径。
- 结构调整和产品行为变化分开提交。无法证明行为等价的改动必须单独评审。
- 不为了减少 crate 数量合并职责；crate 合并只在依赖边界已经稳定且确实没有独立价值时讨论。
- 每个阶段更新本文的状态、实际差异、迁移影响和验收证据。

## 四、分阶段改造

| 阶段 | 主题 | 当前状态 |
| --- | --- | --- |
| 0 | 冻结基线与可观测语义 | 完成（验收证据：[`phase0-baseline.md`](phase0-baseline.md)） |
| 1 | 可靠的工具副作用屏障 | 完成（见阶段 1 状态注记） |
| 2 | 唯一 ToolHost 调用边界 | 完成（见阶段 2 状态注记） |
| 3 | 统一会话恢复与上下文装载 | 完成（见阶段 3 状态注记） |
| 4 | 统一生命周期与 goal 续跑所有权 | 完成（见阶段 4 状态注记；续跑所有权转移记录为阶段 5 前置） |
| 5 | 产品策略移出 direct loop | 完成（开关化+分类；协议化迁移与旧策略删除待 eval 数据，见阶段 5 状态注记） |
| 6 | 收紧模型与 provider 边界 | 完成（见阶段 6 状态注记） |
| 7 | 稳定性与开源发布门槛 | 进行中（决策/文档/故障测试盘点已完成；24h soak 与真机验收为环境受限项，见阶段 7 状态注记） |

状态只能在对应验收证据已链接到本文后改为“完成”；代码已经存在但尚未经过本阶段
验收时，应标记为“进行中”，不能按完成计算。

### 阶段 0：冻结基线与可观测语义

**目的：** 在改变架构前锁住当前能力和关键事件顺序。

> **状态：完成。** 审计结论、状态所有权表、能力矩阵、已证实缺陷（含
> `ToolCallStarted` 崩溃窗口的确定性复现测试）与未验证项见
> [`phase0-baseline.md`](phase0-baseline.md)。新增基线测试：
> `crates/leveler-engine/tests/phase0_baseline_test.rs`。

工作：

- 盘点 direct、goal、resume、验证、审批、取消、子 Agent 与各 provider 的真实行为。
- 盘点 TUI、Web、手机端的命令、事件、snapshot/resync、审批、取消和认证行为。
- 建立复杂任务回归集，记录成功条件而非固定模型措辞。
- 为停止原因、tool call id、session/turn id 和恢复位置建立统一可观测字段。
- 为关键崩溃点准备可控故障注入，不在生产代码使用 Mock。

验收：

- 现有能力矩阵有自动化测试或明确的人工验收说明。
- 同一个任务可以从事件中还原工具调用顺序、终止原因和恢复位置。
- 后续每个阶段都能与此基线做行为对照。

### 阶段 1：建立可靠的工具副作用屏障

**目的：** 消除“工具已经执行，但开始事件尚未可靠落盘”的崩溃窗口。

> **状态：完成。** 实现为 `EventBarrier` flush 屏障（`leveler-agent` 定义、
> `leveler-engine` 实现）：flush 标记与事件走同一条有序 pump 队列，因此
> 屏障永远不会与它等待的事件竞态（直写 EventLog 的替代方案会让
> `ApprovalRequested` 与 `ToolCallStarted` 在持久化顺序上乱序，破坏
> dangling 归因，已否决）。两个等待点：宣布 `ToolCallStarted` 之后、
> authorize 之前（pre-tool hooks 也是外部副作用）；authorize 之后、
> dispatch 之前（审批结论先于其授权的副作用落盘，关闭基线风险 R2）。
> 并行批只含声明为只读无副作用的工具，不设屏障；子 Agent 工具调用本无
> canonical 事件，屏障不适用（既有缺口，见 phase0-baseline §一.6）。
> flush 失败（含 pump 过载、落盘失败）→ 工具**不执行**、turn 显式失败。
> 验收证据：`crates/leveler-engine/tests/side_effect_barrier_test.rs`
> （落盘失败不执行、审批结论先于副作用）、
> `phase0_baseline_test::tool_side_effect_cannot_precede_durable_tool_call_started`
> （阶段 0 崩溃窗口测试按约定反转）、`crash_recovery_test` 8 例不变，
> engine/agent/app 三 crate 测试全绿。dangling call 的分类恢复语义
> （可重试/需 idempotency/人工裁决）不变，仍由 M5 恢复链处理。

工作：

- 将规范工具事件从无确认的通知通道改为可等待提交的事件 sink。
- 执行前可靠追加 `ToolCallStarted`，成功或失败后可靠追加 `ToolCallFinished`。
- 将流式 delta、heartbeat 等展示事件保留在允许丢失的瞬时通道。
- 为恢复时发现 dangling call 定义分类：可安全重试、不可自动重试、需要人工裁决。

验收：

- 在“开始事件前、开始事件后、执行过程中、完成事件前、完成事件后”注入崩溃，恢复结果均符合定义。
- 不安全工具不会因 resume 被自动重复执行。
- 事件提交失败时工具不执行，不返回假成功。

### 阶段 2：收敛为唯一 ToolHost 调用边界

**目的：** 让所有工具共享一条不可绕过的安全与执行路径。

> **状态：完成。** ToolHost 管线收拢在
> `crates/leveler-agent/src/executor/host.rs`：
> `admit()`（副作用屏障 → pre-hooks → 权限规则 → profile 策略 → 审批 →
> 屏障）返回 `AdmittedCall`——`dispatch`/`dispatch_raw` 只接受该类型，
> 且其构造器私有于 host.rs，**未经准入的执行无法通过编译**。宿主逃逸
> 提升（`open`/`xdg-open` 的 turn_unrestricted_fs）也移入准入管线，
> 不再散落在循环里。drive.rs 只保留计划分配给 Agent Loop 的职责：
> 批次调度、并发约束（并行批信号量）、结果回灌顺序与 guard 拒绝
> （guard 拒绝不产生执行，无需准入）。
> 架构 tripwire：`crates/leveler-agent/tests/tool_host_boundary.rs`
> 扫描本 crate 源码，`registry.execute` 与 `run_pre` 出现在 host.rs
> 之外即失败。MCP 工具与内置工具同在 registry，走同一 admit 管线。
> 已知豁免（记录在案）：engine 崩溃恢复重放
> （`replay_dangling`）直接调 `ToolRegistry::execute`——它只对
> persisted-risk=Safe 的只读幂等工具生效（风险门在前），不构成仓库写入
> 或进程执行的绕过；阶段 3 收敛恢复入口时一并处理。
> 行为等价证据：agent/engine/app 三 crate 测试全绿（含审批、hooks、
> 沙箱、并行、写串行、取消全部既有用例），无任何测试改动。

工作：

- 用一个 ToolHost `call` 协议串起 schema 校验、风险评估、审批、路径约束、持久化屏障、执行、取消和结果元数据。
- Agent Loop 只保留工具批次调度、并发约束和结果回灌顺序。
- 保留面向模型友好的独立工具 schema；共享执行实现不等于强制合并工具名称。
- MCP 和内置工具使用相同的宿主安全门，不赋予外部 server 隐式高权限。

验收：

- 代码搜索和测试证明不存在绕过 ToolHost 的仓库写入或进程执行路径。
- 审批、权限 profile、hooks、沙箱、路径、取消和进程树行为与基线一致。
- 并行只发生在声明为安全的调用之间；写冲突仍被正确串行化。

### 阶段 3：统一会话恢复与上下文装载

**目的：** 用有序事实代替字符串重叠推断，形成唯一恢复入口。

> **状态：完成。** 两笔提交：
>
> 1. **水位 + 唯一装载入口。** `ContextSnapshot` 事件新增
>    `through_ordinal`（该快照取代的 transcript 前缀长度，serde default，
>    旧行读出 `None`）。恢复时 `raw[n..]` 精确追加；重叠推断只作为旧
>    快照与 executor 环内快照的兼容回退，水位不一致（截断后遗留）时
>    显式 warn 并回退，不猜测切片。驱动它的红灯：两轮文本完全相同的
>    对话在 overlap 推断下整轮丢失（4 条 vs 应有 6 条），已由
>    `watermark_merge_survives_duplicate_rounds` 锁定。装载统一进
>    `session_context.rs` 的 `RawTranscript::{load_strict,load_lossy}` +
>    `assemble()`——chat、resume、goal 续跑、预算扩展、goal 历史注入、
>    app `/btw` 全部走同一实现（顺带删除 app 里 O(n) 全日志扫描的
>    快照查找）。epoch 切换快照（compact/clear/restore）盖当前消息数
>    水位。
> 2. **交互路径补崩溃 reconciliation（基线风险 R3）。** `chat()` 与
>    `resume()` 一样先 `recover_crash_window`：安全只读 dangling 自动
>    重放并补记 finish；可变/未知风险 dangling 返回
>    `RecoveryConfirmationRequired` 硬停，等显式
>    `acknowledge_crash_window`。**迁移影响（行为变化）**：崩溃会话在
>    TUI/Web 重开后首个 chat 可能报确认错误而不是静默继续——这是把
>    原本被隐藏的工作区不确定性暴露给用户，属计划要求的语义修复。
>    证据：`crash_recovery_test` 新增 2 例（chat 阻断至确认、chat 自动
>    重放安全读），全套 10 例绿。

工作：

- 快照记录明确的事件或消息水位 `through_ordinal`。
- 统一通过 `SessionContext` 装载快照和水位之后的增量消息。
- 集中处理 transcript 解析、压缩和兼容逻辑，移除 engine 中重复实现。
- 为旧快照、旧事件与旧数据库提供显式迁移或兼容读取。
- 让 TUI、Web 和手机端消费同一份 snapshot 水位与增量事件，不在客户端猜测缺失状态。

验收：

- 任意消息边界生成快照后恢复，消息不丢失、不重复、顺序不变。
- 旧会话可以继续读取；无法迁移时给出明确错误和处理办法。
- resume 后的权限、goal、工具状态和终止语义与重启前一致。
- 任一客户端断线期间任务可继续；重连或换端接力后不丢消息、不重复工具调用。

### 阶段 4：统一生命周期与 goal 续跑所有权

**目的：** 删除 engine 与 agent 之间重复的 continuation、budget 和完成判断。

> **状态：完成（本阶段范围内）。**
>
> - **唯一生命周期写入者（基线 R4）。** 会话 `status`/`state` 不再由
>   app 层第二次书写：`TerminalRepository::finish_task` 在**一个事务**里
>   同时提交 `outcome + status + state + task_finished 事件`；Running
>   由 engine 在 run/chat/resume 起步时盖章（`mark_running`）。
>   `status_for_app` 从 app 删除，其产品映射移入 engine
>   （`terminal_status_for`）。并行 worktree 驱动器（app/parallel.rs）
>   自己驱动父会话、不经 engine，因此仍是并行类会话的合法生命周期
>   写入者——按执行类型各自唯一，记录在案。
>   **迁移影响**：resume 的早期校验错误（会话已完成/类型不符）不再把
>   status 误写成 Failed（原 app 无条件后写导致）。
> - **类型化停止原因（基线 R5）。** `StopReason` 获得 serde（snake_case）；
>   `TurnFinished`/`TaskFinished` 事件新增 `stop: Option<StopReason>`
>   （serde default，旧行读出 `None`，旧展示字符串字段保留）。blocked
>   现在在 turn 事件、task 事件、会话 `status=Blocked` 三层全部
>   机器可判别；completed/cancelled/budget/failed 分别对应
>   Completed/Interrupted/BudgetLimited/Failed + typed stop。
>   证据：`phase0_baseline_test::blocked_goal_is_typed_in_terminal_events_and_session_status`、
>   `engine_stamps_running_and_terminal_session_status_itself`。
> - **唯一 outcome 合并（基线 R6）。** goal 续跑与预算扩展共用
>   `merge_continued_outcome`。**迁移影响**：stalled-continue 现在与预算
>   扩展一样传递 `budget_exhaustion`——续跑后耗尽预算的 goal 从此可以
>   进入有界扩展（此前被静默丢弃，永远无法扩展）。
> - **刻意不做（转阶段 5）**：engine 的 `continue_active_goal` 与
>   `extend_budget_exhausted` 本身未删除。续跑何时发生是产品策略
>   （TurnPolicy 方向），随阶段 5 的策略外移一并迁移；现有行为由
>   `direct_test` 续跑/预算用例锁定，本阶段先consolidate其状态写入。

工作：

- Engine 只拥有 task/turn 监督、硬预算、取消、恢复和最终停止原因。
- Agent Loop 在一次受监督运行中推进，直到普通 turn 结束，或 goal 明确 `complete` / `blocked`，或触发硬停止。
- 移除 engine 自动重开 turn、隐式扩预算和重复 outcome 合并路径。
- 旧事件继续通过私有兼容解码器读取，不继续扩大当前规范事件枚举。

验收：

- 一个生命周期状态只有一个生产者，类型或测试阻止第二条写入路径。
- 普通任务与 goal 任务的结果能力不退化；事件数量、turn 计数或预算统计若变化，必须记录为迁移影响。
- 取消、预算耗尽、失败、blocked 与 complete 都产生唯一且类型化的停止原因。

### 阶段 5：把产品策略移出 direct loop

**目的：** 精简循环，同时保留复杂任务能力。

> **状态：完成（策略开关化 + 分类；事件驱动 policy 协议与旧策略删除
> 留待真实任务数据，见下）。**
>
> **逐项分类结论**（工作项 1）：
>
> | 机制 | 分类 | 开关 |
> | --- | --- | --- |
> | admission（hooks/规则/审批）、副作用屏障、路径约束、沙箱 | **安全** | 无条件 |
> | StepLimits（命令/文件/token/成本/墙钟）、绝对轮次上限、取消 | **安全** | 无条件 |
> | 事件/消息持久化、恢复协议 | **安全** | 无条件 |
> | goal 终止语义（update_goal / quiet-nudge / Stalled 判定） | **安全**（termination） | `goal_mode` 随 profile |
> | plan-before-write 强制 | 产品 | `TurnPolicy::require_explicit_plan` |
> | 连续搜索预算 | 产品 | `max_search_calls_per_step`（0=关） |
> | todo 完成门 | 产品 | `goal_todo_gate` |
> | Delivery 证据门（complete_step / EvidenceLedger 门禁） | 产品 | `delivery_gate` |
> | 同参同果 loop guard、observe/closeout/all-refused 轮次裁决与强停 | 产品 | **新增 `progress_guards`**（默认开） |
>
> **实现方式**：产品策略全部经 `TurnPolicy` 可开关；`TurnPolicy::minimal()`
> 是最小 direct 模式（全部产品门关闭）。默认策略保持现有行为不变
> （全套既有测试零改动通过 = 能力不低于基线）。子 Agent 继承父的
> guard 取向。验收测试：
> `loop_test::minimal_policy_disables_the_identical_result_loop_guard`
> （最小模式不装载停滞机制）、`default_policy_still_denies_identical_result_loops`
> （默认不变）、`minimal_policy_keeps_the_admission_boundary`
> （切换 policy 不动摇审批/沙箱/取消边界）。
>
> **刻意保留待做**（需真实复杂任务 eval 数据，非本环境可完成）：
> 把策略实现从 drive.rs 迁到独立 policy 模块并定义
> 「规范事件输入 → 受限控制信号输出」协议；以及基于 eval 对照决定
> 删除哪些旧启发式。当前每个策略已可独立关断，为该迁移提供了
> 行为等价的对照面。engine 侧续跑（continue/extend）同属产品策略，
> 与此一并迁移。

工作：

- 将强制 plan-before-write、`complete_step`、EvidenceLedger、搜索/观察/停滞/收尾启发式逐项识别为安全机制或产品策略。
- 安全机制留在核心；产品策略迁移为可选 policy，并先作为默认策略保持现有行为。
- 定义 policy 输入的规范事件和输出的受限控制信号，不允许 policy 直接执行工具或写存储。
- 用真实复杂任务数据决定是否删除旧策略，不以代码量作为删除依据。

验收：

- 默认 policy 对复杂任务、验证和 goal 模式的能力不低于阶段 0 基线。
- 最小 direct 模式无需装载规划、证据和停滞状态机即可完成简单任务。
- 切换 policy 不改变权限、沙箱、持久化、取消和恢复语义。

### 阶段 6：收紧模型与 provider 边界

**目的：** 只保留真实实现且可验证的厂商中立接口。

> **状态：完成。**
>
> - **不宣传未实现协议。** `configs/example.yaml` 只宣传已实现的
>   `openai_chat` / `anthropic_messages`；`openai_responses` 与
>   `gemini_generate_content` 变体保留在 `ProtocolKind`（解析成功 →
>   registry 以命名错误 `UnsupportedProtocol` 立即失败，绝不静默回退），
>   由 `registry.rs::unimplemented_protocols_fail_loudly_by_name` 与
>   `implemented_protocols_resolve` 锁定。
> - **一致性测试盘点。** 两个已实现协议各自覆盖：请求编码（含工具、
>   温度、reasoning 风格）、流式（分片 tool-call 组装）、截断
>   （`length`/`max_tokens` → `FinishReason::Length`）、畸形/超大参数
>   报错不猜、finalize 幂等（openai_chat 30 例、anthropic_messages
>   14 例，本阶段核查为已齐备）。
> - **同步/流式组装统一。** 新增
>   `leveler_model::stream_from_response`：非流式响应到规范事件流的
>   唯一定义（started → 内容序 → usage → completed，含零 usage 不上报），
>   单测锁定；engine 侧 4 个测试替身的手写 stream 全部改用它。
> - **厂商细节零泄漏（复核）。** engine/agent/tools/TUI 源码无
>   Authorization/x-api-key/anthropic-version 等厂商 wire 细节。
> - **crate 边界决策。** `leveler-protocol` 保持独立 crate、只导出
>   adapter 实现（trait 在 `leveler-model`）：合并进 provider 只减
>   crate 数不改边界，与实施原则「不为减少 crate 合并职责」一致，
>   不做。
> - 无失效 middleware（全仓无 provider 侧 middleware）；`policy:` 旧
>   模型分档配置早已按硬错误处理（example.yaml 注明）。

工作：

- 不再公开宣称尚未实现的协议；新增协议必须有适配与一致性测试。
- 删除无调用方的 middleware 和失效配置；旧配置字段按兼容策略继续解析并给出迁移提示。
- 评估将 `leveler-protocol` 收为 provider 内部实现，保持 `leveler-model` 为中立公共边界。
- 统一同步响应与流式响应的组装语义，减少 provider 和测试替身的重复实现。

验收：

- 所有被广告的 provider/protocol 组合都有请求、流式、工具调用、截断与错误一致性测试。
- 厂商 JSON、鉴权头和 endpoint 特例不会出现在 engine、agent、tools 或 UI。
- 旧配置在兼容窗口内可读；不支持的配置明确失败，不静默降级。

### 阶段 7：稳定性与开源发布门槛

**目的：** 证明核心能长期运行，并为外部贡献建立可维护契约。

> **状态：进行中（可在本环境完成的部分已完成）。**
>
> **已完成：**
>
> - `docs/STABILITY.md` 三项开放决策已按业内 pre-1.0 惯例落定并生效
>   （D1=保留 deny_unknown_fields 并文档化前向不兼容；D2=Rust API 在
>   1.0 前不作公开承诺；D3=serve/web 等保持 Provisional），含弃用周期
>   （N 弃用 → ≥N+2 移除）；CONTRIBUTING 已链接。
> - 存储迁移 + 备份/恢复说明入库（STABILITY「Storage」节 +
>   migrations/README）；规范事件已带 `schema_version` 且新于本构建的
>   行是命名硬错误（阶段 0 既有测试）。
> - client protocol 已有版本（1.3）与兼容性校验（`version.rs`），
>   握手不兼容显式报错；Web/手机 transport 复用同一协议。
> - **故障测试盘点**（现有自动化）：崩溃窗口全分类
>   （`crash_recovery_test` 10 例 + 副作用屏障 4 例）；SQLite busy/full/
>   投影回滚（`terminal_repo` 4 例）；provider 断流/畸形分片
>   （protocol 流式错误用例）；审批超时与本机在场豁免（remote
>   `approval_timeout` 7 例）；取消（engine/TUI/手机各层）；孤儿进程
>   （`zombie_turns`、reaper、Linux PDEATHSIG）；事件缓冲过载显式失败
>   （recorders 3 例）；确定性短 soak（`tui_path_soak`）在 CI 每次运行。
>
> **环境受限，未完成（发布前必须补，不得以本清单替代）：**
>
> - ≥24 小时确定性长稳 soak（墙钟不可压缩；资源增长阈值待 soak 数据）。
> - 手机端真机验收（iOS 签名/Android SDK/蜂窝网络，见
>   [`phase1-acceptance-checklist.md`](phase1-acceptance-checklist.md)）。
> - TUI→Web→手机同 session 跨端接力的单场景自动化（基线 R8）。
> - Windows/Linux 全平台矩阵在 CI 之外的实机复核。

工作：

- 完成 `docs/STABILITY.md` 中 CLI、配置与 Rust API 的开放决策，明确版本和弃用周期。
- 将 client protocol、Web transport 和手机远程协议纳入版本、能力协商与兼容决策。
- 建立存储迁移、规范事件版本与数据库备份/恢复说明。
- 为崩溃、断网、provider 断流、审批超时、取消、磁盘错误和孤儿进程建立故障测试。
- 增加定期 soak：CI 中运行短周期；发布前运行至少 24 小时的确定性长稳测试。
- 完成贡献者架构说明、安全模型、威胁边界和最小复现模板。

验收：

- 长稳测试期间无未处理 panic、死锁、孤儿进程、跨 session 串流或不可解释的任务丢失。
- 重启后所有已接受任务均能恢复、明确停止或进入人工裁决，不出现假完成。
- 资源在预热后不存在持续单调增长；具体内存、句柄和数据库增长阈值先以阶段 0 基线制定并入库。
- Linux、macOS、Windows 的能力差异被测试并如实报告，缺少沙箱时不声称完全隔离。
- TUI、Web、手机端分别通过发布验收，并完成同一 session 的跨端接力测试。
- `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、`cargo test --workspace` 全部通过。

多端发布验收至少包括：

| 客户端 | 必须通过 |
| --- | --- |
| TUI | 创建/继续任务、流式显示、审批、澄清、取消、关闭后任务继续、重开后 snapshot/resync |
| Web | token 鉴权、流式与重连、审批/取消、多标签 session 隔离、多项目路由与 daemon 复用 |
| 手机端 | 配对与指纹确认、设备撤销、远程审批限制、前后台/断网重连、snapshot/resync、多项目隔离 |
| 跨端 | 同一 session 在 TUI、Web、手机端之间接力，事件顺序一致，无重复执行、无状态分叉 |

手机端的真机、蜂窝网络和发布包门槛继续由
[`phase1-acceptance-checklist.md`](phase1-acceptance-checklist.md) 记录；该清单是本计划多端验收的组成部分，不能用模拟器结果替代真机结论。

## 五、整体验收矩阵

| 类别 | 必须证明 |
| --- | --- |
| 功能 | 简单任务、复杂 goal、验证修复、审批、取消、resume、子 Agent、内置与 MCP 工具均无能力回退 |
| 正确性 | 唯一状态所有者、规范事件有序、停止原因类型化、上下文恢复不重不漏 |
| 副作用 | 工具开始先落盘；危险调用不盲目重试；失败不会被记录为成功 |
| 安全 | 路径、权限、sandbox、hooks、敏感信息脱敏及进程树取消不能被模型或 policy 绕过 |
| 兼容 | 旧 CLI/配置/数据库/事件按公布窗口工作，破坏性变化有迁移、版本和 changelog |
| 模型 | 已支持 provider 的流式、工具调用、错误与取消语义一致；未实现协议不对外广告 |
| 多端 | TUI、Web、手机共享协议事实；断线重连、跨端接力、多项目隔离、认证与撤销全部通过 |
| 长稳 | 故障注入与 soak 通过，无任务静默丢失、死锁、孤儿进程和持续资源泄漏 |
| 扩展 | 新工具、provider、policy 和未来 NPC 通过公开边界接入，不复制核心状态机 |

## 六、明确不做

- 不一次性删除 plan、evidence、continuation 或 guard 后再观察回归。
- 不以合并 crate、拆文件或降低行数代替职责收敛。
- 不在同一阶段同时修改持久化协议、安全语义和产品策略。
- 不为了兼容而静默吞错、猜测旧状态或返回假成功。
- 不让 NPC、多 Agent、UI 或远程控制成为 Engine 正确运行的必要依赖。
- 不为 TUI、Web 或手机端分别维护一套任务状态机，也不以“非核心”为由降低其验收标准。

## 七、执行与记录

每个阶段开始前建立独立的任务清单，并在合并时记录：

1. 被删除的重复职责和新的唯一所有者；
2. 新增的失败测试、红灯原因与通过证据；
3. 行为、事件、存储、配置和公开 API 的兼容影响；
4. 完整检查结果以及未能验证的环境项；
5. 本文对应阶段的状态更新。

只有整体验收矩阵全部有可复查证据后，才能宣称“核心收敛完成”。
