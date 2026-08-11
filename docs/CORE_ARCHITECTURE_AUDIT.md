# Core Architecture Audit

## Baseline

`e6edbb8`（BASELINE_SHA，workspace clean）。Core Architecture Hardening 改前审计。
全部结论来自当前代码逐点核对（file:line 已验证），不是文档转述。

## Current Dependency Map

客户端分层**已经干净**：

- leveler-tui / leveler-web / leveler-remote-agent / leveler-session-wire 均**不依赖** leveler-engine，只依赖 leveler-client-protocol。
- leveler-engine **不依赖** leveler-client-protocol（event.rs:14 明示不变量）；engine 侧已有 `public_projection()`（event.rs:536，穷举 + 默认拒绝）作为远程边界 DTO 范式。
- 只有 leveler-app 与 leveler-cli 同时看到两侧 —— **leveler-app 就是投影缝**，Phase 1 无需新 crate。

## Event Flow（实测主路径）

```
Executor 循环 (leveler-agent)
  → AgentEvent（25 变体；From<AgentEvent> for EngineEvent 全射 1:1, event.rs:805）
  → TurnRunner::forward → EngineEvent（47 变体）
  → turn pump → EventLog::append（log.rs:58 —— persist-before-forward 唯一转发点；
     TurnFinished 与查询投影原子提交 turn.rs:422；PumpBarrier 同队列保证工具执行前事件已持久化）
  → observer(EngineEvent)
  → 【SHIM】forward_engine_event → engine_event_to_agent（session.rs:93）
      —— 47→25 有损降级：24 个变体经未命名 `_ => return None`（session.rs:223）静默丢弃
  → EventBridge::forward（event_bridge.rs:143，AgentEvent→RuntimeEvent，
     持 per-turn 状态：tool_starts 时长表 / open_assistant 消息 id / verification_checks
     / 近重复折叠 FOLD_*）
  → broadcast<RuntimeEvent>（per-session cap 2048）→ TUI/Web
```

结构性事实：

- shim 与 `data_class`/`public_projection` 不同，**不是穷举 match**——新增 EngineEvent
  会静默从 UI 消失，无编译期强制决策。
- **A→E→A 往返非恒等**：`ContextExpanded`/`ContextSnapshot` 只有去程；EventBridge 的
  ContextExpanded 通知臂（event_bridge.rs:288）在主路径上是**死代码**。
- UI 为补偿 shim 丢弃需要 3 条旁路：审批走 ChannelApprover、turn 终态走
  `turn_runtime_event(AgentOutcome)`、eval 在 shim 前手工拦截（eval_signals.rs:343-368，
  eval_cmd.rs:1255 注释自证丢弃集是已知负担）。
- shim 调用方：session.rs 3 处（run/chat/resume，UI 主路径）+ eval_signals.rs:369（eval）。
  其余 AgentEvent 消费者：CLI headless render（run_cmds.rs:75/1071）、parallel 丢弃器。

## Tool Execution Flow / Metadata

- `Tool::name/description -> &'static str`（tool.rs:262/265），66 个内建实现站点；
  Registry key `BTreeMap<&'static str, Arc<dyn Tool>>`（registry.rs:17，
  `insert(tool.name(), ...)`）。
- **McpTool 用 `Box::leak` 永久泄漏**动态 name/description（mcp.rs:248,262-266）。
- 变更半径实测很小：trait 返回 `&str`（借 self）+ registry key 改 owned + McpTool
  持有 String 三点即可；内建工具返回 `&'static str` 自动协变，零改动。

## ToolContext（23 字段，生命周期分类实测）

| 生命周期 | 字段 |
| --- | --- |
| 进程级不可变 | workspace, runner, environment, mode, deny_env, auto_format, max_files_per_step, artifact_store, memory_root, background_tasks |
| 进程级共享服务（内部可变，跨子 agent 共享 Arc） | checkpoint, read_guard, file_state, command_gate, lsp_sessions, lsp_start_locks |
| per-engine/turn | read_only（engine 构建时定）, tool_output_budget（factory.rs:135 每 turn 重写）, deny_network（turn grant 可清） |
| per-invocation（每次 clone 重建） | command_write_allowlist, command_modified_files_remaining, command_previously_modified, turn_unrestricted_fs |

构造单点：`App::engine_for_with_profile`（lib.rs:428-436）。克隆链：factory 每 turn →
executor 每子 agent（共享全部 Arc）→ drive.rs:1362 每次调用（apply_turn_grants +
write constraints）。安全执法点全部落图（mode/registry.rs:156、read_only/registry.rs:149
先于 mode、turn_unrestricted_fs 与 deny_network 是仅有的两个**直接字段赋值**放权点，
分散在 injected_tools.rs:319 与 host.rs:211——无结构性收口）。

侧发现：command_gate 的 doc 注释物理上挂错位置（tool.rs:47-58，LSP 注释串味）。

## Client Command Flow / Application Ownership

`InProcessRuntimeClient`（interactive.rs 2739 行 / 非测试 2397）：13 个字段（9 个可变
共享 map）、3 个 trait、33 个 ClientCommand 变体全部内联处理。职责簇实测：

| 簇 | 规模 | 备注 |
| --- | --- | --- |
| Checkpoint（两 map 一致性 + git 快照生命周期） | ~230 LOC | 最清晰的提取候选；Arc 只为 detached compact worker 服务 |
| Context ops（clear/compact/restore + task epoch） | ~330 LOC | 自有"部分失败不毁历史/epoch 写序"不变量；**compact_conversation:2291 手搓第二份 UiSessionSnapshot（pending/live view 全空）——与 snapshot() 契约漂移** |
| deliver 命令信封协议（指纹/receipt/版本栅栏/会话栅栏） | 156 LOC | 纯协议中间件，只碰 pending 两字段 |
| Session 目录（list/CRUD 9 臂 + placeholder 标题策略） | ~194 LOC | *For 成对重复 |
| Session runtime config（缓存+write-through） | ~180 LOC | 3 条非原子 DB 写 |
| Live view / snapshot 投影 | ~190 LOC | update_live_view 已是纯 reducer 带测试，只差没成型 |
| Turn launcher（5 个 spawn + 3 处重复的 25 行前奏） | ~183 LOC | |
| Media 摄取 ×3 / Memory ×3 | 100/131 LOC | 各三份近重复体 |

不变量已核：单会话单主 turn（ActiveTurns cap 4）、context ops 复用 admission、
审批绑定栅栏、at-least-once 去重的诚实 Uncertain、乐观版本栅栏、checkpoint 0 位拒绝、
epoch 写序。侧发现：**删除/归档会话不清理 6 个 per-session map（含 per-session
forwarder task 泄漏）**；**ForceCancelCurrentTurn 与 Cancel 字节相同（无升级语义）**；
steering 队列不清理可能把旧文本漏进后续 turn。

## Client Event Presentation Debt

- **B 类（稳定产品事实被硬编码为中文/英文散文）**：turn_runtime_event 的 5 条默认
  reason（event_bridge.rs:28-49）；Compacted/ContextExpanded 通知（:283/:291）；
  AgentActivity 全部 8 个标签（:320-372 + interactive.rs:2227 同概念异文案）；
  interactive.rs 的 fork id/记忆采纳/压缩成功(带✓)/部分回滚等 ~10 处；
  session.rs:258 与其兄弟的 REASON_* token 用法不一致；engine.rs:1209 唯一引擎侧泄漏；
  协议 crate 里的中文常量 `COMPACTION_SUMMARY_PREFIX`（lib.rs:86，被 TUI 前缀匹配）。
- **A 类（自由诊断，保留 String 合理）**：notify_error 全部调用点、传输错误透传、
  模型/工具输出载荷。
- **实 bug**：TUI 以子串嗅探本地化 reason（transcript_lines.rs:399 匹配
  `"budget exhausted"` 带空格），而 budget.rs:64 发的是 `budget_exhausted` 下划线——
  **永不命中**，预算截停会把裸机器串render给用户；`"no-progress streak"` 前缀同样永不匹配
  engine.rs 的 `"continue suppressed: no-progress cap"`。
- 半 token 化设计：closeout_reason=goal_unresolved; + 中文句子拼接（closeout.rs:64）。

## Wire Compatibility 机制（Phase 5 的约束面）

- PROTOCOL_VERSION 1.3，major-only 兼容判定；schema_export（CI --all-features 已跑）+
  event_transcript_golden（Rust/Flutter 双端字节级）+ ~30 roundtrip + session-wire 7 golden。
- **缺口**：`parse_runtime_event`（unknown-variant 跳过机制）是**死代码**——真实传输
  用 envelope 硬解，旧客户端遇新增变体是解码错误→静默丢帧（local-transport lib.rs:1017
  只 log）；Web 的 protocol.ts 手工镜像**已漂移**（缺 command_progress/
  sub_agent_activity/memory_list.pending），仅靠 switch default 不崩；minor 版本
  bump 无 CI 强制。
- 附加规则实测：新增变体需 UPDATE_SCHEMAS=1 + UPDATE_GOLDEN=1 再生成，测试失败信息
  自带命令。Flutter 端有 unknown-kind 允许列表的历史事故记录。

## Confirmed Architecture Debt（审计门判定）

| # | 债务 | 判定 |
| --- | --- | --- |
| A | transitional event projection（E→A→E 有损 shim + 兜底 `_ => None`） | **EXISTS** |
| B | dynamic tool metadata（`&'static` + MCP Box::leak） | **EXISTS** |
| C | ToolContext 增长热点（23 平铺字段、放权点无收口、doc 错位） | **EXISTS** |
| D | InProcessRuntimeClient 所有权热点（2.4k LOC/33 臂/9 可变 map/多处契约漂移） | **EXISTS** |
| E | 跨客户端预格式化文案债（B 类清单 + 实 bug 两处） | **EXISTS** |

无一项 ALREADY RESOLVED。

## Change Radius / Proposed Minimal Slices

| Slice | 内容 | 半径 |
| --- | --- | --- |
| 1 | 投影等价基线测试（现行 shim+bridge 行为特征化） | leveler-app tests |
| 2 | `EngineEvent → RuntimeEvent` 单一投影（穷举 match，效仿 public_projection；吸收 EventBridge per-turn 状态；shim 降级为 LEGACY-only：CLI render/eval 两个调用方） | leveler-app（session/event_bridge/interactive）+ eval_signals 标注 |
| 3 | Tool metadata：trait `&str` + registry owned key + McpTool 去 leak + 动态注册测试 | leveler-tools 3 点 |
| 4 | ToolContext facets 按上表生命周期分组 + 放权点收口 + anti-growth 规则 + doc 错位修复 | leveler-tools + agent/app 构造点 |
| 5 | InProcessRuntimeClient：按簇提取（checkpoint / context-ops / deliver 中间件 / live-view+snapshot 优先），facade 保留 | leveler-app 内部 |
| 6 | 结构化事件：高置信事实（compacted/expanded/turn-incomplete reason 码/advisory 标签）additive 迁移 + 修 TUI 嗅探 bug + schema/golden 再生成 | protocol(additive)+app+tui 本地化 |
| 7 | 文档 + 终报 | docs |

## Explicit Non-Goals

不实现 !command/Capability/Extension/NPC/APP；不新建 crate；不改持久化 schema 与既有
wire 变体名；不动 ExecutionKind/ClientOrigin 词汇；不重开 TUI 架构；side findings
（map 泄漏、ForceCancel 语义、protocol.ts 漂移、parse_runtime_event 死代码）只登记，
是否本轮修由各 Phase 范围决定，不擅自扩大。
