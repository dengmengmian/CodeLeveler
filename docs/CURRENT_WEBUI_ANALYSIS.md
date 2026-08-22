> **Historical / superseded.** The implemented desktop shell is
> [`WEB_DESKTOP_UI.md`](WEB_DESKTOP_UI.md). This analysis predates the
> AppRail removal and contextual Inspector.

# Current WebUI Analysis (vs WebUI v2)

**Date:** 2026-08-19  
**Baseline:** branch `fix/ma-wa1-delegation-reliability`, observatory closeout
`2caf588`.  
**Rule:** code facts only. No UI change in this document.  
**Target:** [`WEBUI_V2_ARCHITECTURE.md`](WEBUI_V2_ARCHITECTURE.md)  
**Prior freeze:** [`WEB_PRODUCT_UX_CLOSURE.md`](WEB_PRODUCT_UX_CLOSURE.md)
is **CLOSED** for that phase. v2 is a **new product stage**, not a silent
reopen of that freeze.

---

## Verdict

Web 已经是 **loopback SPA + 同一套 `ClientCommand` / `RuntimeEvent`**，不是
SaaS Chat 后端。Desktop Shell 和 Activity 缺的不是 EventLog，而是：

1. **壳层**：左栏是 260px「导航+业务列表」混在一起，不是 48px icon rail。
2. **Workspace**：只有 `chat | diff`，没有 Execution / Activity。
3. **Durable projection**：协议已有 `QueryObservability` /
   `ObservabilityLoaded`，**Web 从未发送、从未消费**。
4. **前端状态是 live turn 视图**，不是 Session 飞行记录仪。新用户消息会
   **清空本回合 tools / agents / backgroundTasks**。

TUI `/trace` 与 `leveler trace` 已经走 `query_observability`。Web 接入
Activity **不应**新建 EventLog、不应在浏览器 group_by。

---

# 1. Current Architecture

## 1.1 技术栈（Web SPA）

| 层 | 事实 |
| --- | --- |
| UI | React 19 + TypeScript，无 React Router（`web/src/main.tsx` 单根 `App`） |
| 构建 | Vite 6（`web/vite.config.ts`），`npm run build` → `web/dist` |
| 嵌入 | `leveler-web` `rust-embed` 编译期嵌入 dist（`crates/leveler-web/README.md`） |
| 状态 | `useImmerReducer`（`web/src/lib/useImmerReducer.ts`）+ `store.tsx` |
| 样式 | 单文件 `web/src/app.css`（token + 布局），无 Tailwind/CSS-in-JS |
| 网关 | Axum：REST + `/ws`（`crates/leveler-web/src/server.rs`） |
| 协议 | Rust schemars → `schemas/*.schema.json` → `web/src/types/protocol.gen.ts`；网关帧手写在 `protocol.ts` |

运行方式（`crates/leveler-web/README.md`）：

- `leveler web`：本进程 runtime
- `leveler web --connect`：连 `leveler serve --tcp`
- 聚合：`RouterService`（`crates/leveler-web/src/router.rs`）把多仓库
  daemon 合成一个 `LocalRuntimeService`

安全：loopback-only，256-bit bearer，token 不落盘。

## 1.2 UI 结构（现在）

`App.tsx` 外壳：

```
.deck  CSS grid  260px | 1fr | 340px     app.css:169
  Rail          左栏：品牌 + 新对话 + 列表面板 + Workspace 四 tab
  main.stage
    header      ☰ · repo/branch/**task title** · 对话|改动 · RunStatus · CTX · ⋯ · Inspector
    Timeline | DiffView | Hero
    Composer
  Inspector     任务 / 验证 / 检查点 / 记忆
```

响应式（`App.tsx:43-50`，`app.css:899+`）：≥1280 三栏；900–1279 Inspector
抽屉；&lt;900 Rail+Inspector 都是抽屉。与
[`WEB_PRODUCT_UX_CLOSURE.md`](WEB_PRODUCT_UX_CLOSURE.md) §9 一致。

| 区域 | 组件 | 职责 |
| --- | --- | --- |
| Rail | `components/Rail.tsx` | 面板 `sessions \| files \| search \| git`（`Panel` 类型 L21）。Sessions 是默认。Files/Search/Git 走 REST |
| Header | `App.tsx` `Shell` + `RunStatus` | 身份 + **对话/改动 tab** + 等待/运行/终态 + 上下文 |
| Conversation | `Timeline.tsx` + `AgentRunBlock.tsx` | 文档流；运行摘要插在本轮 user 之后（`lib/timelineLayout.ts`） |
| Changes | `DiffView.tsx` | 文件列表 + 聚焦文件 |
| Inspector | `Inspector.tsx` | 状态优先：waiting &gt; running &gt; terminal &gt; idle（`lib/inspectorModel.ts`） |
| Composer | `Composer.tsx` | 附件 + Run Configuration 摘要 + 发送；斜杠命令 **没有 `/trace`**（L21–33） |

没有独立 Settings rail：Appearance/Settings 在 Header `MoreMenu`
（`Appearance.tsx`）。没有 Agents rail。没有 Activity rail。

## 1.3 数据流

```
Engine EventLog  (persist-before-forward)
        ↓
leveler-app event_bridge  →  RuntimeEvent
        ↓
LocalRuntimeService / RouterService
        ↓
WS DownstreamMessage { event | snapshot | ack | error | project_status }
        ↓
web/src/lib/ws.ts  →  RuntimeBridge.handleFrame
        ↓
controller.applyEvent / applySnapshot
        ↓
store reducer  →  SessionView
        ↓
React
```

REST 不读 EventLog：

| 路径 | 文件 |
| --- | --- |
| `POST /api/sessions` | `server.rs:124` |
| `GET /api/sessions/:id/snapshot` | `server.rs:125` |
| `GET .../file,files,search,git-status,attachments` | `server.rs:126-136` |
| `GET/POST /api/projects*` | 聚合层 |
| `GET /ws` | `ws.rs` |

上行只有两种帧（`protocol.ts:29-31`）：`deliver`（任意
`ClientCommand`）和 `snapshot`。因此 **Web 理论上已能 deliver
`query_observability`**；前端没有调用。

Durable 查询在应用层：

- `crates/leveler-app/src/observability.rs` `query_observability`
- `interactive.rs` 处理 `ClientCommand::QueryObservability` →
  `RuntimeEvent::ObservabilityLoaded`
- TUI：`crates/leveler-tui/src/reducer/screen_nav.rs`、
  `observability/render.rs`
- CLI：`crates/leveler-cli/src/trace_cmd.rs`
- 远程：`QueryObservability` **Deny**（`leveler-remote-protocol/src/policy.rs`）

## 1.4 前端领域模型（live，不是 observatory）

`SessionView`（`store.tsx:110-155`）：

- 会话：id, title(=goal), repo, branch, model, axes, permission
- **本回合** tools / agents / backgroundTasks（新 `user_message` 时清空，
  L424–427）
- snapshot 只带回 `active_tools`（正在跑的调用），不是历史工具表
  （`store.tsx:286`）
- 终态 `lastTurn`（7 值 Turn Truth，`lib/turn.ts`）
- verification / diff / checkpoints / memory / tokens / context

**没有** `last_sequence`、没有 `UiObservabilityLoaded`、没有 session-wide
`UiToolAggregate`。

`controller.applyEvent`（`controller.ts:110-313`）消费 live
`tool_call_*`、`sub_agent_*`、`verification_updated`、`token_usage` 等。
`default` 分支（L309）显式忽略未知事件。`observability_loaded` 在
`protocol.gen.ts:543` 已生成，**这里会掉进 default，等于没接到**。

## 1.5 Runtime 数据在哪产生 vs Web 是否消费

| 事实 | 产生 | Web 现在 |
| --- | --- | --- |
| Session / snapshot | storage + `UiSessionSnapshot` | 是（整量重同步） |
| Turn 终态 | `RuntimeEvent` 7 终态 | 是（Turn Truth） |
| Tool live | `ToolCallStarted/Completed` | 是，但 **仅当前回合** |
| Tool durable | EventLog + `query_observability` | **否** |
| Model request rows | `model_requests` | **否**（只有 last-round `token_usage`） |
| Verification | snapshot + `verification_updated` | 是（Inspector 验证 tab） |
| Sub-agent live | `SubAgentUpdated/Progress/Activity` | 是（Inspector Agents 列表，无图） |
| Sub-agent durable | EventLog window | **否** |
| Event window / relations | `ObservabilityLoaded.window` | **否** |
| Diff / checkpoints | snapshot + events | 是 |
| User shell | `UserShell*` | 否（controller 注释：Web 无 `!command`） |

SSE：无。传输是 **WebSocket JSON 帧**。

缺的 projection：**不是新后端表**。缺的是 Web 对已有
`QueryObservability` 的一次 deliver + reducer + Execution/Activity 视图。

---

# 2. Gap Analysis

相对 [`WEBUI_V2_ARCHITECTURE.md`](WEBUI_V2_ARCHITECTURE.md)。

## P0 — Application Shell

| 目标 | 现在 | 差距 |
| --- | --- | --- |
| 48px icon rail | `.rail` 宽 260px，图标和 Sessions 列表同一栏（`app.css:169,184`） | **结构冲突**。要拆「icon rail / context sidebar」，不是换图标 |
| Context sidebar | Sessions/Files 内容直接画在 Rail 里（`Rail.tsx:45-49`） | 有内容、无独立 sidebar 槽 |
| Header 不堆任务 | Header 含 **task title** + **对话/改动 tabs**（`App.tsx:143-163`） | 与 v2「Header 禁止 Tab / 任务堆叠」冲突。UX Closure 故意把任务放 Header。v2 要把 tab 下放到 Workspace，title 回 Inspector/Sessions |
| Workspace tabs Conversation / Changes / Execution | `stageView: 'chat' \| 'diff'`（`store.tsx:159`） | 缺 Execution。改动已在中央，不必新开路由 |
| Inspector 动态 | 已有 waiting/running/terminal/idle | **可复用**。缺 Runtime TRACE 计数（turns/tools/errors session-wide） |
| Composer | 已是「摘要 + Run Configuration 弹层」 | 接近 v2。不要把四颗 chip 加回去 |

**可复用：** `Inspector` 模式机、`Composer` Run Configuration、`DiffView`
聚焦 diff、`Timeline` 非气泡排版、`AgentRunBlock` 本回合摘要、
`headerWaitingCue`、Turn Truth。

**冲突：** 260px 混合 Rail；Header 兼导航；无 Execution 槽；「新对话」文案
仍是 chat-first（`Rail.tsx:41-43`）。

## P1 — Runtime Activity / Execution / Changes / Design

| 目标 | 现在 | 差距 |
| --- | --- | --- |
| Activity = Observatory | TUI `/trace` 全屏；Web 无入口 | 协议已通，UI 未接 |
| Execution Timeline | `AgentRunBlock` + `ToolCallRow` 看 **本回合** live tools | 不是 EventLog 时间线；无 seq、无 class tag、无历史 |
| Session-wide tools | `MetaFooter` 用 `current.tools` 计数（`Inspector.tsx:352-364`） | **会把窗口/回合统计当成会话总量**——正是 Observatory 刚修掉的 TUI 问题。Web 绝不能再 group_by live tools |
| Changes workspace | `DiffView` 已是文件列表 + 聚焦 | Phase 3 大部分已完成 |
| Typography / design system | `app.css` tokens | 无独立 design-system 包；够 Desktop shell 迁移，不必先换栈 |
| Inspector Runtime section | running 态有 activity/elapsed/agents；terminal 有 Turn Truth + verify + changes | 无 model/duration/token **会话累计**、无 TRACE turns/tools |

Live `AgentRunBlock` 应保留为 **本回合 HUD**。Activity/Execution 必须走
`UiObservabilityLoaded`，否则重启/重连后工具表是空的。

## P2 — Agent Graph / Replay / Advanced debug

| 目标 | 现在 | 差距 |
| --- | --- | --- |
| Agent Graph | Inspector `AgentRow` 扁平列表（`Inspector.tsx:130-147`） | 无图、无 Main 根节点、无 child session id |
| Replay | 无 | EventLog 可重放在引擎侧（resume），Web 没有 debugger replay。保持 P2 |
| Advanced debug | 无 TTFT / request-id-on-tool / owner epoch | 与 Observatory Remaining Debt 一致，**不要为本阶段发明** |

---

# 3. Component Mapping

| Current | Target (v2) | 迁移动作 |
| --- | --- | --- |
| `App.tsx` `Shell` / `.deck` | `WorkspaceLayout` | 改 grid：48px rail + sidebar + workspace + inspector |
| `Rail.tsx` 整栏 | 拆 `ApplicationRail` + `SessionsSidebar` / `WorkspaceSidebar` | 列表从 icon 轨挪到 sidebar |
| Header `view-tabs` | Workspace tabs | 从 Header 移到 stage 顶；增加 Execution |
| `Timeline.tsx` | `ConversationView` | 保留；不要变成气泡 |
| `AgentRunBlock.tsx` | Conversation 内本回合摘要 | **不要**改成 Execution Timeline |
| `DiffView.tsx` | `ChangesView` | 基本已是 |
| `Inspector.tsx` Task/Verify | `TaskInspector` + 终态 Verification | 加 Runtime 段，数据来自 observatory DTO，不来自 `current.tools` |
| Inspector Agents 列表 | `AgentInspector` 雏形 | P2 再变 Graph |
| （无） | `ExecutionView` / Activity | **新建**，只渲染 `ObservabilityLoaded` |
| `Composer.tsx` | Composer + RunConfiguration | 保留；slash 加 Activity 入口或 Rail 承担 |
| `MoreMenu` | Settings rail item | Phase 1 可仍放 ⋯，Rail 留位 |
| `Hero.tsx` | New Task empty | 文案从「新对话」收到「New Task」 |
| TUI `observability/render.rs` | Web Activity | **不要抄 TUI 全屏**。同一 DTO，嵌入 Workspace |
| `query_observability` | Backend projection | **零改动**（已封板） |

---

# 4. Migration Proposal

原则：EventLog / query / 协议不动。Web 只增加 **command + view model +
壳层**。禁止为 v2 加 `tool_stats` 表、禁止 Grafana、禁止浏览器扫全量
payload。

## Phase 1 — Shell Migration

**改：** `App.tsx`、`Rail.tsx`、`app.css`、`store.tsx`（`stageView` 可先
仍两档，预留第三档类型）、Header 减负。  
**不改：** runtime、协议、Inspector 模式机、Diff 语义、Composer 轴。

| | |
| --- | --- |
| 范围 | 48px rail + 当前面板的 sidebar；Workspace 顶 tab 承接「对话/改动」；Header 只留 repo/branch/run status |
| 风险 | 与 UX Closure「任务在 Header」打架——验收时任务身份改到 Sessions 行 / Inspector 标题。响应式抽屉要重测 |
| 验收 | 三栏仍是应用壳；Sessions 仍是主实体入口；Changes 不丢；无新协议 |

## Phase 2 — Runtime Observatory Integration

**改：** `controller.ts`（deliver `query_observability`，apply
`observability_loaded`）、`store.tsx`（session 级 `observation` 字段，
**不要**覆盖 live tools）、新 Execution/Activity 视图、Inspector Runtime
计数用 `UiSessionObservation` / `UiToolAggregate`。  
**不改：** `query_observability` 实现、EventLog、Tool 执行语义。

| | |
| --- | --- |
| 范围 | 打开 Execution 或 Activity 时查询；refresh 可跟随 TUI
  `should_refresh_trace` 同类 live 事件（TUI：`observability/mod.rs`） |
| 风险 | 两套工具数字：live 回合 vs session-wide。UI 必须写清。重连后必须能重建
  Activity（这正是 observatory 的意义） |
| 验收 | 历史 session 的 Tools 是全会话；窗口行 ≠ 工具汇总；远程 Deny 保持 |

## Phase 3 — Engineering Workflow

大部分已在 UX Closure：Changes、Verification tab、Turn Truth、waiting
action。本阶段只补 v2 文案/信息架构对齐（Changes 作为 Workspace tab 已存在），
**不要**做 side-by-side review comment（Closure 明确非目标）。

## Phase 4 — Multi-Agent UI

Inspector 树 → Graph。数据：live `SubAgentView` + durable
`UiAgentObservation`（窗口局部，TUI 文案已承认）。Replay 单独立项。

---

# 5. Architecture Risks

1. **React state vs streaming**  
   当前 immer reducer 按事件增量更新，适合 WS。风险是把
   `ObservabilityLoaded`（有界窗口 + 聚合）误当成无限 event 数组塞进
   `SessionView`。应单独字段、有界（协议已 cap window 100、requests 200）。

2. **EventLog 要不要 frontend projection**  
   要，而且 **已经有**：`leveler-app::observability`。Web 不要第二套。
   `controller.ts` 缺的是 apply 分支，不是新 crate。

3. **TUI / Web 是否共享 domain model**  
   共享 **协议 DTO**（`UiObservabilityLoaded` 等），不共享组件。TUI 全屏
   `Screen::Trace`；Web 应是 Workspace 的第二视角。禁止 Web 直连 SQLite
   （TUI 也不连）。

4. **重复状态**  
   `SessionView.tools`（live）vs `observation.tools`（durable session-wide）
   必须并存、必须标注。用 live 列表冒充 Activity = 回归 Observatory 已修
   的窗口 bug。`token_usage` 是 last-round；会话 token 在
   `UiSessionObservation`。

5. **Desktop / Tauri**  
   没有 Tauri crate。Web 已是 rust-embed SPA + 同一协议
   （`docs/ARCHITECTURE.md` FUTURE desktop）。Phase 1–2 不引入 Tauri。
   只要 Web 继续只依赖 `ClientCommand`/`RuntimeEvent`/REST 浏览，日后 native
   window 可以嵌同一 dist。**不要**为 Desktop 加 special transport。

6. **Remote**  
   `QueryObservability` 对 remote **Deny**。Web 是本机 loopback，允许。
   不要为了「统一」改 remote policy。

7. **Header vs v2**  
   UX Closure 把「现在发生什么」放进 Header 一条 chip。v2 禁止 Header 堆
   任务/Tab。迁移时保留 **一条** `RunStatus` / waiting cue，把「对话|改动」
   移走即可，不要删 waiting 入口。

8. **Settings / Agents rail 项**  
   Phase 1 若强行六个 icon，Agents/Settings 会空。宁可 Phase 1 只放
   Sessions / Workspace / Search / Changes / Activity，Agents/Settings 占位
   或 Settings 仍走 ⋯。空 icon 比假页面好。

---

# 6. Recommended Sequence (do not skip)

```
Phase 0  本文（现状 + 差距）          ← 当前
Phase 1  Desktop Shell（无新协议）
Phase 2  QueryObservability 接入 Web
Phase 3  工程工作流对齐（增量）
Phase 4  Agent Graph / Replay
```

**下一指令应是 Phase 1 Shell，而不是先做 Activity 页面。** 先把壳拆对，
Activity 才有嵌入点；先做独立 `/trace` 页面会把 Observatory 做成「又一个
Dashboard」，与 v2 原则相反。

---

# 7. Files to read first in Phase 1

- `crates/leveler-web/web/src/App.tsx`
- `crates/leveler-web/web/src/components/Rail.tsx`
- `crates/leveler-web/web/src/app.css`（`.deck` / `.rail`）
- `crates/leveler-web/web/src/state/store.tsx`（`StageView`、`SessionView`）
- `crates/leveler-web/web/src/lib/controller.ts`（Phase 2 再动）
- `crates/leveler-client-protocol/src/observability.rs`（DTO，只读）
- `crates/leveler-app/src/observability.rs`（查询，只读）
- `docs/WEB_PRODUCT_UX_CLOSURE.md`（不要回退已关闭的 UX 语义）
