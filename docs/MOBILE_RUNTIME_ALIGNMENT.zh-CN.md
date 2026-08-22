# CodeLeveler Mobile Runtime Alignment Plan v1.0

**Status:** M8–M11 + `fetch_attachment` 已落地，随后 **FROZEN**（[`MOBILE_FREEZE.zh-CN.md`](MOBILE_FREEZE.zh-CN.md)，tag `mobile-beta-mvp`）。M12 Push 未做。Agent 写仓库文件仍不会自动变成 attachment。  
**Scope:** `apps/leveler-mobile` 在 M1–M7 之后，对齐 Runtime、能控制、能收产物  
**不做什么:** 换栈、重写、新 Runtime 能力、手机 IDE、Always allow、本阶段实现 Push  

对照：[`MOBILE_UI_UX_CLOSURE.zh-CN.md`](MOBILE_UI_UX_CLOSURE.zh-CN.md) · `leveler-client-protocol` · `leveler-remote-protocol`

> 手机创建任务 → Agent 执行 → Timeline 同步 → 手机干预 → 产物回传。  
> 这才是第二客户端，不是第二套聊天。

---

# 1. M1–M7 状态评价

| 项 | 评价 |
| --- | --- |
| 文案任务化 | 做到。Session 模型未拆，正确。 |
| Home 工作台 | 做到。运行中任务依赖**已打开过的项目缓存**，跨项目实时仍弱。 |
| Timeline | 做到。`transcript` 仍双写，golden 未破。 |
| Tool / Turn / Plan / Attachment 行 | 事件已进 timeline；Attachment **只有文件名行，不能预览/下载**。 |
| 气泡去掉 | 做到。composer 还在。 |
| 控制闭环 | Running 时 composer 走 `steer_current_turn`；idle 仍走 `submit_message`。 |

结论：看起来像控制端，用起来还差两刀——**中途改方向**和**拿走结果**。这就是 M8–M11。

现有资产必须保留：配对、验签、allowlist、只读、`command_id` 重发、审批/澄清钉底、`ChangeNotifier` 栈。

---

# 2. M8 Event Coverage Audit

权威事件在 host：`RuntimeEvent`（`crates/leveler-client-protocol/src/event.rs`）。手机只投影。

Wire 上**没有** `turn_started`（现有注释：用户消息落地即开始）。产品表里的 TurnStarted = `user_message_added` 把 `status` 设为 `running`。

## Coverage matrix（对照 `session_state.dart`）

| 产品名 | Wire `type` | 现在 | 备注 |
| --- | --- | --- | --- |
| UserMessage | `user_message_added` | Timeline 引用条 | 本地 steer 去重 |
| AssistantMessage | `assistant_message_*` | 全宽 Markdown | |
| TurnStarted | （无独立事件） | StatusChip / running | 不发明 wire |
| TurnCompleted | `turn_*` | 状态行 | 文案分未验证/失败 |
| ToolCallStarted | `tool_call_started` | 「读取文件」+ path | 不再堆生 JSON |
| ToolCallCompleted | `tool_call_completed` | 完成/失败 + preview | |
| PlanUpdated | `plan_updated` | 步骤标题 | TaskHeader `done / total` |
| AttachmentAdded | `attachment_added` | Artifact 卡片 | 预览页有；下载等 B3 |
| ApprovalRequired | `approval_requested` | 钉底卡 + 行 | |
| StatusChanged | `session_updated` + `turn_*` | TaskHeader | |
| SubAgentStarted/Completed | `sub_agent_updated` | 同一行更新 | |
| SubAgentProgress/Activity | `sub_agent_progress` / `_activity` | 更新同行，不刷屏 | |
| Thinking | `reasoning_delta` | 可折叠，默认收起 | 不进 transcript |
| Diff | `diff_updated` | 「工作区变更」摘要行 | 不当聊天 |
| Verification | `verification_updated` | 状态行 | |
| SessionCompleted | `session_completed` | 任务完成/失败条 | |
| Token/progress noise | `token_usage`, `turn_progress`, `command_progress` | 故意忽略 | 不是丢能力 |

**仍故意静默：** token 计数、context 折叠、memory_list、btw_*、checkpoint。未知事件：计数 + Settings，不渲染谜题行。入 Settings 未知计数只会吓人。

## Renderer 原则

1. 能改变用户决策的事件必须有行或芯片，禁止只进 `_ignored`。  
2. 高频心跳（progress、token）禁止每条占一行。  
3. 未知事件：计数 + resync，不渲染谜题行。  
4. 不在手机上造第二套 EventLog。

### Commit M8

| id | 范围 | 可运行验收 |
| --- | --- | --- |
| **A1** | Coverage 表进测试：每种关键 `type` 要么 timeline/芯片，要么在「故意忽略」白名单并写理由 | `flutter test` |
| **A2** | `reasoning_delta` → 可折叠 Thinking 行 | 默认不展开 |
| **A3** | `sub_agent_updated` → 一条「子 Agent {nickname} · {role} · {status}」 | 忽略 progress 刷屏 |
| **A4** | `verification_updated` / `session_completed` → 状态行，不进未知 | 完成任务看得到结束 |

---

# 3. M9 Artifact First Class（优先）

这是 Mobile 存在的核心理由之一：**结果回到手上**。不做文件管理器。

## CURRENT 事实（不要装成已有下载通道）

- `AttachmentRef`：`id, kind, name, mime_type, size_bytes, sha256, width, height`。字节在 host **media store**，客户端只拿引用。  
- 事件：`attachment_added`。手机已画文件名行。  
- 远程：**禁止** `AddAttachment` / `AddAttachmentData`（防手机点本地路径）。上传走 `upload_attachment` RPC（测试有，App 未接）。  
- **下行下载 RPC 今天不是产品表面。** M9 分两刀：先把已有引用画成卡片；下载/分享等 host 提供按 `sha256` 取字节的远程 RPC。

Agent 写进仓库的 `review-report.md` **不是**自动 `attachment_added`。那是 workspace 文件。产品上「Generated」要 host 把交付物登记为 attachment/artifact 事件。手机不扫仓库。

## 产品模型（投影，不是新库）

```text
Artifact
  id              AttachmentId
  session_id
  task_id         // CURRENT = session_id
  name
  type            markdown | diff | image | archive | other  // 从 mime/扩展名
  size
  sha256          // 取字节的钥匙
  preview         // 可选截断
  created_at
```

不要 `download_url` 当公开 HTTP。通道仍是 **签名 RPC**，与配对同一套。

## UI

1. **Timeline Artifact Card**（替换纯文件名行）  
   图标 + 名 + 大小 + 类型芯片。点开预览。  
2. **Preview**（新页，push）  
   md → Markdown；diff/patch → 等宽；图 → 全宽；zip → 元数据 + 分享，不解压。  
3. **Share**  
   系统 Share sheet。下载失败时卡片保留、错误用现有 `StatusBanner` 声。

不做：全库浏览、多选删除、当 Files.app。

### Commit M9

| id | 范围 | 依赖 |
| --- | --- | --- |
| **B1** | `TimelineKind.attachment` 改卡片；mime → 类型 | 无 host 新能力 |
| **B2** | Preview 页（有字节才渲染；先支持测试夹具/内存字节） | 无 |
| **B3** | host：`fetch_attachment { sha256 }`（或等价）经 remote-agent | **Runtime/remote 小补丁**；若本阶段冻结 Runtime，B3 停在接口文档 |
| **B4** | 接 B3：下载后预览/分享 | B3 |

B1–B2 可先合、先演示。B3 是唯一可能碰到 Runtime 的点；**没有 B3 不要假装能收仓库里的任意文件。**

---

# 4. M10 Human Steering（优先，且 host 已就绪）

Web 审计：Steer **PARTIAL**——线上有 `SteerCurrentTurn`，Web 还在排队 follow-up。Remote policy **已 Allow** `steer_current_turn`（interactive 配对）。

手机 `Commands` **没有**这个 builder。Running 时点发送仍是 `submit_message`（回合结束后才进下一轮）。这就是「不能中途改方向」。

## 场景

```text
Agent: 准备改数据库结构
用户:  保持 API 兼容，不要改库
Runtime: SteerCurrentTurn → 下一 round 顶部注入
```

这不是新 Event 名叫 Human Intervention。权威仍是 **ClientCommand::SteerCurrentTurn**。不要另造 Intervention 总线。

## UI

Composer 保留。Running 时：

- 提示从「追加要求」改为「干预当前回合（立刻生效）」  
- 发送走 `steer_current_turn`  
- idle 仍走 `submit_message`  
- 时间线仍显示为 User 行（用户说过的话），不发明第三种泡  

只读配对继续藏 composer。

## API

已有：

```json
{ "type": "steer_current_turn", "session_id": "…", "content": "…" }
```

Ack / `command_id` 重发规则与 submit 相同。

### Commit M10（建议第一个做）

| id | 范围 | 验收 |
| --- | --- | --- |
| **C1** | `Commands.steerCurrentTurn` | 单测 JSON 与 schema 一致 |
| **C2** | `status==running` → steer，否则 submit | 现有发消息测试仍过 |
| **C3** | 提示文案随状态变 | 肉眼：运行中不是「等它说完」 |

**不破坏聊天：** idle 行为与今天完全一样。

---

# 5. M11 Task Workspace

用户看见的是 Task。CURRENT 仍是 Session 1:1 Task，**不拆存储**。

不要先做独立大页抢 `SessionState`。两步：

**M11a（现在）：** 时间线顶 `TaskHeader`  
目标、StatusChip、当前 `activity`、计划步骤数、产物数（timeline 里 attachment 条数）。点 Header 暂不跳转。

**M11b（稍后）：** 同一 `SessionState` 上的 Task Detail 路由（push）：Header + 精简 timeline + Artifacts 列表 + 钉底审批。返回仍回时间线。禁止第二份事件缓存。

Progress 百分比：**不要假造。** 有 `plan_updated` 才用 completed/total steps；否则只显示阶段芯片。

### Commit M11

| id | 范围 |
| --- | --- |
| **D1** | `TaskHeader` 嵌在 `ChatScreen` 顶 |
| **D2** | 步骤完成数（仅当 plan 在） |
| **D3** | 可选 Detail 路由，共享 SessionState |

---

# 6. M12 Push（只规划，不实现）

今天：MISSING。Remote ≠ 推送。

未来（Beta 后）：

| 用户该被拍醒 | 触发（host 事实） |
| --- | --- |
| 需要确认 | `approval_requested` / `clarification_requested` |
| 任务失败 | `turn_failed` / `session` failed |
| 任务完成 | `session_completed` / 终态 turn |
| 有产物 | `attachment_added`（交付物） |

通道：APNs / FCM。Relay 或 host 边车发，**手机不轮询 EventLog**。Payload 只含 `session_id` + 类型；点进已有配对栈。Observe-only 仍可推「需要确认」但打不开批准。本阶段零代码。

---

# 7. Flutter 页面调整（不换架构）

```text
配对 → Home → 项目任务列表 → Timeline(+ TaskHeader)
                              └ Preview (M9)
```

- 不加 Chat Tab。  
- 不加 IDE。  
- `go_router` 非必须。  
- 新文件：`ui/task_header.dart`、`ui/artifact_card.dart`、`ui/artifact_preview.dart`。  
- `Commands` 只加 `steerCurrentTurn`。

---

# 8. 数据模型 / API / Event

| 层 | 规则 |
| --- | --- |
| SessionState | 继续双写 transcript + timeline；加 `List<ArtifactRef>` 从 attachment 事件填 |
| Task | UI 词；id = session_id |
| Artifact | 投影 AttachmentRef；下载走签名 RPC，不公开 URL |
| Intervention | 不是新表；就是 Steer 命令 |

API：配对 REST + 会话 WSS 不变。新增需求只有 **steer（已有命令）** 和 **fetch_attachment（host 可能要补）**。

Event：继续用 `RuntimeEvent.type`。产品名只是文档别名。

---

# 9. 推荐开发顺序

价值：先能改方向，再能拿文件，再补齐时间线，再工作台头，Push 只写文档。

```text
M10 C1–C3   Human Steering     ← host 已 Allow，最小闭环
M8  A1–A4   Event coverage     ← 别再静默丢 sub-agent / thinking
M9  B1–B2   Artifact 卡片+预览 ← 无 host 补丁也能看形态
M11 D1–D2   TaskHeader
M9  B3–B4   下载 RPC           ← 唯一可能动 Runtime
M11 D3      Task Detail 路由
M12         Push 文档 only
```

每个 commit：配对、发消息、审批仍绿。`flutter test` 过。

**本阶段明确不做：** 重写、RN、编辑器、Terminal、Always allow、假进度条、公开 download URL、实现 Push。
