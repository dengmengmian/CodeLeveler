# TUI / WebUI Alignment Audit（Phase 1）

日期：2026-08-18。基线 commit：4b83bd2。
本文是收口工程的事实基线：先钉住"现在是什么"，Phase 2-6 的每一项修复都对应这里的一条。

## 1. 协议现状（Rust 是事实源）

- `ClientCommand`：`crates/leveler-client-protocol/src/command.rs:19`，34 个变体。
- `RuntimeEvent`：`crates/leveler-client-protocol/src/event.rs:39`，55 个变体。
- `UiSessionSnapshot`：`crates/leveler-client-protocol/src/snapshot.rs:244`。
- 已有 schema 管线：`schemas/*.schema.json` 由 `schema_export.rs` 测试守护
  （`UPDATE_SCHEMAS=1 cargo test -p leveler-client-protocol --features schema` 再生）。
  Rust→schema 漂移已经是 loud 的；**schema→TS 这一段完全缺失**，TS 靠手抄。

### 关键协议事实

| 事实 | 位置 |
| --- | --- |
| `SetProductAxes { session_id, work_profile, collaboration }`，wire 值 economy/balanced/delivery × chat/plan/goal | command.rs:82 |
| 产品轴 SoT = session record（DB），`SetProductAxes` 持久化后发 `SessionUpdated` | interactive.rs:1482 |
| collaboration=goal 时 runtime 把普通 `SubmitMessage` 直接路由成 goal turn | interactive.rs:161 |
| collaboration=plan 强制 Safe-only 工具（read_only），与权限档正交 | session.rs:586 |
| snapshot 已带 runtime 决议后的 `reasoning.effective`（§11 不需要扩协议） | snapshot.rs:293 |
| snapshot **不带**产品轴 → 客户端刷新/重连后轴丢失（TUI 亦然，靠本地态硬撑） | snapshot.rs:244 |
| 7 个 turn 终态 + 2 个稳定 reason token（`no_code_changes` / `no_automatic_verification`） | event.rs:20-23,171-187 |
| `ApprovalDecision` 是 4 档（含 `approve_always`） | wire_types.rs:14 |

## 2. TUI 消费面（对齐参照系）

`crates/leveler-tui/src/reducer/runtime_apply.rs` 全量 match、无 `_ =>` 兜底，55/55 事件全消费。
关键语义（web 必须复刻的部分）：

- **终态**：`TurnEndStatus` 7 值（transcript.rs:63）。措辞（render/transcript_lines.rs:299）：
  Completed/Answered=`✓ 任务已完成`；Truncated=`⚠ 已完成，但有警告`；
  Incomplete=`⚠ 未完成`（detail 以 `failed gate(s)` 开头时改成 `验证未通过`）；
  Unverified+`no_code_changes`=`◇ 结束 · 未改仓库`（平静，成功色）；
  Unverified+`no_automatic_verification`=`✓ 完成 · 未自动验证`（平静，成功色）；
  其他 Unverified=`⚠ 已完成，但有警告`；Failed=`✗ 失败`；Cancelled=`⊘ 已取消`（次要色，非失败）。
  reason/detail 永远保留在终态标记上（runtime_apply.rs:254-263 特意清掉重复 notification）。
- **进度**：`CommandProgress`→activity=`运行 {label} · mm:ss`；`TurnProgress` closing→`收口中 · {phase}`、
  streak→`无进展 ×N · {phase}`（runtime_apply.rs:108,229）。
- **子 agent**：`SubAgentUpdated/Progress/Activity` → 每 agent 一个块，running→done 原地更新，
  带 nickname/role/detail/token 用量/最近一步（transcript.rs:106）。
- **memory**：`MemoryList` 含 pending（K36 同意门）；accept/forget 命令用户权威。
- **reasoning**：`ReasoningDelta` 累积，工具调用把当前思考标记 superseded，下一条 delta 替换。
- **上下文**：`ContextUpdated.estimated_tokens` 只做占位（有真实 usage 后不覆盖）；
  compact/expand 是结构化事实，客户端管措辞。

## 3. Web 漂移清单（修复对象）

### P0 协议漂移（`web/src/types/protocol.ts` 手抄层）

| 漂移 | 事实 |
| --- | --- |
| `set_agent_mode { orchestrate }` | Rust 协议根本没有此变体；WS 网关回 `invalid_frame`，UI 乐观置位 → `/mode`、DIRECT/PLAN 切换**静默失效**。只在 mock（server.mjs:381）里"能用" |
| `AgentMode direct/plan` | 旧语义，与 `SetProductAxes`（work × collab）冲突，删除 |
| 缺命令 | `steer_current_turn` `add_attachment_data` `accept_memory` `run_user_shell` `cancel_user_shell` `request_session_list_for`(有)…共 5 个缺失 |
| 缺 snapshot 字段 | `user_shells` `reasoning`（→ Reasoning Effort 无法展示） |
| `UiSessionSummary.repository` 注释称 Rust 没有 | Rust 已有（snapshot.rs:238），注释过期 |
| `memory_list` 缺 `pending` | Rust 有（event.rs:246） |
| 事件缺 `project_rules_loaded`(有)/`assistant_attempt_reset`(有)；缺 `runtime_ready`(有) | 覆盖但个别字段漂移 |

### 终态压扁（正确性）

controller.ts:230：7 终态压成 completed/failed/cancelled；
`turn_incomplete`、`turn_completed_unverified`、`turn_answered` 全部显示为「已完成」，reason 丢弃。
`turn_truncated` 被并进 failed。

### 事件丢弃（controller.ts:224 default 分支）

`reasoning_delta` `sub_agent_updated/progress/activity` `user_shell_*` `background_task_*`
`memory_list` `context_updated/compacted/expanded` `command_progress` `turn_progress`
`project_rules_loaded` `notification(info)` — 共 16 类。

### 交互缺失

- 产品轴：无 work-mode / collab 入口（被伪 `/mode` 占位）。
- `/memory` 发了 `list_memory` 但结果无人接（空操作）；无 accept/forget。
- 无 force cancel；无删除会话（仅归档）；`session_completed` 存了没人读。

### 结构重复

顶部 stage 有「对话|改动」，Inspector 又有「任务|改动|验证|历史」——改动出现两份。

## 4. 收口决定（Phase 2-6 的设计基线）

1. **Protocol SoT 链**：Rust types →（既有 schemars 测试）→ `schemas/*.schema.json` →
   **新增** `web/scripts/gen-protocol.mjs` → `src/types/protocol.gen.ts`（提交入库）。
   `npm run typecheck` / `build` 前置 `--check`：schema 变了而生成文件没变 → 红。
   web 网关自有帧（UpFrame/DownFrame/ProjectInfo/REST DTO）留在手写 `protocol.ts`，
   它们的事实源是 leveler-web，不在 client-protocol schema 里。
2. **最小协议扩展（唯一一处）**：`UiSessionSnapshot` 增加
   `work_profile: Option<String>` / `collaboration: Option<String>`（additive，缺省不序列化）。
   理由：轴的 SoT 在 session record，runtime 路由行为依赖它（goal 路由、plan 只读），
   但 snapshot 不携带 → web 刷新后必然显示假轴。TUI 同步改为优先采纳 snapshot 值（修重连漂移）。
3. **删除**：`set_agent_mode` / `AgentMode` / `orchestrate` 全部清理（web + mock），不留兼容层。
4. **终态**：web 保留 7 值 `TurnOutcome`，reason/detail 永不丢；措辞对齐 §2 的 TUI 规则
   （含两个软 token 的平静收场——不是所有 unverified 都要黄牌）。
5. **Multi-agent**：复用 Inspector→任务 tab，agent 树（running/done/failed + activity + tokens）；
   background task 作为运行区活动行，不建新页面。
6. **进度**：`command_progress`/`turn_progress` 复用 activity 槽（与 TUI 同一规则）。
7. **memory**：Inspector 新增「记忆」tab：active/pending/archived + 接受/遗忘。
8. **Reasoning**：只展示 `snapshot.reasoning.effective`（协议无 set 命令，不发明旋钮）。
9. **Inspector 去重**：右栏改「任务|验证|历史|记忆」，任务 tab 的改动摘要点击→切中央 Changes。
10. **Queue 保留**，不改成 Steer（两个能力，本次不动默认）。

## 5. 不做（本次范围外，含理由）

- `!command` / user_shell 的 web 渲染：协议类型随 codegen 进入 TS，但 web 执行入口不做
  （浏览器场景的 shell 转发安全边界需要单独设计，不塞进对齐批次）。
- Context Picker（@ 上下文）：只接事件（meter/通知），picker 后续。
- `/skill` `/editor` `/paste` `/remote` 等 TUI 终端特有命令：不搬。
- mockup.html 的 SetAgentMode 对照表行：静态设计稿，随 P0-2 一并订正一行，不重做稿子。
