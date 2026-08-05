# 阶段 0 基线：真实执行路径、状态所有权与能力矩阵

> 本文是 [`core-runtime-convergence-plan.md`](core-runtime-convergence-plan.md) 阶段 0 的验收产物。
> 记录 2026-08 时点的真实调用链、每个生命周期状态的权威写入者、能力基线矩阵、
> 已证实的缺陷与未验证项。本阶段不改变任何用户可见行为。
> 所有结论对应到具体代码或测试；文件路径以 `crates/` 为根。

## 一、真实调用链（按代码验证，非按文件长度推测）

### 1. Direct / chat turn（所有产品执行的唯一路径）

```text
TUI / Web / 手机
  → leveler-client-protocol (ClientCommand / CommandEnvelope)
  → InProcessRuntimeClient::send / deliver          leveler-app/src/interactive.rs
      spawn_content_turn / spawn_direct_goal_turn   （spawn_blocking + block_on）
  → Application::run_in_session_with_*              leveler-app/src/session.rs
      写 sessions.status=Running（第一处生命周期写入，见 §二）
  → TaskEngine::run / chat                          leveler-engine/src/engine.rs
      chat() 先锚定 base_commit（baseline.rs，失败归因用）
  → TurnRunner::run_turn                            leveler-engine/src/turn.rs
      reap_running_turns（僵尸 turn 清理）
      TurnRepository::start → TurnStarted 落盘
      futures::join!(exec, pump)：
        exec  = ExecutorFactory::build → Executor::run/run_conversation/resume
        pump  = EventEmitter(容量 256) → EventLog::append（persist-before-forward）
  → Executor::drive                                 leveler-agent/src/executor/drive.rs
      每轮：模型流式请求 → 工具批次 →
        observer(ToolCall) → authorize()（风险/审批/hooks）→ dispatch()
        （read-only 且 supports_parallel 的调用进并发批；写调用串行）
  → ToolRegistry::execute → leveler-execution（路径/沙箱/进程树取消）
  → TurnFinished + turns 行终止态原子提交（TerminalRepository::finish_turn）
  → TaskEngine::finish_task：task_finished 事件 + sessions.outcome 原子提交
  → Application 写 sessions.status/agent_state 终态（第二处生命周期写入）
  → EngineEvent → engine_event_to_agent → EventBridge → RuntimeEvent（broadcast）
  → 各客户端渲染；事实恢复只走 snapshot()（读 DB）
```

关键点：canonical 事件的持久化顺序 = executor 的发射顺序（pump 单消费者），
持久化失败会中止 turn（`EventLog::append` 注释与 `persists_before_forwarding` 测试）；
canonical 事件溢出（256 满）触发 cancel + `EventBufferOverloaded` 显式失败
（`recorders.rs::canonical_overflow_is_retained_and_reported`）。

### 2. Goal continuation（谁在续跑）

两层续跑并存，职责边界如下（阶段 4 的收敛对象）：

| 层 | 机制 | 位置 |
| --- | --- | --- |
| Executor（turn 内） | 轮次推进、quiet-nudge、closeout、停滞 guard，直到 `update_goal(complete/blocked)` 或 guard 停止 | `leveler-agent/src/executor/drive.rs`、`closeout.rs` |
| Engine（turn 间） | 仅 `StopReason::Stalled` 且 `progress.allows_engine_continue()` 时重开一个持久化 turn（`continue_active_goal`）；`BudgetExhausted` 且有真实进展时按 `MAX_BUDGET_EXTENSIONS` 上限扩预算重跑（`extend_budget_exhausted`） | `leveler-engine/src/engine.rs:902,1041` |

重复点（已确认，不在本阶段修）：`continue_active_goal` 与 `extend_budget_exhausted`
各自维护一份几乎相同的 outcome 合并逻辑（rounds/final_text/stop_reason/progress/
metrics/modified_files 六项手工合并，两处）。

### 3. Tool call / 审批 / 取消

单个调用的事件与执行顺序（`drive.rs:1352-1539`）：

```text
observer(ToolCall)            ← 宣布（进入 pump 队列，不等待落盘）
→ authorize()                 ← 风险分类 + hooks + Approver.decide()
    RecordingApprover 发 ApprovalRequested / ApprovalResolved（engine/recorders.rs）
→ dispatch()                  ← 真正执行（副作用发生点）
→ observer(WorkspaceSnapshot?) → observer(ToolResult)
```

- 审批发生在执行前：由 `authorize_tests`（dispatch.rs）与
  `loop_test::dangerous_command_denied_is_fed_back_not_executed`、
  `auto_reviewer_can_deny_before_user_approval` 锁定。
- guard 拒绝（plan gate / 搜索预算 / 写白名单 / 预算）统一走 `deny_call`：
  先宣布再拒绝，模型收到原因回灌。
- 取消：`CancellationToken` 从客户端 `cancel_active` 一路传到模型流与
  `leveler-execution` 进程树；turn 记 `Interrupted`，task 记 `Interrupted`
  （`direct_test::cancellation_is_recorded_as_interrupted`）。
  长工具执行中取消会停止本批后续工具，但已运行工具的结果仍入账
  （drive.rs `cancelled_mid_batch`）。

### 4. Verification / repair

`conclude_direct`（engine.rs:1123）：Completed/Answered/（Incomplete+有改动）→
`verify()` 跑 gate 命令（leveler-verifier）→ 失败且可修复时 `repair_goal` 重开一个
repair turn（有界，`failed_verification_repairs_once_then_fails`）。
完成判定只认项目自身 gating 检查；无 gate 或无改动 → 最多 `CompletedUnverified`
（K19 短路，`pure_qa_with_green_gates_is_completed_unverified`）。

### 5. Resume 与崩溃窗口

- `TaskEngine::resume`（engine.rs:521）：拒绝 kind 不符 / 已成功会话；严格反序列化
  transcript；`latest_context_snapshot` + 水位后增量消息合并（`budget_prior_messages`，
  "never drop post-snapshot transcript rows"）；然后 **先** `recover_crash_window`
  **再** 续跑。
- dangling call 分类（`log.rs::dangling_tool_calls` + engine.rs:671）：
  - 无 persisted risk / 非幂等 → `RecoveryConfirmationRequired` 硬停，等
    `acknowledge_crash_window`（CLI `leveler resume --acknowledge...`，run_cmds.rs:854）；
  - pending_approval → 同样硬停（审批落盘与 dispatch 存在竞态窗口，注释明确）；
  - 幂等只读 → 自动重放，结果只进事件日志不进 transcript。
  - 全部由 `crash_recovery_test.rs` 8 个用例锁定。
- **已确认的语义缺口**：`recover_crash_window` 只在 `resume()` 路径被调用。
  交互路径（TUI/Web 重开会话后继续 chat）走 `TaskEngine::chat`，不做 dangling
  reconciliation——崩溃遗留的 dangling `ToolCallStarted` 永远悬着，交互续聊
  不会提示人工裁决。（`reap_running_turns` 只清 turns 行，不清 dangling call。）

### 6. Subagent

`spawn_agent`（injected_tools.rs → drive.rs spawn 处理）：深度 1、并发 4、总量 6、
15 分钟墙钟；explorer 只读 / worker 需互斥 `files`；子事件以
`SubAgentStarted/Progress/Activity/Finished` 归属 id 重发（transient，不进父
transcript）。父子共享命令预算（`multi_agent_test` 全套锁定，含
`concurrent_spawns_emit_attributed_tool_activity`、
`exhausted_parent_command_budget_hard_blocks_child`）。

### 7. Provider streaming

```text
HTTP bytes → SSE 解码 → protocol chunk 解码（openai_chat / anthropic_messages）
→ 分片 tool-call 组装 → ModelEvent 流 → executor/stream.rs 消费
```

畸形/截断 JSON 报错不修复为可执行调用（协议 crate 内单测）。厂商 JSON 不出协议层。

### 8. 多端 transport

| 端 | 通道 | 事实来源 |
| --- | --- | --- |
| TUI | in-process 或 Unix socket（leveler-local-transport 帧协议 + 握手） | `snapshot()` + `subscribe_session` |
| Web | axum REST + WS（loopback-only、constant-time token）；多项目 RouterService 按 session→project 路由 | 同上，经 daemon socket |
| 手机 | leveler-remote-agent：签名信封过不受信 relay、设备配对/撤销、远程审批禁 ApproveAlways、120s 超时 | 同上 + resync_required |

客户端断开不改变任务事实：turn 由 spawn_blocking 驱动，不持有客户端连接；
命令幂等（`duplicate_command_id_dispatches_once`）；会话/项目隔离
（`daemon_event_subscriptions_are_isolated_per_session`、
`socket_clients_receive_only_their_session_events`、
`events_do_not_cross_between_two_open_projects`）。

### 9. 目标调用链与当前差异

目标边界见计划 §二。当前实际差异（即后续阶段的工作量所在）：

1. ToolHost 尚不是一个独立 `call` 协议：schema/风险/审批/hooks 在
   `Executor::authorize` + `deny_call` + guard 分支里内联（阶段 2）。
2. 工具事件持久化屏障不存在：宣布是 fire-and-forget（阶段 1，见 §四）。
3. 产品策略（plan gate、搜索预算、停滞 guard、closeout、evidence）与 direct loop
   同体（阶段 5）。
4. 生命周期在 app 层还有第二份表示（§二 A 项，阶段 4）。

## 二、状态所有权与重复写入路径

| 状态 | 权威写入者 | 第二写入路径 | 评估 |
| --- | --- | --- | --- |
| `sessions.outcome` + `task_finished` 事件 | `TaskEngine::finish_task`（原子提交） | 无 | ✅ 唯一 |
| `sessions.status` / `agent_state` | `Application::run_in_session_with_policy`（Running/终态两次） | `insert_session`、context ops 等 app 内多处 | ⚠️ 与 outcome 是同一生命周期的两份表示、两层写入者。崩溃在 finish_task 与 update_status 之间 → status 永远 Running 而 outcome 已终止（阶段 4 收敛对象） |
| `turns.status` + `turn_finished` 事件 | `TurnRunner::run_turn`（原子提交） | `reap_running_turns`（僵尸清理，interrupted） | ✅ 第二写入是刻意的崩溃清理，语义明确 |
| `tool_call_started/finished` 事件 | executor 发射 → turn pump 持久化 | 恢复路径 `replay_dangling` / `record_recovery_skip` / `acknowledge_crash_window` 补 finished | ✅ 补录是 reconciliation，永不伪造成功（errored marker） |
| 审批事件 | `RecordingApprover` / `RecordingClarifier` | 无 | ✅ 唯一；重启不重建 pending 审批（`pending_approval_restart.rs`） |
| plan / evidence / progress | executor（运行时唯一写入者，事件为 SoT） | engine 只做 seed（turn.rs `should_seed_task_state` 门控） | ✅ 读写分离清楚 |
| transcript 消息 | `TurnSink`（turn 内唯一） | context ops（truncate/clear/compact/checkpoint restore） | ✅ 第二写入者是显式用户命令，且 turn 活跃期被拒（`context_ops_reject_while_turn_is_active`） |
| goal 续跑决定 | executor（turn 内）+ engine（turn 间，仅 Stalled/预算扩展） | outcome 合并逻辑在 engine 两处重复 | ⚠️ 阶段 4 对象；当前行为有测试锁定 |
| 停止原因 | executor `StopReason`（typed）→ engine 映射 `TaskOutcome` | `TurnFinished.stop_reason` 用 `format!("{:?}")` 持久化 | ⚠️ 见 §五 |

## 三、能力基线矩阵

| 能力 | 档 | 证据 |
| --- | --- | --- |
| direct turn 持久化全链（turns/消息/事件/终态） | ✅ 自动化 | `direct_test::direct_run_persists_turns_messages_events_and_outcome`（含事件顺序 + 序列无洞断言） |
| 工具调用顺序可从事件还原 | ✅ 自动化 | 同上 + `phase0_baseline_test`（started < finished）+ `event_bridge::tool_result_pairs_by_id_even_when_out_of_order` |
| 审批发生在执行前 / 拒绝不执行 | ✅ 自动化 | `dispatch.rs::authorize_tests`、`loop_test::dangerous_command_denied_is_fed_back_not_executed` |
| 取消传播（turn/task 记 Interrupted） | ✅ 自动化 | `direct_test::cancellation_is_recorded_as_interrupted`；手机端取消 `pairing_flow_test.dart`（模拟器） |
| 进程树取消 / 孤儿清理 | ✅ 自动化（capability-gated） | `leveler-execution` 单测 + PDEATHSIG（Linux）；macOS 实机 soak 为手工 |
| resume 不丢消息（snapshot+水位合并） | ✅ 自动化 | `direct_test::interrupted_direct_task_resumes_from_the_persisted_transcript`、`multi_turn_session_test::multi_turn_deictic_followup_after_compact_then_resume` |
| 崩溃窗口分类恢复（resume 路径） | ✅ 自动化 | `crash_recovery_test.rs` 全部 8 例 |
| 崩溃窗口 reconciliation（交互 chat 路径） | ❌ 缺陷/未覆盖 | 只有 `resume()` 调 `recover_crash_window`；`chat()` 不调（§一.5） |
| ToolCallStarted 先于副作用落盘 | ❌ 缺陷已证实 | `phase0_baseline_test::tool_side_effect_can_precede_durable_tool_call_started`（确定性复现） |
| goal complete/blocked | ✅ 自动化 | `loop_test::goal_mode_update_goal_emits_completed_tool_event`、`goal_mode_blocked_reports_blocked`、`quiet_without_update_goal_is_not_task_success` |
| goal 自动续跑 + 上限 | ✅ 自动化 | `direct_test::active_goal_automatically_continues_in_a_new_persisted_turn_after_stall`、engine.rs 单测（no-progress cap） |
| 预算耗尽 / 有界扩展 | ✅ 自动化 | `loop_test::configured_token_budget_stops_before_another_model_request`、`budget_extension_policy_grants_refuses_and_caps` |
| verification / repair | ✅ 自动化 | `direct_verification.rs` 6 例、`failed_verification_repairs_once_then_fails` |
| 子 Agent（并发/角色/预算/事件归属） | ✅ 自动化 | `multi_agent_test.rs` 全套 |
| provider streaming（SSE 分片/工具组装/截断报错） | ✅ 自动化（单元级） | `leveler-protocol` 内联测试；真实 provider 为手工（本机 proxy 影响 provider 测试，见测试环境备忘） |
| TUI（渲染/reducer/会话 e2e） | ✅ 自动化 + 🖐️ PTY | `leveler-tui/tests` 6 组；PTY 驱动为脚本手工 |
| TUI 关闭后任务继续（daemon） | ✅ 自动化 | `command_delivery::creating_a_daemon_session_does_not_reap_another_live_turn` 等 |
| Web（token/WS/快照/文件端点安全） | ✅ 自动化 | `leveler-web/tests/server.rs` 20+ 例 |
| Web 多项目路由 / daemon 复用 | ✅ 自动化 + 🖐️ | `end_to_end.rs::events_do_not_cross_between_two_open_projects`；E2E 脚本手工 |
| 手机（配对/撤销/审批限制/重连 resync） | ✅ 自动化（后端+模拟器） / ⏳ 真机 | `leveler-remote-agent/tests` 7 组 + [`phase1-acceptance-checklist.md`](phase1-acceptance-checklist.md)（真机/Android 未做） |
| 跨端接力（同 session 多客户端） | 🟡 部分 | socket 多客户端隔离与命令幂等已自动化；「TUI→Web→手机同 session 顺序接力」无单一场景测试（阶段 7 验收项） |
| 停止原因可判别 | 🟡 部分（见 §五） | `phase0_baseline_test::blocked_goal_collapses_to_failed_at_the_task_level` 锁定现状 |

## 四、ToolCallStarted 副作用屏障：真实调用链验证结论

**结论：崩溃窗口存在，且已确定性复现。** 链路：

1. `drive.rs:1352` `observer(AgentEvent::ToolCall{..})` — 同步闭包；
2. → `EventEmitter::emit` → `try_send`（有界 channel，**不等待**）；
3. executor 立即继续 `authorize` → `dispatch`（副作用发生）；
4. pump（`turn.rs:280`）异步消费并 `EventLog::append` 落盘。

副作用与 started-row 落盘之间没有同步点。测试
`phase0_baseline_test::tool_side_effect_can_precede_durable_tool_call_started`
用一个 gated `EventStore`（把 `tool_call_started` 的落盘保持挂起，同时观察工作区）
证明：**补丁已写入工作区时，started 行仍未持久化**。此窗口内进程崩溃 →
副作用存在但事件日志无 dangling 记录 → `dangling_tool_calls` 与整套 M5 恢复分类
都看不见它。审批同理：`recover_crash_window` 的注释已承认
ApprovalResolved 落盘与 dispatch 的竞态。

阶段 1 的验收方式已内建：屏障建立后该测试的断言应反转（测试自身注释已写明）。
本阶段不修。

## 五、停止原因盘点（completed / blocked / cancelled / budget / failed）

| 语义 | agent 层（typed） | task 层（typed） | 判别性 |
| --- | --- | --- | --- |
| completed | `Completed` / `Answered` / `CloseoutForced`（+合成 `CompletedUnverified`） | `Verified` / `CompletedUnverified` | ✅ |
| cancelled | `AgentError::Cancelled` | `Interrupted` | ✅ |
| budget exhausted | `BudgetExhausted`（`TurnLimitReached` 刻意除外） | `BudgetLimited` | ✅ |
| blocked | `Blocked` | **`Failed`（塌缩）** | ❌ task 层不可判别 |
| failed | `Incomplete` / `Stalled` / `TurnLimitReached` / 错误 | `Failed` | ✅（但与 blocked 混同） |

两个已证实问题（阶段 4 输入，不在本阶段修）：

1. `direct_non_success_outcome`（engine.rs:1588）把 `Blocked` 映射为
   `TaskOutcome::Failed`；"blocked" 只存活在 `TaskReport.stop_reason`（内存）与
   `TurnFinished.stop_reason`（持久化）。
2. `TurnFinished.stop_reason` 用 `format!("{:?}")`（turn.rs:314）持久化为 Debug
   字符串（如 `"Blocked"`），非 serde 序列化——重命名枚举变体即破坏事件兼容。
   现状由 `phase0_baseline_test::blocked_goal_collapses_to_failed_at_the_task_level` 锁定。

## 六、已发现的正确性 / 安全 / 恢复风险汇总

| # | 风险 | 类别 | 证据 | 归属阶段 |
| --- | --- | --- | --- | --- |
| R1 | ~~副作用先于 `ToolCallStarted` 落盘（崩溃窗口）~~ **已修**（阶段 1 flush 屏障） | 恢复 | §四；`side_effect_barrier_test.rs` + 反转后的窗口测试 | 阶段 1 ✅ |
| R2 | ~~审批 resolved 落盘与 dispatch 竞态~~ **已修**（dispatch 前第二次 flush） | 恢复 | `side_effect_barrier_test::approval_resolution_is_durable_before_dispatch` | 阶段 1 ✅ |
| R3 | ~~交互 chat 路径不做崩溃窗口 reconciliation~~ **已修**（chat 与 resume 同链） | 恢复 | `crash_recovery_test::chat_blocks_on_a_mutating_dangling_call_until_acknowledged` | 阶段 3 ✅ |
| R4 | ~~status/outcome 双写入者可分叉~~ **已修**（finish_task 单事务盖全列;app 不再写） | 正确性 | `engine_stamps_running_and_terminal_session_status_itself` | 阶段 4 ✅ |
| R5 | ~~blocked 不可判别;stop_reason 为 Debug 字符串~~ **已修**（typed `stop` 字段+`status=Blocked`） | 正确性 | `blocked_goal_is_typed_in_terminal_events_and_session_status` | 阶段 4 ✅ |
| R6 | ~~两处 outcome 合并重复~~ **已修**（`merge_continued_outcome` 唯一实现） | 正确性 | engine.rs | 阶段 4 ✅ |
| R7 | 真机（iOS/Android/蜂窝）路径未验证 | 多端 | phase1-acceptance-checklist | 阶段 7 |
| R8 | 跨端单场景接力（TUI→Web→手机）无自动化 | 多端 | §三 | 阶段 7 |

安全边界本身（路径约束、审批先于执行、敏感路径、进程树取消、loopback-only Web、
手机远程审批限制）在本次审计中均有测试证据，未发现新的绕过路径。

## 七、本阶段新增的基线测试

`crates/leveler-engine/tests/phase0_baseline_test.rs`：

1. `tool_side_effect_can_precede_durable_tool_call_started` —
   用 gated `EventStore`（确定性故障注入，测试 fake，非生产 Mock）复现崩溃窗口，
   并同时锁定 happy path 的事件顺序（started < finished）。阶段 1 完成时必须反转断言。
2. `blocked_goal_collapses_to_failed_at_the_task_level` —
   锁定 blocked 的三层现状：agent typed / task 塌缩 / turn Debug 字符串。

其余行为特征（工具顺序、审批先于执行、取消传播、resume 不丢消息、goal
complete/blocked、子 Agent 归属、多端隔离）经盘点已有既存测试锁定（§三矩阵），
不重复补写。

## 八、对 TUI / Web / 手机端的影响

无。本阶段只新增测试与文档，无行为性生产代码改动，客户端协议与事件语义不变。

## 九、检查门槛现状（本阶段实测）

审计时发现 HEAD 本身未过仓库规定的完整检查（此前提交未跑门槛）：

- `cargo fmt --all -- --check`：26 个文件存在格式漂移（纯空白/换行）。已用
  pinned toolchain（1.90.0）统一格式化，无语义改动。
- `cargo clippy -D warnings`：两处既有错误——
  `leveler-execution/src/approval.rs:374`（needless_borrow）、
  `leveler-tui/src/reducer/mod.rs:469`（unnecessary_cast）。各修一行，无行为变化。

修复后三项门槛全绿（测试结果见阶段汇报）。
