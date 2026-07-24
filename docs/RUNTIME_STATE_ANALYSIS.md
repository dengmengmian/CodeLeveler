# Runtime State 分析（Phase 1，仅分析不改）

> 结论先行：可观测性的**事件总线已经很完整**（`RuntimeEvent` 20+ 变体 + `TurnPhase` + `AgentActivity`
> 标签），根因不是"没有事件系统"，而是三处具体缺口：①**前台命令执行是阻塞的、执行中零反馈**
> （长 `cargo test` = 6 分钟黑盒）②**阶段太粗**，长模型推理只回落到"等待模型"③**终态用词把
> "测试失败/模型放弃" 说成"被阻塞"**，语义误导。你架构判断对：在 Agent Core 发事件 → 总线 →
> TUI/Eval/Logs 复用，是正确的层。以下按证据展开。

## 一、两张截图对应的真实链路

### 截图 A：`等待模型 · 6m30s · ↑24,494 ↓656`

- 渲染：`status_line.rs:383` — 状态取 `state.activity`，为空则回落 `t.waiting_model`（"等待模型"，
  `i18n.rs:422`）。
- `state.activity` 只由 `RuntimeEvent::AgentActivity{label}` 更新（`event_bridge.rs`）。
- 长模型推理（reasoning 模型单轮可达 ~95s，见记忆 `waiting-for-model-real-cause`）期间**不产生新的
  AgentActivity**，于是整轮显示"等待模型"，只有 `streaming_output_estimate`（`↓~`)在动。
- 判定：**不是卡死**，是"正常慢但不可见"。缺的是推理阶段的心跳/阶段标签。

### 截图 B：`被阻塞 · failed gate(s): cargo test`

- `被阻塞` = `i18n.rs:619 final_blocked`，走 `transcript_lines.rs` 终态标记。
- 状态映射（`session.rs:110`）：`StopReason::Blocked → SessionStatus::Blocked`。
- `StopReason::Blocked` 的语义（`executor.rs:410`）：**"Goal 模式下模型 `update_goal(blocked)` 声明目标
  不可达"**（`drive.rs:1047`）。
- summary `failed gate(s): cargo test` 来自 `verification_failure_summary`（`session.rs:136`）。
- 判定：这不是取消（Ctrl+C 正确映射到 `Interrupted`，`session.rs:114`），是**模型在 cargo test
  反复失败后放弃**。但"被阻塞"读起来像"系统卡住需介入"，与真实语义"agent 放弃了失败的测试"错位。

## 二、当前状态系统（已有，复用）

| 资产 | 位置 | 说明 |
|------|------|------|
| 事件总线 `RuntimeEvent` | `leveler-client-protocol/src/event.rs` | 20+ 变体，供 TUI/Web/Eval 复用 |
| 轮次阶段 `TurnPhase` | `leveler-lifecycle` | Active/AwaitingModel/ToolBatch/Closing/AwaitingUser/Terminal |
| `TurnProgress` 事件 | `event_bridge.rs:345` | phase + closing + no_progress_streak |
| 活动标签 `AgentActivity` | `event_bridge.rs` 多处 | 压缩上下文/续跑/无进展 streak/gate refused 等 |
| 工具事件 | `ToolCallStarted/Completed` | 名称+参数+耗时（客户端测量） |
| 终态 | `TurnCompleted/Answered/Truncated/Incomplete/CompletedUnverified` | 区分"完成/已答/截断/未完/未验证" |
| 流式 token 估计 | `status_line.rs streaming_output_estimate` | 推理时 `↓~` 在动 |
| 后台任务日志泵 | `execution/background.rs:319 spawn_log_pump` | **仅后台任务**有 stdout/stderr 泵 |

## 三、用户可见状态（现状）

- Busy：spinner + `activity || 等待模型` + elapsed + `↑↓ tokens` + `↓~估计` + 队列数。
- 终态标记：完成 / 已答 / **被阻塞** / 未完成 / 未验证 / ⊘已取消。
- 工具调用：开始/结束两点（名称 + 结果 preview + 耗时）。

## 四、缺失状态（真缺口）

| # | 缺口 | 证据 | 影响 |
|---|------|------|------|
| G1 | **前台命令执行中零反馈** | `run_command.rs:292 context.runner.run(...).await` 单次 await 返回完整输出，不流式；无 elapsed/stdout 心跳 | 长 `cargo test/build/npm i` = 黑盒；只有 ToolCallStarted→Completed 两点 |
| G2 | **无 CommandMonitor** | 无"last_output_time / 输出频率 / 进程存活"跟踪 | 无法区分"正常慢 / 异常慢 / 卡死" |
| G3 | **阶段太粗** | `TurnPhase` 只 6 态；推理无 `Thinking`，命令无 `RunningCommand`，验证无 `Verifying`，失败无 `AnalyzingFailure/Recovering` 独立呈现 | 长任务只显示"等待模型" |
| G4 | **终态用词误导** | `StopReason::Blocked`（模型放弃）+ 门失败 → "被阻塞"；用户读作"系统卡住" | 假 blocked 体验；应区分 CommandFailed→recovery vs 真 Blocked（缺权限/需输入） |
| G5 | **无 TTFF / SilentDuration 指标** | eval 无"首次反馈时延 / 最长静默"度量 | 无法量化"黑盒等待"退化 |
| G6 | **心跳缺失** | 无周期性 `Heartbeat{phase,current,elapsed,last_output}` 事件 | UI 无法证明"还活着" |

## 五、改造方案（分级，先低风险高价值）

**架构原则**：所有新状态在 **Agent Core / executor 层发事件**，经 `RuntimeEvent` 总线，TUI/Eval/Logs 复用。
不在 TUI 层造状态。

| 级别 | 改动 | 触及 | 风险 | 价值 |
|------|------|------|------|------|
| L1 | **终态用词修复**（G4）：`被阻塞` 拆成 `命令失败`/`已取消`/`需要你介入`；门失败走"分析→修复→重试"话术而非"被阻塞" | i18n + session marker + transcript_lines | 低（纯呈现，不改状态机） | 高（直击截图 B） |
| L2 | **命令执行透明**（G1/G6）：前台命令流式泵 stdout + 周期 `CommandProgress{cmd,elapsed,last_line}` 事件；TUI 显示 `Running cargo test · 02:31 · <末行>` | run_command 执行体 + 新事件 + 总线 + TUI | 中（executor 流式化） | 高（直击截图 A/长命令） |
| L3 | **智能超时/Monitor**（G2）：`CommandMonitor` 记 start/last_output/alive；正常慢=继续、异常慢=提示 Continue/Inspect/Cancel、卡死=进入恢复 | 新 monitor + 命令执行 | 中 | 中高 |
| L4 | **细化 AgentPhase**（G3）：在现有 `TurnPhase` 上补 `RunningCommand/Verifying/AnalyzingFailure/Recovering`，状态行按 phase 呈现 | lifecycle + event_bridge + TUI | 中 | 中 |
| L5 | **Eval 指标 + 场景**（G5，接我已建的 eval）：`TTFF`、`SilentDuration`、`FalseBlockedRate`、`UserInterruptAccuracy`；slow/hang/fail/interrupt/long-thinking 5 类场景 | leveler-eval + scenarios | 低-中 | 中（可回归防退化） |

## 六、建议落地顺序

L1（用词，纯呈现、可立即做且直击截图 B）→ L2（命令透明，直击截图 A）→ L5（指标锁住不回退）
→ L3（Monitor）→ L4（细 phase）。L2/L3 触及 executor，改动前会先小步验证。

## 七、边界

- 不在 TUI 层伪造状态：无真实事件的阶段不显示（同 eval 的"不捏造"原则）。
- Ctrl+C 现已正确映射 `Interrupted`，不动；只修"门失败/模型放弃"被叫成"被阻塞"。
- L2 流式化需保证不破坏现有 `ToolCallCompleted` 的完整输出契约与超时语义。
