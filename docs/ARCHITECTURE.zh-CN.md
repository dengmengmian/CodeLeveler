# CodeLeveler 架构

本文对照**当前代码**描述 CodeLeveler 的稳定边界（基线 `main@04a015b` 及之后，
除非某节另有标注）。故意不绑定源码行号与随版本变动的实现计数，以便仓库演进后
仍可用。

## 如何阅读本文

每条架构陈述只属于下面四类之一，禁止混写：

| 标记 | 含义 |
| --- | --- |
| **CURRENT** | 树中已实现、今日可用。 |
| **TARGET** | 下一阶段目标归属，尚未做完。 |
| **KNOWN DEBT** | 已从代码确认的 CURRENT 与 TARGET 结构差距。 |
| **FUTURE** | 产品方向，尚无完整实现。 |

差距不得宣传成已交付能力；已交付能力不得写成「仅规划」。

配套文档：

- [`TUI_ARCHITECTURE.md`](TUI_ARCHITECTURE.md) — Geometry / Conversation /
  Presentation 的 **CURRENT** 所有权契约（`eceb271` 之后 hardening 已落地）。
- [`TUI_ARCHITECTURE_AUDIT.md`](TUI_ARCHITECTURE_AUDIT.md) — `eceb271` 改前审计
  （历史文档；geometry 单一 owner 其后已完成）。

---

## 设计目标

CodeLeveler 不追求最少 crate 或最少行数，而是一套**本地优先的编程代理运行时**，
能作为长期工作流基础设施：

1. **职责唯一。** 每种状态迁移、完成判断和副作用只有一个明确负责人，不在
   engine、agent 与工具层重复实现。
2. **行为可预测。** 安全、预算、取消、重试、恢复与终止语义由宿主确定，不依赖
   模型自觉。
3. **长期稳定。** 进程崩溃、断流、取消、超时或重启后，系统能判断已经发生什么，
   以及是否可以安全继续。
4. **能力可组合。** 规划、证据、验证、委派与（FUTURE）NPC 行为是可组合策略，
   不绑死在 direct loop 中。
5. **模型无关运行时。** Provider / 线协议差异不渗入编排、工具或 UI。
6. **安全边界不可绕过。** 路径、权限、沙箱、限额、取消与危险命令策略由宿主代码
   强制。
7. **状态可恢复、可审计。** Session、工具副作用边界与运行时事件可持久化并
   resume，不依赖单一进程生命周期或远程控制面。
8. **接口可演进。** 内部实现可以持续简化；公开 CLI、配置、存储与扩展接口按明确
   的兼容和迁移规则演进。
9. **单向依赖与类型化错误。** 应用层组合底层库；基础 crate 不反向依赖应用层，
   库边界保留可判别的失败类型。
10. **多端一致。** 终端、浏览器与（FUTURE）桌面/移动客户端共享同一 runtime
    契约：命令、事件、快照、审批与取消语义。

---

## 术语表

| 术语 | 权威含义 |
| --- | --- |
| **Engine** | 长期运行内核 / 监督器（`leveler-engine`）：task/turn 生命周期、EventLog、persist-before-forward、recovery、resume、明确终止、ownership fencing。 |
| **Agent Loop** | 模型 ↔ 工具执行器（`leveler-agent`）。不负责会话持久化、transport 或 UI。 |
| **Tool** | 面向模型的动作，注册在 `leveler-tools`（`Tool` trait + `ToolRegistry`）。 |
| **Host Execution** | 工作区安全、权限、进程执行、沙箱、检查点、artifact（`leveler-execution`）。 |
| **Task** | Engine 拥有的工作单元（`TaskId`）。 |
| **Turn** | Task 内一次有界执行切片。 |
| **Session** | 对话 / 客户端聚合；今日与主 task 1:1。 |
| **EngineEvent** | Engine 产出的规范域事实（非 transient 则持久化）。 |
| **RuntimeEvent** | 面向客户端的投影事实（`leveler-client-protocol`）。 |
| **ClientCommand** | 客户端发入 runtime 的意图（提交、取消、审批……）。 |
| **ClientOrigin** | 命令来源：`Local` \| `Remote` \| `RemoteTimeout`。**不是** User/Agent/System。 |
| **ExecutionKind** | Task/Session 执行策略：`Direct` \| `Parallel`。**不是** Tool/Shell/MCP/Capability。 |
| **Workflow / Policy** | Engine 之上可替换的规划 / 检查 / 重试组合。 |
| **NPC** | 长期运行 runtime / workflow / policy 域（FUTURE 产品化）。**不是** UI 客户端。 |
| **MCP** | **CURRENT** 工具集成：发现后适配为 `Tool`，暴露为 `mcp__<server>__<tool>`。 |
| **User Shell Execution** | CURRENT `!command`：用户发起的直接命令，**无** LLM/Agent Loop（测试硬门），经 `CommandRunner::run_streaming` 复用宿主执行安全。 |
| **Capability / Extension** | FUTURE 扩展面（可提供 model tool、user capability、workflow、hook）。未交付。 |

禁止在无 ADR 的情况下把 `ClientOrigin` 与动作发起者（User / Agent / Policy）
合并成一个叫 `ExecutionOrigin` 的维度。

在正文区分 Tool / Shell / MCP / Capability 时，优先用中性说法：**invocation
type**、**operation type**、**execution surface**——不要另造与 `ExecutionKind`
冲突的 Rust 枚举名。

---

## 核心架构

产品核心只有一件事：**在宿主强制的边界内，让模型用工具把仓库里的活干完，并且
状态可恢复、可审计。** 其余（TUI、Web、slash、技能、远程配对）都是入口或扩展面，
不是引擎本身。

crate 数量不是边界，职责和数据所有权才是：

| 块 | 唯一职责 | 不应承担 | 主要落点（CURRENT） |
| --- | --- | --- | --- |
| **1a. Engine** | Task/turn 生命周期、EventLog、恢复、resume、明确停止、可持久化 runtime 事实。 | 工具具体实现；UI；provider 线格式。 | `leveler-engine` |
| **1b. Agent Loop** | 模型 → 工具调用 → 宿主执行 → 结果回灌 → 模型。 | 会话持久化；transport；UI；硬编码产品规划策略。 | `leveler-agent` |
| **2. ToolHost / 执行** | Schema 校验、风险与审批、路径约束、可靠工具起止记录、进程/文件执行与取消。 | 对话编排；任务完成策略；UI 状态。 | `leveler-tools` + `leveler-execution` |
| **3. 会话状态** | 持久化消息、规范事件、快照与迁移，作为有序可审计事实。 | Agent 策略或隐式产品决策。 | `leveler-storage` + engine EventLog |
| **4. 模型适配** | 厂商中立的请求、流式与工具调用语义。 | 会话状态、工具权限、工作流决策。 | `leveler-model` + provider/protocol 适配 |

**正文不要把 Agent Loop 与 Engine 合并描述。** Engine 监督；Agent 只跑一轮
模型–工具反馈循环。

### 目标分层（全体客户端）

```text
                         客户端（CURRENT + FUTURE）

               TUI       Web       APP（FUTURE）
                │         │         │
                └─────────┼─────────┘
                          │
                leveler-client-protocol
                ClientCommand / RuntimeEvent / Snapshot
                          │
                          ▼
                 应用层  (leveler-app)
                          │
        ┌─────────────────┼──────────────────┐
        │                 │                  │
 交互式编程用例        投影              交互用例
        │                 │                  │
        └─────────────────┼──────────────────┘
                          │
                          ▼
                     ENGINE 内核
                    leveler-engine
         生命周期 / EventLog / 恢复 / 持久化
                          │
              ┌───────────┴───────────┐
              │                       │
              ▼                       ▼
      交互式编程工作           NPC Runtime（FUTURE）
                             Workflow / Policy
              │                       │
              └───────────┬───────────┘
                          │
                          ▼
                      AGENT LOOP
                    leveler-agent
                          │
                          ▼
                    模型工具
                    leveler-tools  （+ MCP 适配）
                          │
                          ▼
             宿主执行边界
                leveler-execution
          文件系统 / 进程 / 外部 I/O
```

**CURRENT 今日已有：** TUI、Web、远程 host bridge（`leveler-remote-agent`）、
Engine、Agent Loop、工具 + MCP、execution、client protocol、持久化。

**FUTURE：** 完整桌面/移动 APP 体验、User Shell（`!command`）、NPC runtime
产品化、Capability / Extension 框架。

不要凭空宣布不存在的 crate（`leveler-npc`、`leveler-capability`……）。逻辑层
以后若所有权需要，可以再物理拆分。

---

## Runtime 内核（Engine）

**CURRENT。** `leveler-engine` 是长期运行的内核 / 监督器：

- Task 与 Turn 生命周期
- 追加写 **EventLog**，**persist-before-forward**
- 崩溃恢复、resume、重启后 reap
- 带类型结果的明确终止
- ownership fencing 与可持久化 runtime 事实
- `ExecutionKind`：`Direct` \| `Parallel`（task/session 策略）

它必须能回答：哪些状态已持久化、哪个工具可能已产生副作用、能否安全重试、
恢复点在哪里、任务为何停止。

规范工具事件的持久化属于副作用协议：产生外部副作用前必须可靠记录开始，结束后
必须可靠记录结果；仅用于显示的流式 delta 可以走允许丢失的通道。

生命周期词汇由 `leveler-lifecycle` 共享（通用 runtime 状态 + Coding workflow
面包屑；后者细化前者，从不重定义）。

---

## Agent Loop

**CURRENT。** `leveler-agent` 是 **Agent Executor / Agent Loop**：

```text
模型 → 工具调用 → 宿主执行工具 → 工具结果 → 模型
```

它**不是**：

- 会话持久化所有者
- Transport 所有者
- UI 所有者
- 厂商协议所有者

产品侧执行只有一条路径：**direct agent 工具循环**。长任务仍走该路径——goal
模式（`update_goal` 直到 complete 或 blocked）与可选的 `spawn_agent` 扇出。
日志里遗留的 `orchestrate` kind 会被接受并按 direct 跑。

**一个 goal 跨多个 bounded work window，而不是一个巨大的 turn。** 单 turn 的
100-round 上限结束的是一个 *work window*，不是整个 goal：supervisor
（`DefaultSupervisorPolicy::after_turn`）会开下一个 window（`DriveGoalAgain`，
一个重述目标的全新 turn）而非直接失败。窗口受双重 bound——内存级跨窗
no-progress 计数器（`MAX_NO_PROGRESS_WINDOWS`，被实质工作区改动重置）在绝对
`MAX_SUPERVISED_TURNS` 上限之下——所以卡住的 goal 会收敛而非空转。用尽 round
预算的窗口是 `BudgetLimited`（未完成、可恢复），**不是** `Failed`。goal 身份即
session；window 预算在内存中（daemon 崩溃后的窗口恢复是未来工作，非当前）。

---

## 宿主执行边界

**CURRENT。** `leveler-execution` 负责：

- 工作区安全与路径解析
- `PermissionProfile`、`RiskLevel`、审批策略
- 进程执行（`CommandRunner`）、沙箱后端、取消
- 检查点、后台进程、artifact

**执行宿主在 daemon 里，不在 client。** 审批是每 session 的策略
（`ApprovalPolicy`，随 trusted-local 的 `CreateSessionRequest` 下发）：
`--auto-approve` 是在 daemon 上选一个无人值守的 session，而不是把 runtime 嵌进
TUI 进程——因此长跑的 goal 能熬过 client 断连，reconnect 能恢复仍在运行的 turn。
remote/web client 不能把自己的 session 提升为 auto-approve——边界会把它强制
重置回 interactive。

平台控制（显式能力探测，从不虚报）：

- Windows：Job Objects；可用时 AppContainer / ACL
- macOS：Seatbelt
- Linux：Bubblewrap；经审计的 `PR_SET_PDEATHSIG` 孤儿进程清理

**不变量：** 所有仓库修改与进程执行都必须经过该边界——包括 FUTURE 的 User Shell
（`!command`）、Capability 与 Extension 工具。用户发起 ≠ 绕过宿主安全。

区分：

- **模型授权**（模型可以*请求*什么）
- **宿主授权**（宿主实际允许什么）

模型永远不能自行提升权限。

---

## 客户端运行时契约

**CURRENT。** `leveler-client-protocol` 已是稳定的跨客户端契约——不是未来才需要
的新设计。

| 表面 | 职责 |
| --- | --- |
| `ClientCommand` | 客户端发入 runtime 的意图 |
| `RuntimeEvent` | 归一化的面向客户端事实 |
| `UiSessionSnapshot` | 重连 / resync 真相（+ 水位） |
| `InteractiveRuntimeClient` | `send` / `subscribe` / `snapshot` |
| 协议版本 / envelope | major 兼容；minor 可加字段 |

客户端**不得**各自实现任务生命周期。关闭任一客户端不应取消 runtime 已接受的
工作。视图状态（滚动、展开、选区）可以是客户端本地的；任务事实、权限决定与
执行状态只能来自 engine 事实与快照。

Transport 可变，契约不变：

- 进程内（`InProcessRuntimeClient`）
- 本地 daemon / Unix socket（`leveler-local-transport`）
- Session wire / WebSocket（`leveler-session-wire`、Web）
- 远程 bridge（`leveler-remote-agent` + remote protocol / relay）

除非真实需求强制，否则不要平行新造「Presentation Protocol」「UI Protocol」
或「Frontend Event Bus」。

`ClientOrigin`（`Local` \| `Remote` \| `RemoteTimeout`）是**命令来源**（审计、
远程审批超时压力），不是授权，也不是动作发起者（User / Agent / System）。

---

## 应用层

**CURRENT。** `leveler-app` 是组合根：配置、打开存储、组装 provider/工具、挂上
engine、交互会话用例，以及从 engine 事实向客户端投影。

`InProcessRuntimeClient` 当前集中了多种职责（会话 runtime 配置、事件流、审批、
澄清、媒体、检查点、live view、活跃 turn、steering、命令分发）。该集中度记在
[当前架构债](#当前架构债与演进接缝)。

环境变量读取集中在配置与应用启动；下游库接收已解析的值。

---

## 客户端

### TUI（**CURRENT**）

`leveler-tui` 是一等终端客户端，只通过 `leveler-client-protocol` 接入，**不**
拥有 agent 逻辑、工具执行、权限决策或持久化真相。

Unix 上默认 `leveler tui` 为 discover-or-start（探测仓库 daemon socket）；关闭
TUI 不会取消已接受的工作。`--in-process` 为显式嵌入模式。Windows 保持嵌入式
runtime（尚无 socket transport）。

TUI 交互正确性（历史 disclosure、鼠标、时长真实性、PTY 校验）已收口。
Conversation geometry 所有权已 hardening（见 [TUI 架构](#tui-架构)）；残余
可维护性接缝见架构债。

### Web（**CURRENT**）

`leveler-web` **已经是** runtime 客户端——不是未来概念。axum 服务把 SPA 桥到
`LocalRuntimeService`，经 token 鉴权的 REST + WebSocket 传输 `ClientCommand` /
`RuntimeEvent` / 快照。仅绑定 loopback；256-bit bearer token；多项目靠 per-repo
daemon 与 `RouterService`。详见 `crates/leveler-web/README.md`。

### Remote / APP（**CURRENT** bridge，**FUTURE** 完整产品）

`leveler-remote-agent` 是 **host 侧远程 bridge**。它刻意**不**依赖
`leveler-web`，而是通过 `leveler-session-wire` 与 runtime/client 协议边界工作。
面向未来桌面/移动 APP 的基础设施已经存在。

不要写「未来从零增加 Remote APP architecture」。完整 Desktop APP 与移动产品
体验仍属 **FUTURE**。

---

## 工作流与 NPC Runtime

复杂任务不靠扩大 direct loop 实现。核心提供持久化生命周期、安全执行和明确终止；
规划、证据、阶段检查、修复、委派由可替换的 **workflow / policy** 组合。

```text
复杂任务 / NPC（FUTURE 产品化）
    ↓
Workflow / Policy       规划、拆解、检查、重试
    ↓
Engine                  生命周期、监督、持久化、恢复、终止
    ↓
Agent Loop              单一模型—工具—结果循环
    ├── Model Runtime
    └── ToolHost / 执行
```

### NPC 不是 UI 客户端

**错误：** 把 TUI | Web | APP | NPC 全部并列成 Interface。

**正确：**

- **客户端 / 界面：** TUI、Web、APP
- **NPC：** 同一 Engine 之上的长期运行 runtime / workflow / policy 域

NPC **必须**复用 Engine、ToolHost、权限、持久化、恢复与事件模型，**不能**产生
第二套 Agent Loop、Session、Permission 或 Recovery 模型。

NPC 产品化是 **FUTURE**。支撑它的 policy / 生命周期原语是 **CURRENT** 积木。

---

## 事件与投影模型

| 层 | 职责 |
| --- | --- |
| **EngineEvent** | 规范域事实（engine 拥有）。 |
| **RuntimeEvent** | 面向客户端的投影（协议拥有）。 |

**TARGET** 依赖方向：

```text
Engine → EngineEvent → 单一应用层投影 → RuntimeEvent → TUI / Web / APP
```

UI 不应依赖 engine 内部事件模型。

### CURRENT 过渡路径（**KNOWN DEBT**）

```text
EngineEvent
  → engine_event_to_agent()   (leveler-app)
  → AgentEvent
  → EventBridge
  → RuntimeEvent
```

这是今日真实代码中的 transitional bridge，**不是**理想终点，也不得假装已删除。
收敛为单一应用层投影是 **TARGET** 的 core hardening，不是已完成能力。

---

## 持久化与恢复

**CURRENT。** 规范 task 事实、执行事实、消息、event log 与快照属于
runtime/持久化（`leveler-storage` + engine EventLog）。

Task 与 Session 身份不同：**task** 是 engine 拥有的工作单元；**session** 是
对话/客户端聚合。今日每个 task 恰好有一个主 session。Engine 只依赖窄的存储
port（`EventStore`、`TaskStore`、`SessionStore`…，打包为 `EngineStores`）。

UI 瞬时状态（滚动、展开、选区、焦点、viewport、拖拽）**绝不是**规范 task 真相。
客户端若需保存视图偏好，只能是 client-local state。

密钥可来自环境变量或本地 `api_key`，但解析后的凭证与 Authorization 头不得写入
session 消息、runtime 事件、日志或 artifact。

---

## 工具与 MCP

### 工具（**CURRENT**）

`leveler-tools`：

- `Tool` trait、`ToolRegistry`
- 执行前 schema 校验
- 内置工具与适配器的模型侧注册

### MCP（**CURRENT**，不是「未来功能」）

MCP 工具动态发现后适配为 `Tool` 并注册，暴露名为：

```text
mcp__<server>__<tool>
```

生命周期、重连、与 extension/capability 的更深层集成属于可选后续增强——MCP
**已经**作为工具集成存在。

### 执行面（**CURRENT**）

所有 mutation 与进程执行仍经过 `leveler-execution`。写操作与命令类工具在必要
时串行，避免冲突修改。

**多 Agent：** 父 executor 可在 depth 0 且启用委派时广告 `spawn_agent`；并发子
agent 的工具起止以归属 activity 上浮；完整子对话不进父消息列表。详见
`docs/multi-agent.zh-CN.md`。

---

## TUI 架构

完整 **CURRENT** 所有权图：[`TUI_ARCHITECTURE.md`](TUI_ARCHITECTURE.md)。

```text
TUI
  → Client Protocol
  → 本地呈现状态
  → Components
  → Ratatui
```

TUI 不得拥有：agent 逻辑、工具执行、权限决策、持久化真相。

### Conversation 子系统（geometry hardening 后 **CURRENT**）

Conversation 拥有权威 geometry 及相关视图关注点：

- View state、geometry、viewport、scroll、auto-follow
- Hit test、选区映射、行/hit 缓存、bottom alignment

Renderer 与 reducer **不得**各自再算一份 viewport geometry（历史「点 A 展开 B」
正源于此）。

### Workbench

负责顶层布局与组件组合。**不**负责 Conversation 内部滚动数学、重复的
screen/content 坐标、工具语义分类或 runtime 执行逻辑。

### Disclosure

Tool disclosure 呈现已存在（`presentation::disclosure` 为 domain-free 视觉；
`activity_stream` 为 Agent Tool 适配器）。

**CURRENT：** 同一 disclosure 视觉语言已通过适配器呈现 Agent Tool 与 User
Shell（第二个消费者证明了边界）；未来 Capability 行沿同一适配器模式接入。

---

## 扩展方向（**FUTURE**）

Extension / Capability **没有**完整实现。禁止写成已支持。

仅描述语义方向：

```text
Extension
   └── 可提供
         ├── Model Tool
         ├── User Capability
         ├── Workflow
         └── Hook
              └── 宿主执行 / 安全边界
```

未来 Capability 不能绕过权限、工作区、取消、审计或执行。

本文**不**冻结：`ExtensionHost`、`CapabilityRegistry`、manifest schema、WASM
ABI、JSON-RPC 插件协议或 marketplace。这些需有真实实现后才成为架构事实。

今日已有的扩展点仍是：

- Provider / 协议适配
- 注册工具 + MCP server
- 校验命令
- Skills
- Hooks（工具前后外部命令）

---

## 当前架构债与演进接缝

只记录代码已确认的结构问题，不是 wishlist。

### 1. TUI geometry 所有权 — 主体已完成

| | |
| --- | --- |
| **曾是（`eceb271` 改前）** | workbench / reducer / AppState 共同知道 conversation rect、viewport、scroll、auto-follow、hit-test、screen→content、选区、缓存——存在「点 A 展开 B」类风险。 |
| **CURRENT（`04a015b`+）** | `conversation::{geometry,view,build,viewport,interaction}` 为唯一 owner；workbench 只做组合；reducer 向 interaction 询问点击语义。见 `TUI_ARCHITECTURE.md`。 |
| **残余** | 双重展开语义（`tools_expanded` vs per-group `expanded`）；首帧 geometry fallback；User Shell 呈现适配器尚未存在。 |
| **状态** | Geometry 单一 owner：**已完成**。残余可维护性项：**KNOWN SEAM**。 |

### 2. EngineEvent 投影 shim —— 已解决

| | |
| --- | --- |
| **CURRENT** | `EngineEvent` → `EventBridge`（leveler-app）→ `RuntimeEvent`，**穷举 match**：新增 EngineEvent 变体必须显式做投影决策才能编译。16 条表驱动等价测试钉住客户端可见形态。 |
| **Legacy** | `engine_event_to_agent` 仅作单向适配器保留，服务 headless CLI 渲染器与 eval 收集器；永不作为 UI 路径。 |
| **状态** | **DONE**（core hardening）。 |

### 3. 动态 Tool 元数据 —— 已解决

| | |
| --- | --- |
| **CURRENT** | `Tool::name/description` 返回借自实例的 `&str`；Registry 拥有自己的 key（`BTreeMap<String, _>`）；`McpTool` 持有 owned 元数据。生产代码零 `Box::leak`；内建工具零改动。 |
| **状态** | **DONE**（core hardening）。reconnect/reload 可重建 registry 而不累积泄漏。 |

### 4. ToolContext 膨胀 —— 已解决

| | |
| --- | --- |
| **CURRENT** | 按生命周期分三个 facet：`execution`（进程级执行+写安全基础设施）、`policy`（门禁/预算；两个放权开关为私有字段，仅 `grant_network()` / `grant_unrestricted_fs()` 可放权）、`services`（LSP/artifact/memory/background）。类型上写明 anti-growth 规则：新字段必须声明生命周期并入 facet。 |
| **归属指引** | Extension 服务 / secret provider → `services`；remote executor → `execution`。 |
| **状态** | **DONE**（core hardening）。执法语义零变更。 |

### 5. InProcessRuntimeClient 职责集中 —— 部分解决

| | |
| --- | --- |
| **CURRENT** | `CheckpointStore`（双 map 联合不变量）、`LiveViews`（重连状态 + 纯 fold）、`stage_turn`（唯一 turn 启动前奏）已提取；facade 只做路由与委托（2739 → 2501 行，12 个状态字段）。 |
| **残余** | 交付中间件、会话目录 CRUD、runtime-config store、media/memory 臂保持内联（无独立状态/单一路径，按提取规则暂不拆）；删除会话时 per-session map 不清理仍为 KNOWN SEAM。 |
| **状态** | **核心簇 DONE**；其余登记在案。新增 client use case = 小 handler + facade 路由。 |

### 6. 结构化客户端事件 vs 预格式化文案 —— 规则已立，迁移已启动

**规则**（对新代码有约束力）：稳定产品事实以 typed `RuntimeEvent` 过线，客户端
负责措辞/排版/语言；自由诊断（意外错误、传输故障、模型/工具输出）保留
`Notification`/`String`。领域事实永远不是一条预格式化中文字符串。

| | |
| --- | --- |
| **已迁移** | `ContextCompacted { from, to }` 与 `ContextExpanded { from_tokens, to_tokens, reason }`（协议 1.4 additive；schema+golden 已再生成；TUI 双语本地化；Web 镜像更新）。顺带修复：TUI 曾以 `budget exhausted`（空格）嗅探而执行器发 `budget_exhausted`（下划线）——用户看到裸机器串。 |
| **残余** | AgentActivity 咨询标签、turn-incomplete 默认 reason、interactive.rs 各处通知仍为预格式化——登记在案，按上方规则机会性迁移。 |
| **状态** | **政策生效；高置信事实 DONE** |

---

## 不可破坏的核心约束

- 所有仓库修改与进程执行只能经过 ToolHost / execution 边界。
- 同一个生命周期状态只能有一个写入者；投影和 UI 只能消费规范事件。
- 已被宿主接受的工作不能因为某个 UI 断开而丢失。
- 恢复不得盲目重放可能已经产生外部副作用的工具调用。
- 每次停止都必须有可判别原因：完成、阻塞、取消、预算耗尽或失败。
- 策略可以替换或关闭；安全、持久化和取消边界不能关闭或绕过。
- 旧配置、旧数据库和旧事件通过显式兼容窗口与迁移处理。
- 断开 TUI、Web 或远程客户端不会改变任务事实；重连只能通过规范 snapshot /
  resync 恢复。
- FUTURE 的 User Shell / Capability / Extension 仍须经过宿主执行安全。

### Unsafe Rust

大多数 crate 使用 `#![forbid(unsafe_code)]`。**`leveler-execution` 例外：**
`#![deny(unsafe_code)]`，并有一处经审计、局部允许的 Linux
`PR_SET_PDEATHSIG` pre-exec hook，用于孤儿进程清理。应用与 CLI 可用 `anyhow`
补充上下文；可复用的库 crate 暴露 `thiserror` 类型化错误。

---

## 组件图

```text
User
  │
  ├── leveler-cli ────────────────┐
  ├── leveler-tui                 │
  └── leveler-web（浏览器 UI）     │
          │                       │
          ▼                       ▼
  leveler-client-protocol    leveler-app  ◀── 组合 / 配置 / 投影
          │                       │
  leveler-local-transport         ▼
  leveler-session-wire     leveler-engine
  leveler-remote-agent            │
  leveler-remote-protocol         │
  services/leveler-relay          │
                 ┌────────────────┼─────────────────┐
                 ▼                ▼                 ▼
          leveler-agent                    leveler-verifier
                 │                                  │
                 ├────────▶ leveler-context         │
                 └────────▶ leveler-tools ◀─────────┘
                                  │
                                  ▼
                         leveler-execution

  leveler-provider ─▶ leveler-protocol ─▶ leveler-model
         │                                      ▲
         └──────────── 由 engine 使用 ──────────┘

  支撑：leveler-core, leveler-lifecycle, leveler-storage,
  leveler-project, leveler-vcs, leveler-lsp, leveler-skills,
  leveler-memory, leveler-media, leveler-eval, leveler-test-support
```

箭头是概念上的依赖与调用方向。部分边通过 trait 表达，便于用确定性假实现测试。

---

## 运行时流程

### 1. 组合

`leveler-app` 解析全局与项目配置、打开存储、构建 provider 与工具注册表、选择
执行策略，并把 engine 接到 CLI、进程内 client 或本地 transport。

### 2. 模型请求与流式

Agent 产出与 provider 无关的 `ModelRequest`。`leveler-provider` 选择
provider/model；`leveler-protocol` 负责线格式互转。

```text
HTTP 字节流
  → SSE 帧解码
  → 协议 chunk 解码
  → 分片 tool-call 组装
  → ModelEvent 流
  → engine 与客户端
```

非法或截断的 tool-call JSON 会报错，**不会**被「修好」成可执行调用。

### 3. Turn 与工具循环

Engine 拥有 task/turn 生命周期；Agent 跑 direct 工具循环；宿主代码拥有状态迁移、
预算、取消、权限与完成规则。

### 4. 工具与命令执行

`leveler-tools` 做 schema 校验与分发；`leveler-execution` 强制宿主边界。

### 5. 校验与完成

`leveler-verifier` 发现或接收 format / build / test 命令，记录证据并分类失败。
修复尝试有界。

- `format`：尽力而为，**不门控**完成
- `build` / `test`：**门控**完成
- 项目 `verify` 任一字段出现时，**整段替换**语言自动发现计划

### 6. 持久化与重连

SQLite 存储；客户端经 snapshot + 事件水位 resync。

每个 runtime 状态目录拥有持久 `RuntimeId`。同一状态目录上同一时间只服务一个
daemon（socket bind + 锁）。

---

## 重要边界

### Provider 边界

上层只消费 `ModelRequest` / `ModelResponse` / `ModelEvent` / `ModelError`。
厂商 JSON、SSE 癖性与 Authorization 头留在协议层以下。

### 执行边界

Agent 循环内不得直接访问文件系统或进程以绕过审批、检查点、脱敏与取消。

### 持久化边界

写入前脱敏凭证。

### UI 边界

客户端渲染协议事件并发送命令，不拥有 agent 执行。多项目 Web 行为（daemon
探测、spawn、`RouterService`、`~/.leveler/state/web/projects.json`）见「客户端 → Web」。

---

## User Shell Execution（**CURRENT**）

`!command` 已实现（TUI 入口）：

```text
!git status
  → TUI submit 路由（原始首字符 `!`，不 trim；散文永不误执行）
  → ClientCommand::RunUserShell（协议 1.5，additive）
  → leveler-app use case（ActiveTurns 前台互斥；hang guard）
  → CommandRunner::run_streaming —— 与 agent shell 同一宿主边界
      （权限档写限制 / 网络沙箱 / env 清洗 / 进程树终止 / 仓库根 cwd）
  → 规范 EngineEvent 事实（Started/Output*/Finished；Output transient，
      其余持久化 LocalOnly）→ 会话 EventLog（turn_id = None）
  → 穷举 EventBridge 投影 → RuntimeEvent UserShell*
  → TUI UserShell block（复用 presentation::disclosure）+ Shell Details
```

测试硬门：每次执行零模型请求；命令与输出永不进入模型上下文。取消按
`UserShellId` 逐执行（`CancelUserShell`），绝非 `CancelCurrentTurn`。重连经
`UiSessionSnapshot.user_shells` 恢复活跃 + 有界历史。MVP 为非交互 shell
（`sh -c` / `cmd /C`，stdin 关闭）——不是 PTY/终端模拟器，vim/top/交互式 ssh
不在范围。Remote policy 拒绝两条命令。runtime 重启按既有进程清理策略处理
（无跨进程收养）。

---

## 下一阶段架构顺序

1. 架构文档校准（本文）
2. 残余 TUI 可维护性接缝（双重展开态、shell 呈现）
3. Core architecture hardening（EngineEvent → RuntimeEvent 投影等）
4. 全量回归
5. User Shell Execution（`!command`）
6. 真实项目 dogfooding
7. 仅在观察到需求后再做 Capability / Extension

没有新证据时，不要把 Capability 排到 User Shell 之前。

---

## 配置分层

| 层 | 路径 | 作用 |
| --- | --- | --- |
| 全局 | `~/.leveler/config.toml` | 默认模型、provider、MCP |
| 包配置 | `configs/providers/`、`configs/models/` | 可入库的 provider/model 档案 |
| 项目 | `<repo>/.leveler/config.yaml` | 模型覆盖、权限 profile、verify、ignore、只读根、limits |
| 权限 | `~/.leveler/permissions.yaml`、项目文件 | 持久 allow/ask/deny |
| Hooks | `~/.leveler/hooks.yaml`、项目文件 | 工具前后外部命令 |

示例见同目录 `*.example.yaml`、`leveler-config-example.yaml` 与
[`configs/example.yaml`](../configs/example.yaml)。

---

## Home 与运行时布局（**CURRENT**）

**零工作区污染。** CodeLeveler 绝不在工作区内创建或修改自己的运行时/状态设施；
所有机器写入都落在唯一的全局 home 下，checkout 保持干净、无需 `.gitignore`。
仓库内唯一的 `.leveler/` 是**用户手写、可入库**的配置（`config.yaml`、
`instructions.md`、`rules/`、`skills/`、`hooks.yaml`，以及 `AGENTS.md`）——
运行时只读不写。

`LevelerHome`（[`leveler-core/src/home.rs`](../crates/leveler-core/src/home.rs)）
是所有自有路径的唯一权威：它只解析一次根目录（`$LEVELER_HOME`，否则
`$HOME/.leveler`，否则 `%USERPROFILE%\.leveler`，否则进程级临时目录——绝不用
cwd 相对兜底），并通过具名访问器交出每个子路径。业务 crate 只向 `LevelerHome`
索取，不自行拼接根目录。一个测试探针会在任何 crate 手工重建 home 路径时让构建失败。

```text
~/.leveler/
├── config.toml                      全局用户配置（+ agents/、skills/、hooks.yaml、permissions.yaml、trusted.yaml）
├── state/                           持久状态
│   ├── projects/<id>/               每仓库：sessions.db、memory/、permissions.yaml（机器写）、图片存储
│   ├── remote/                      远程配对密钥、配置、设备
│   └── web/  projects.json · uploads/   多项目注册表 + 导入附件
├── run/                             临时运行时协调
│   ├── sockets/                     daemon Unix socket（短的每仓库哈希 → 守住 macOS SUN_LEN）
│   ├── locks/                       工作区编辑咨询锁
│   └── sandboxes/<id>/ (+ <id>.lock)   带 OS 租约的单命令 scratch
├── cache/  tools/                   可丢弃、可重建的工具链缓存
├── runtimes/                        受管执行依赖（预留）
└── logs/   leveler.log · crash/ · daemon/<id>.log
```

布局是惰性的：索取路径绝不创建它。项目 id = 可读 slug + 其规范路径的短 SHA-256，
因此状态以仓库为键、无需注册表。

**Sandbox 租约 + reaper。** 每个受沙箱命令在 `run/sandboxes/` 下拿到一个 scratch
目录，由其 `<id>.lock` 旁文件上的排他咨询锁（与 daemon 选举锁同一 `flock` 原语）
守护，持有到命令结束。RAII 在完成时清除两者；崩溃时 flock 由 OS 释放。一个
fail-closed 的 reaper 每进程运行一次（绝不定时）：能拿到的锁即证明持有者已死，
于是回收该 scratch；仍被持有的锁、或没有锁的目录，一律不动。

---

## Browser Capability（**CURRENT**）

结构化浏览器自动化——agent 验证 Web/前端工作的主路径，取代 `chrome --headless`/curl 临时脚本。

```text
Agent → browser_* 工具（leveler-tools）
      → ToolServices.browser（Arc<BrowserRuntime>，daemon 持有）
      → BrowserRuntime（crates/leveler-browser）：refs · generation · session 隔离
        · snapshot 预算 · 懒式受管安装
      → Node/Playwright driver 子进程（stdio 上的 JSON-RPC）
      → 系统 Chrome（channel:'chrome'）或受管 Chromium
```

- 工具：`browser_navigate/snapshot/click/type/select/press/wait/tabs/dialog/
  console/screenshot`。**语义快照**（Playwright 带 ref 的可访问性文本，有预算上限）是
  控制协议；`[ref=…]` 只在其 `(session, page, generation)` 内有效，过期 ref 被判
  `RefStale`、绝不改指向。无 `browser.evaluate`。
- **daemon 持有**（在 `Application` 上，同 `background_tasks`），跨回合、跨客户端断连存活。
  懒式：首次调用 browser 工具前不启动。
- **文件系统**：受管 runtime 在 `runtimes/browser/`，每项目隔离 profile 在
  `state/projects/<id>/browser/profile/`——绝不入工作区、绝不用用户真实 Chrome profile。
- **权限**：复用 `RiskLevel`/`ApprovalPolicy`（读=Safe，动作=Network）；`browser_navigate`
  走 SSRF 门（`web_fetch::is_blocked_ip`）。

见 `docs/BROWSER_CAPABILITY_ANALYSIS.md` 与 `docs/BROWSER_CAPABILITY_REPORT.md`。

---

## 仓库导览

- `crates/` — Rust workspace
- `configs/` — provider/model 档案
- `docs/` — 架构与配置示例
- `evals/` — 评测
- `migrations/` — SQLite 迁移
- `services/leveler-relay` — 远程中继服务
- `.github/workflows/` — CI

英文版：[`ARCHITECTURE.md`](ARCHITECTURE.md)。入口：[`README.zh-CN.md`](../README.zh-CN.md)。
