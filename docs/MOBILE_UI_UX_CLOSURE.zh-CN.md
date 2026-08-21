# CodeLeveler Mobile UI/UX Closure Plan v1.0

**Status:** TARGET 产品文档 · 不描述为已实现  
**Scope:** `apps/leveler-mobile` 的 UI/UX 收口  
**原则:** 先读现有代码、不推翻已有实现、不扩大量新功能、数据模型向 Runtime 靠拢  

> 随时随地管理你的 AI Coding Agent。  
> CodeLeveler Mobile lets you create, monitor, control, and receive results from your AI coding agents anywhere.

本文是 **UI/UX Closure**，不是新 App 设计。配对、验签、relay、allowlist、只读配对、会话栈已经是产品资产。要改的是：**屏幕讲的是聊天，还是工作台。**

对照：

| 层 | 权威 |
| --- | --- |
| Runtime / 协议 | [`ARCHITECTURE.md`](ARCHITECTURE.md)、`leveler-client-protocol` |
| 远程桥 | `leveler-remote-agent` + `leveler-relay`（CURRENT） |
| 未来运行时 | [`FUTURE_RUNTIME_ARCHITECTURE.md`](FUTURE_RUNTIME_ARCHITECTURE.md) |
| 现有 App | `apps/leveler-mobile/` |
| Web 远程就绪 | [`WEBUI_V2_FINAL_PRODUCT_CLOSURE.md`](WEBUI_V2_FINAL_PRODUCT_CLOSURE.md) § Mobile |

---

# 第一部分：现有代码分析（Current State Review）

## 技术架构

Flutter（Dart ≥3.6），iOS/Android。**没有命名路由**：`main.dart` 的 `_Root` 按 `AppController` 标志切屏。

```
PairingScreen
    ↓ isPaired
ProjectsScreen
    ↓ currentProjectId
SessionsScreen
    ↓ session != null
ChatScreen
```

设置从各页 `settingsButton` push。状态：单个 `ChangeNotifier`（`AppController`）+ 打开会话时的 `SessionState`。网络：`RelayClient`（REST 配对/项目）+ `SessionSocket`（WSS，**先验签再进 UI**）。协议：`envelope.dart` 与 Rust 共用 `testdata/signed_envelope.golden.json`。

## 页面列表（CURRENT）

| 文件 | 职责 |
| --- | --- |
| `ui/pairing_screen.dart` | 粘贴配对载荷、指纹对照、等主机确认 |
| `ui/projects_screen.dart` | 主机上的项目列表（在线/离线） |
| `ui/sessions_screen.dart` | 某项目的会话列表 +「新会话」对话框 |
| `ui/chat_screen.dart` | 气泡 transcript、审批卡、澄清卡、composer、取消回合 |
| `ui/settings_screen.dart` | 主机/relay/指纹/只读、未知事件、解除配对 |

没有 Home、没有独立 Task、没有 Artifact、没有命名 Router。

## 已实现能力（保留）

- 配对（粘贴；扫码未做）、只读 vs 交互
- 多项目列表 + 离线项目灰显不消失
- 会话列表、创建会话（goal 对话框）、打开会话
- `submit_message`、`cancel_current_turn`
- 审批（一次 / 本会话 / 拒绝；**没有 Always allow**，与 host 拒绝一致）
- 澄清
- 连接横幅、resync、未知事件计入设置
- 未确认命令按原 `command_id` 重发（上限 16）

未做（README 已诚实记录）：附件上传、产物下载、推送、Steer、真机蜂窝、Android 本机构建。

## UI 风格

`ui/theme.dart`：种子色 `0xFF2F6FEB`（对齐桌面蓝）、近中性表面、细线 AppBar、无 tint 卡片。方向对。

`ChatScreen` 仍是 **聊天气泡**：用户右对齐色块、助手左对齐 Markdown、`reverse: true` 的 `ListView`。工具调用只写进 AppBar 下的一行 `activity` 字符串。空会话图标是 `chat_bubble_outline`。

## 数据结构

`SessionState`：

```text
transcript: List<TranscriptEntry>   // {id, role, text}
approvals / clarifications          // 独立 map
status / activity / goal
unknownEvents
```

`applyEvent` 已识别 `user_message_added`、`assistant_*`、`tool_call_started|completed`、`approval_*`、`clarification_*`、`turn_*`。

**故意忽略（`_ignored`）**、注释写明「将来该有 UI」：

`reasoning_delta` · `plan_updated` · `verification_updated` · `diff_updated` · `attachment_added` · `sub_agent_updated|progress|activity` · `checkpoint_created`

也就是说：**协议已经在推 Agent 事实，手机把它们压成气泡和一行 spinner。**

`Commands` 没有 `steer_current_turn`、`run_goal`、`add_attachment_data`（host 协议里都有）。

## 优点

1. 安全模型正确：验签失败不进 UI；私钥在 Keychain；allowlist 在客户端第二道闸。
2. 下游开放：未知事件 resync，不因 host 升级拆连接。
3. 会话 1:1 任务的 CURRENT 现实没有假装成另一套对象。
4. 审批/澄清已经是工作台式中断，不是聊天附件。
5. 主题已经在往专业工具靠，而不是默认 Material 紫雾。

## 问题（产品，不是工程质量）

1. **信息架构是 Chat App：** 项目 → 会话 → 气泡。没有「所有 Agent 现在怎样」。
2. **时间线被压扁：** tool / plan / diff / attachment / sub-agent 不进 transcript。
3. **会话 ≠ 用户说的「任务」：** UI 写「新会话」，用户想的是「开一个 Coding Task」。
4. **没有跨项目状态面：** 打开 App 必须先选项目才能知道有没有 Running。
5. **Composer 语义是聊天：** 没有区分「新消息」和「打断正在跑的回合」（Steer）。
6. **产物只进 `_ignored`：** Artifact 是产品核心，代码里已标明缺口。

**结论：** 不要重写 Flutter、不要换协议、不要换 `AppController`。Closure = 同一条栈上，把「会话气泡」换成「任务时间线」，并加一层 Home。

---

# 第二部分：产品重新定位

## 为什么不是 ChatGPT 类 App

ChatGPT Mobile 的对象是**一段对话**。CodeLeveler 的对象是**一台开发机上、某个仓库里、受权限约束的长期任务**。

手机上没有仓库、没有模型、没有沙箱。聊天气泡暗示「这里就是 Agent」。工作台必须暗示「Agent 在你的电脑上跑；你在远程管它」。

不要做：手机代码编辑器、手机 Terminal、VS Code Mobile。那些是生产端。

## 为什么是 AI Coding Agent Mobile Workspace

职责（按优先级，**本 Closure 只把 1–5 做成像工作台**；6–7 预留槽位）：

1. 多项目管理  
2. 创建 Coding Task（现有「新会话」）  
3. 查看 Agent 执行状态  
4. 同步会话（升级为 Timeline）  
5. 追加指令 / 中途干预  
6. 接收生成文件（Phase 3）  
7. Approval 深化 + Multi-Agent 展示（Phase 4–5）

一句话：**人离开电脑后，仍能管理自己的 AI Coding Agent。**

## 三端关系

```text
                 CodeLeveler Runtime
            (Engine · EventLog · Agent Loop)
                          |
          --------------------------------
          |               |              |
         TUI           Desktop/Web      Mobile
        生产端            生产端           控制端
```

| 端 | 职责 | 不负责 |
| --- | --- | --- |
| TUI | 主编码界面 | 被关掉就取消任务 |
| Desktop / Web | 同 Runtime 的宽屏生产面 | 第二套 Agent |
| Mobile | 远程工作台：看、令、批、收 | 编辑仓库、跑命令、当 IDE |

规则（已写在未来架构里）：**Task 属于 Runtime，不属于任何一个客户端。** 手机断开 ≠ 取消。

`leveler-mobile` 已经走对了传输：手机 → Relay → `leveler-remote-agent` → Runtime。不要再发明「Mobile Gateway」crate。产品上继续叫这条路径为远程通道。

---

# 第三部分：新的信息架构

**不**上五 Tab「Chat 当首页」。Chat 是会话内时间线，不是一级导航。现有栈保留，只加 Home、改文案。

```text
[配对]                    已有，不动
    ↓
[Home]                    新增：跨项目 Agent 状态
    ├ [项目]              已有 ProjectsScreen
    │     └ [任务]        已有 SessionsScreen，文案改为任务
    │           └ [时间线] 已有 ChatScreen，渲染改为 Timeline
    └ [设置]              已有
```

配对后默认 **Home**，不再默认项目列表。项目 / 任务 / 时间线仍是 push。

可选（Phase 1 末）：底栏 **工作台 | 项目 | 设置**。时间线仍不进底栏。

### Home（新）

- **目标：** 打开 App 十秒内知道「有没有 Agent 在跑、有没有人等我」。
- **场景：** 离开工位解锁手机。
- **展示：** Waiting Actions（审批/澄清）→ Running Tasks → 最近项目。
- **交互：** 点 Running / Waiting 直达时间线；点项目进项目页。数据来自已有 `projects` + 每项目 `sessions` 拉取，**不新协议**。

### 项目（现有）

- **目标：** 选一台主机上的仓库。
- **展示：** `path_display`、在线点、离线灰显。
- **交互：** 在线 tap → 任务列表。保持「离线不消失」。

### 任务（现有会话列表，改名）

- **目标：** 该仓库的工作单元。CURRENT 下 Session 1:1 Task，**不拆表**。
- **展示：** 目标文本、Running / 等待审批 / 空闲、相对时间。FAB：「新任务」不是「新会话」。
- **交互：** 创建 = 现有 goal 对话框；打开 = 现有 `openSession`。

### 时间线（现有 Chat，改渲染）

- **目标：** 看 Agent **做了什么**，不只看它 **说了什么**。
- **展示：** 见第四部分 Timeline。
- **交互：** 底部指令条；Running 时发送走 Steer（Phase 2 接 `steer_current_turn`）；审批/澄清仍钉在 composer 上方。

### 设置（现有）

配对、指纹、只读、未知事件、解除配对。未知事件列表保留——它是「host 比 App 新」的诚实信号。

### Artifacts（Phase 3，不是一级 Tab）

先做时间线里的产物行 + 预览 sheet。根级 Artifacts 库等下载 RPC 存在后再说。

---

# 第四部分：UI 重构方案

## 1. Home

布局（单列，上到下）：

1. 主机名 + 连接状态（复用 `StatusBanner` + `hostName`）
2. **需要你：** 待审批 / 待澄清（空则整块隐藏）
3. **正在运行：** 任务目标、项目名、当前 `activity`
4. **项目：** 最近用过的 3–5 个，带在线点

组件：`StatusChip`（Running / Waiting / Idle）、`TaskRow`、现有 `StatusDot`。不要大卡片瀑布、不要对话预览气泡。

## 2. Projects

`ProjectsScreen` 几乎可留。改动：AppBar 副标题保持主机名；离线行副标题继续「电脑上没有在跑」。不要加仓库树、文件浏览器。

项目详情 = 今天的 `SessionsScreen`（任务列表），不新开第三层「Project Hub」，除非以后要 Artifacts 库。

## 3. Chat → Agent Timeline（重点）

**删掉左右气泡作为主隐喻。** 保留 `ListView` + 流式追加，改 row 类型。

| 行 | 视觉 | 数据来源（已有事件则 Phase 1 就能画） |
| --- | --- | --- |
| User | 左对齐短引用条，不是右侧彩泡 | `user_message_added` |
| Assistant | 全宽 Markdown，无泡 | `assistant_*` |
| Thinking | 可折叠、次要色、默认收起 | `reasoning_delta`（今天在 `_ignored`） |
| Tool call | 等宽工具名 + 一行摘要 | `tool_call_started`（今天只写 activity） |
| Tool result | 成功/失败芯片 + 截断预览 | `tool_call_completed` |
| Status | 居中细线：「回合完成 / 未验证 / 已取消」 | `turn_*` |
| Notice | 已有 `_Notice` | notification/warning/error |
| Artifact | 文件行：图标 + 名 + 开（Phase 3） | `attachment_added` / 未来产物事件 |
| Approval | 已有 `_ApprovalCard`，保持钉底 | `approval_requested` |

参考：GitHub Actions 步骤、Linear Activity、Cursor Agent 时间线。不是 iMessage。

空状态：图标从 `chat_bubble_outline` 换成工作/目标类图标；文案从「说点什么」改为「任务已建立，电脑会开始执行。你可以在下方追加要求。」

AppBar：第一行任务目标（已有），第二行状态芯片 + 当前工具名（把 `activity` 从底栏挪上来，底栏只留 composer）。

## 4. Task Detail

**Phase 1 不新开页。** AppBar + 时间线顶部一条 `TaskHeader`：

- 目标（goal）
- 状态：Created / Running / Waiting Approval / Waiting Input / Idle / Failed（映射现有 `status` + pending maps）
- 阶段面包屑（弱）：有 `plan_updated` 才显示，没有就不要假进度条

独立 Task Detail 路由放到 Phase 2，避免和 Timeline 抢同一份 `SessionState`。

阶段标签（产品文案，不是新 Runtime 枚举）：Planning / Coding / Testing / Completed —— 只能从已有 `plan_updated` / `verification_updated` / `turn_*` **投影**，禁止手机本地猜。

## 5. Artifact

Phase 1：时间线为 `attachment_added` 留文件行占位（点了说「尚未支持预览」也可以，比 `_ignored` 诚实）。

Phase 3：预览 Markdown / Diff / 图；系统 Share sheet 下载。聊天里是行，不是第二个聊天气泡。压缩包只分享不预览。

---

# 第五部分：数据模型调整建议

## 当前

`messages[]`（`TranscriptEntry`）+ 并行的 approvals/clarifications。Tool 不入库。这是 Chat App 模型。

## 目标（兼容，不打断）

在 `SessionState` 增加 **timeline**，不删 `transcript` 直到渲染切完。

```text
TimelineItem
  id
  at            // 若事件无时间戳，用到达顺序
  kind          // user | assistant | thinking | tool | status | artifact | notice
  payload       // 已有 JSON，不复制一份领域对象
```

`applyEvent`：今天更新 `transcript`/`activity` 的分支，**同时** append `TimelineItem`。未知事件继续进 `unknownEvents` 并 resync。

不要在手机上造第二套 EventLog。权威事实仍是 host 的 `EngineEvent` → `RuntimeEvent`。手机只投影。

共享模型（Desktop / TUI / Mobile）：

| 产品 | CURRENT Runtime | 手机现在 | Closure 后 |
| --- | --- | --- | --- |
| Project | 主机上的 repo daemon | `ProjectSummary` | 不变 |
| Task | `TaskId`，与 Session 1:1 | 无此词 | UI 把 Session 叫任务 |
| Session | 会话聚合 | `SessionSummary` + `SessionState` | 不变 id |
| Message | snapshot.messages | `TranscriptEntry` | 仍用于 assistant 文本；时间线引用同一 id |
| Event | `RuntimeEvent` | 半用半 ignore | timeline 的 kind |
| Artifact | `AttachmentRef` + media store | ignore | Phase 3 行 |
| Approval | `UiApprovalRequest` | `PendingApproval` | 钉底保留 |

Event-driven 适合 Agent：因为事实是 **工具、审批、子 Agent、产物**，不是轮流说话。气泡模型会把 40 次 `read_file` 变成「一分钟空白 + 一段长回复」。

---

# 第六部分：API 需求

传输保持：**配对 REST + 会话 WSS**（已有）。不要为 Mobile 再开一套公开 HTTP Agent API。

| 产品 API | 现状 | Closure 用法 |
| --- | --- | --- |
| Project | Relay `hosts` / `projects` REST | Home + 项目列表 |
| Task / Session | `request_session_list`、`open_session`、创建会话 RPC | 任务列表；UI 改名 |
| Message | `submit_message` | 空闲时追加 |
| Steer | host 有 `steer_current_turn`，手机 `Commands` 无 | Phase 2：Running 时 composer 走 Steer |
| Event stream | WSS `event` + `snapshot` + `resync_required` | Timeline；扩大 `applyEvent`，缩小 `_ignored` |
| Approval | `approval_decision` | 已有，UI 保持钉底 |
| Artifact | host `upload_attachment` 未完成；`AddAttachmentData` 远程策略部分拒绝 | Phase 3：只做 **下行** 预览/下载 RPC，不在本 Closure 做上传 |

投影名字（文档层，不是新 wire tag）：`session.started` ← `session_opened`；`message.created` ← `user_message_added` / `assistant_message_started`；`tool.called` ← `tool_call_started`；`tool.completed` ← `tool_call_completed`；`artifact.created` ← `attachment_added`；`approval.required` ← `approval_requested`；`task.completed` ← `session_completed` / 终态 `turn_*`。

**不要**让手机直接打 Unix socket 或 loopback Web。

---

# 第七部分：UI 技术实现建议

**继续 Flutter。** 配对 golden、Keychain、WSS 验签、16 个单测已经付过学费。不要迁 React Native / 不要用 WebView 包一层。

调整（小，不是换栈）：

| 项 | 建议 |
| --- | --- |
| 路由 | Phase 1 仍用 `_Root` 栈；Home 作为 `currentProjectId==null && session==null` 的默认页。需要时再加 `go_router`，不是本阶段必须 |
| 状态 | 保留 `ChangeNotifier`。Timeline 放进 `SessionState`，不要上 Bloc |
| Theme | 保留 seed `0xFF2F6FEB`。Timeline 行用 `outlineVariant` 细线，不用聊天气泡圆角 |
| 字体 | 工具名 / diff 用 monospace（Markdown 代码块已开始这样做） |
| 组件 | 新增 `Timeline`、`ToolStep`、`StatusChip`、`TaskRow`；`_ApprovalCard` / `_ClarificationCard` / `StatusBanner` 保留 |

Design 方向：Linear / GitHub Mobile / Cursor Agent 面板 / Claude Code 状态。不是 WhatsApp。

---

# 第八部分：实施计划与可执行任务（按 Commit）

一次不重写。每个 commit 应仍能配对、仍能发消息、仍能审批。

### 现在做（Phase 1 — UI Closure）

| Commit | 范围 | 验收 |
| --- | --- | --- |
| **M1** 文案与空状态 | 「新会话」→「新任务」；空会话图标与文案；Sessions 标题不改协议 | golden / chat_rendering 仍绿 |
| **M2** StatusChip | 任务列表 + Chat AppBar 用芯片代替「运行中…」纯文字 | 列表一眼能分 Running / Idle |
| **M3** Home | `_Root`：已配对且未选项目 → `HomeScreen`；聚合 Running / Waiting | 打开 App 不先掉进空项目列表 |
| **M4** Timeline 骨架 | `TimelineItem` + `applyEvent` 双写；Chat 用 Timeline 渲染 user/assistant/notice | 无气泡；旧 snapshot 仍能画出助手文本 |
| **M5** Tool 行 | `tool_call_*` 进入 timeline，不再只写 `activity` | 运行中能看到工具名序列 |
| **M6** Turn 状态行 | `turn_*` 在时间线插一条状态，不只把 `status=idle` | 完成/取消/失败可见 |
| **M7** Theme 收口 | 去掉 bubble `borderRadius` 路径；composer 圆角从 22 收到工具形 12 | 截图不再像即时通讯 |

### 紧接着（Phase 2 — 已有事件进 UI）

| Commit | 范围 |
| --- | --- |
| **M8** | 从 `_ignored` 拿出 `reasoning_delta`，可折叠 Thinking |
| **M9** | `plan_updated` → TaskHeader 弱阶段 |
| **M10** | `Commands.steerCurrentTurn`；Running 时 composer 走 Steer，idle 走 submit |
| **M11** | 独立 TaskHeader 组件（仍嵌在时间线页，不新路由） |

### 以后做（Beta 后产品）

| Phase | Commit 方向 | 依赖 |
| --- | --- | --- |
| **3 Artifact** | `attachment_added` 行；预览/分享；下行下载 RPC | host 上传/产物通道 |
| **4 Approval** | 已有卡保留；Home「需要你」聚合；不把 `approve_always` 加回来 | 无 |
| **5 Multi-Agent** | `sub_agent_*` 从 ignore → 时间线分组 / 简易 Agent 条 | Multi-Agent 产品稳定 |

### Beta 前 vs Beta 后

| Beta 前（Runtime 已大致具备） | Beta 后（手机产品） |
| --- | --- |
| Session、EventLog、ClientCommand、Approval、Remote pairing | Home、Timeline、Steer、Artifact 下行、Multi-Agent 展示、Push |
| 不要为手机改 spawn/claim/ownership | 不要在手机上做 IDE |

**明确不做（本 Closure）：** 扫码（粘贴已等价）、Android 本机构建、推送、语音、文件树、终端、Always allow、重写为 RN。

---

## 执行对照

现有代码里已经替我们标好了下一步：`session_state.dart` 的 `_ignored` 注释写着 plans、diffs、attachments「eventually」。Closure 就是把「eventually」排进 M4–M11，而不是另起一个 App。
