# CodeLeveler 架构

本文描述 CodeLeveler 的稳定边界。故意不绑定源码行号和随版本变动的实现细节，
以便仓库演进后仍然可用。

## 设计目标

CodeLeveler 的目标不是最少的 crate 或代码行，而是一套能作为长期工作流基础设施的
Agent 核心。它必须同时满足：

1. **职责唯一。** 每种状态迁移、完成判断和副作用只有一个明确负责人，不在 engine、agent 与工具层重复实现。
2. **行为可预测。** 安全、预算、取消、重试、恢复与终止语义由宿主确定，不依赖模型自觉。
3. **长期稳定。** 进程崩溃、断流、取消、超时或重启后，系统能判断已经发生什么，以及是否可以安全继续。
4. **能力可组合。** 规划、证据、验证、委派与 NPC 行为是可组合策略，不绑死在 direct loop 中。
5. **模型无关运行时。** Provider / 线协议差异不渗入编排、工具或 UI。
6. **安全边界不可绕过。** 路径、权限、沙箱、限额、取消与危险命令策略由宿主代码强制。
7. **状态可恢复、可审计。** Session、工具副作用边界与运行时事件可持久化并 resume，不依赖单一进程生命周期或远程控制面。
8. **接口可演进。** 内部实现可以持续简化；公开 CLI、配置、存储与扩展接口按明确的兼容和迁移规则演进。
9. **单向依赖与类型化错误。** 应用层组合底层库；基础 crate 不反向依赖应用层，库边界保留可判别的失败类型。
10. **多端一致。** TUI、Web 和手机端是同一个 runtime 的一等客户端，共享命令、事件、快照、审批与取消语义。

## 核心

产品核心只有一件事：**在宿主强制的边界内，让模型用工具把仓库里的活干完，并且状态可恢复、可审计。**  
其余（TUI/Web、slash、技能、远程配对）都是入口或扩展面，不是引擎本身。

核心仍然只有四块；crate 数量不是边界，职责和数据所有权才是：

| 块 | 唯一职责 | 不应承担 | 主要落点 |
| --- | --- | --- | --- |
| **1. Engine + direct loop** | Engine 监督 task/turn 生命周期、恢复与明确终止；Agent Loop 只完成模型提出工具调用 → 宿主执行 → 结果回灌的循环。 | 工具具体实现；重复的续跑状态机；硬编码的产品规划策略。 | `leveler-engine` + `leveler-agent` |
| **2. ToolHost / 执行边界** | 统一完成 schema 校验、风险与审批、路径约束、可靠的工具起止记录、执行和取消。 | 对话编排、任务完成判断、UI 状态。 | `leveler-tools` + `leveler-execution` |
| **3. 会话状态** | 持久化消息、规范事件、快照与迁移；为恢复提供有序且可审计的事实。 | Agent 策略、隐式业务决策。 | `leveler-storage` + engine 事件日志 |
| **4. 模型适配** | 在薄 provider / 线协议边界之上提供厂商中立的请求、流式与工具调用语义。 | 会话状态、工具权限或工作流决策。 | `leveler-model` + provider 内部协议适配 |

Engine 是长期运行的监督器，不只是循环外壳。它必须能回答：哪些状态已经持久化、
哪个工具可能已经产生副作用、能否安全重试、恢复点在哪里，以及任务为何停止。
规范工具事件的持久化属于副作用协议的一部分：产生外部副作用前必须可靠记录开始，
结束后必须可靠记录结果；仅用于显示的流式 delta 可以走允许丢失的通道。

非核心实现（宜薄、可独立演进）：UI 装饰、命令菜单、多阶段编排栈、插件市场，以及
超出循环与门闩所需的过厚产品轴。**TUI、Web 和手机端虽然不拥有核心执行逻辑，仍是
必须长期支持的一等产品入口和发布验收面，不能按可有可无的附属功能处理。**

## 多端运行时契约

TUI、Web 和手机端通过 `leveler-client-protocol` 与 transport 接入同一个 Engine，
不各自实现任务生命周期。客户端协议必须覆盖：

- 创建、继续、取消任务，以及提交审批或澄清结果；
- 订阅规范运行时事件，并以 snapshot + 水位完成 resync；
- 断线重连和跨端接力，同一个 session 不重复执行、不丢进度；
- session 与项目隔离，任何客户端都不能看到错误项目的事件；
- 协议版本、能力协商、认证和明确的不兼容错误。

关闭或断开任一客户端不应取消已经被 runtime 接受的工作。客户端可以保存视图状态，
但任务事实、权限决定和执行状态只能来自 Engine 的规范事件与快照。手机端额外受配对、
设备撤销和远程审批限制；这些限制不能由前端隐藏按钮代替宿主授权检查。

## 策略与复杂任务

复杂任务不靠扩大 direct loop 来实现。核心提供持久化生命周期、安全执行和明确终止；
规划、证据收集、阶段检查、修复、委派等行为由可替换的 workflow / policy 组合。
默认策略可以保持当前产品行为，但不得成为工具执行或恢复正确性的前提。

```text
复杂任务 / NPC
    ↓
Workflow / Policy       怎么规划、拆解、检查和重试
    ↓
Engine                  生命周期、监督、持久化、恢复和终止
    ↓
Agent Loop              单一模型—工具—结果循环
    ├── Model Runtime    厂商中立模型调用
    └── ToolHost         审批、安全执行和可靠副作用记录
```

长期运行的 NPC 建立在同一个 Engine 之上。身份、长期记忆、定时唤醒、收件箱、世界状态
和角色策略属于 NPC runtime；它们不能复制 direct loop，也不能绕过 ToolHost。这样新增
NPC 或工作流不会产生第二套会话、权限和恢复语义。

## 不可破坏的核心约束

- 所有仓库修改与进程执行只能经过 ToolHost / execution 边界。
- 同一个生命周期状态只能有一个写入者；投影和 UI 只能消费规范事件。
- 已被宿主接受的工作不能因为某个 UI 断开而丢失。
- 恢复不得盲目重放可能已经产生外部副作用的工具调用。
- 每次停止都必须有可判别原因：完成、阻塞、取消、预算耗尽或失败。
- 策略可以替换或关闭，安全、持久化和取消边界不能关闭或绕过。
- 旧配置、旧数据库和旧事件通过显式兼容窗口与迁移处理，不靠猜测式修复。
- TUI、Web 或手机端断开不会改变任务事实；重连只能通过规范 snapshot / resync 恢复。

本文描述的是规范目标架构；与当前代码之间的差距不能按已实现能力宣传。

每个 crate 都 `forbid(unsafe_code)`。应用与 CLI 可用 `anyhow` 补充上下文；
可复用的库 crate 暴露 `thiserror` 类型化错误。

## 组件图

```text
User
  │
  ├── leveler-cli ───────────────┐
  ├── leveler-tui                │
  └── leveler-web（浏览器 UI）    │
          │                      │
          ▼                      ▼
  leveler-client-protocol   leveler-app  ◀── 组合与配置
          │                      │
  leveler-local-transport        ▼
          └──────────────▶ leveler-engine
                                  │
                 ┌────────────────┼─────────────────┐
                 ▼                ▼                 ▼
          leveler-agent                    leveler-verifier
                 │                │                 │
                 ├────────▶ leveler-context         │
                 └────────▶ leveler-tools ◀─────────┘
                                  │
                                  ▼
                         leveler-execution

  leveler-provider ─▶ leveler-protocol ─▶ leveler-model
         │                                      ▲
         └──────────── 由 engine 使用 ──────────┘

  支撑库: leveler-storage, leveler-project, leveler-vcs,
  leveler-lsp, leveler-skills, leveler-memory, leveler-media, leveler-core
```

箭头是概念上的依赖与调用方向。部分组合边通过 trait 表达，便于用确定性假实现测试。

## 运行时流程

### 1. 组合

`leveler-app` 是组合根：解析全局与项目配置、打开存储、构建 provider 与工具注册表、
选择执行策略，并把 engine 接到 CLI 或本地 transport。

环境变量读取集中在配置与应用启动；下游库接收已解析的值，而不是随意读进程环境。

### 2. 模型请求与流式

Agent 产出与 provider 无关的 `ModelRequest`。`leveler-provider` 选择配置的
provider/model；`leveler-protocol` 负责与厂商线格式互转。

流式字节路径：

```text
HTTP 字节流
  → SSE 帧解码
  → 协议 chunk 解码
  → 分片 tool-call 组装
  → ModelEvent 流
  → engine 与 UI
```

SSE 解码接受任意分片。Tool-call 参数拼完整后再做 JSON 解析；非法或截断的 JSON
会报错，**不会**被“修好”成可执行调用。

### 3. Turn 与工具循环

`leveler-engine` 拥有 task/turn 生命周期。**产品侧执行只有一条路径：direct agent
工具循环。** 长任务仍走该路径——goal 模式（`update_goal` 直到 complete 或 blocked）
与可选的 `spawn_agent` 扇出。多阶段 orchestrate 栈（crate、CLI `plan`/`discuss`、
双 session 模式）已移除。日志里遗留的 `orchestrate` kind 会被接受并按 direct 跑。

生命周期词汇在 `leveler-lifecycle` 共享。模型可以提议动作，但状态迁移、资源预算、
取消、权限决策与完成规则由宿主代码拥有。

### 4. 工具与命令执行

`leveler-tools` 定义内置与 MCP 工具的 schema 与分发。参数先 schema 校验再执行。
写操作与命令类工具在必要时串行，避免冲突修改。

**多 Agent。** 父 executor 可在 depth 0 且启用委派时广告 `spawn_agent`。同一模型
轮次内多次调用会并发（默认上限：并发 4、单次 top-level 共 6 次；深度 1）。子
agent 的工具起止会以带 id 归属的 activity 事件上浮给客户端；完整子对话不会写入
父消息列表。详见 `docs/multi-agent.zh-CN.md`。

`leveler-execution` 强制工作区边界、敏感路径规则、审批策略、检查点、进程树取消，
以及可用的 OS 级隔离。文件系统决策使用宿主解析后的路径与可信执行意图；
模型输入不能选择更高权限后端。

平台相关控制包括：

- Windows：Job Objects；在能力可用时配合 AppContainer 与 ACL
- macOS：Seatbelt
- Linux：Bubblewrap

能力探测是显式的。缺少所需隔离后端时，不会谎称“完全沙箱”。

### 5. 校验与完成

`leveler-verifier` 发现或接收 format / build / test 命令，记录证据并分类失败，
再允许任务完成。修复尝试有界，并服从与原 turn 相同的权限与资源限制。

校验与语言无关。Rust / Go / TypeScript 有更深的内置默认；其它栈可在
`.leveler/config.yaml` 的 `verify` 中声明。

- `format`：尽力而为，**不门控**完成
- `build` / `test`：**门控**完成

当 `verify` 中任一字段出现时，**整段替换**语言自动发现计划。

### 6. 持久化与重连

`leveler-storage` 用 SQLite 持久化 session 与运行时状态。本地 runtime 通过
`leveler-client-protocol` 发布归一化事件；TUI 可重连、拉取 snapshot，并从当前
session 继续。

Transport DTO 与内部 engine 类型分离，便于本地协议演进而不暴露存储或 provider 结构。

## 重要边界

### Provider 边界

上层只消费 `ModelRequest` / `ModelResponse` / `ModelEvent` / `ModelError`。
厂商 JSON、SSE chunk、Authorization 头与 endpoint 癖性留在协议层以下。

新增 OpenAI 兼容端点通常只需配置。新线格式应落在 protocol adapter，由 provider
配置选择该 adapter。

### 执行边界

所有仓库修改与进程执行必须经过注册工具与 execution 层。Agent 循环内直接访问
文件系统或进程会绕过审批、检查点、脱敏与取消。

### 持久化边界

密钥可来自环境变量或本地显式 `api_key`，但解析后的凭证与 Authorization 头不得
写入 session 消息、运行时事件、日志或 artifact。写入前会脱敏。

### UI 边界

TUI 渲染 client-protocol 事件并发送命令/交互响应，不拥有 agent 执行。
daemon 模式下关闭 TUI 不会取消已接受的工作；关闭 runtime 才会。

`leveler-web` 是同一接缝上的浏览器 UI：axum 服务把单页应用桥接到
`LocalRuntimeService`（进程内，或经 `leveler web --connect` 接 `leveler serve
--tcp` daemon），通过 token 鉴权的 REST + 一条 WebSocket 通信。它**只绑定
loopback**——`bind` 拒绝非 loopback 地址——且每个入口都要求 256-bit bearer token
（常数时间比较）；前端构建在编译期嵌入。跨机访问（如手机）应经隧道终止 TLS 再转发
到 loopback，而非直接绑定公网地址。详见 `crates/leveler-web/README.md`。

**多项目。** `leveler web` 可以在一个页面聚合多个仓库：当前仓库保持进程内
runtime 不变；其他项目由 per-repo daemon 承载。打开项目时先探测该仓库的
daemon Unix socket（复用已在跑的 daemon，比如用户自己的 `leveler tui`）；
探测不到才 spawn `leveler --repo <path> serve --ready-json <file>`，就绪文件
出现后经 Unix socket 连接——spawn 的 daemon 无需 token。`RouterService`
（自身也是 `LocalRuntimeService`）按 session→项目 映射路由命令、快照与按
会话的事件订阅，REST 与 WS 层看到的仍是单一门面；WS 按连接订阅会话流，
不同会话/项目的标签页互不串台。daemon socket 位于
`<home>/sock/<仓库路径哈希>.sock`——短且稳定（macOS 的 `sun_path` 约 104
字节，装不下深路径仓库的哈希 state 目录）——同时兼任所有权锁：`serve --tcp`
也会绑定它，同一仓库上的第二个 daemon 立即失败，而不是把第一个的活跃 turn
当僵尸回收（TCP 模式的 token 经 `LEVELER_DAEMON_TOKEN` 环境变量传入，永不
走 argv）。项目注册表 `~/.leveler/web-projects.json` 只存仓库路径；web 重启
后仍活着的 daemon 靠 socket 探测重新接管，不信任 pid。

## 扩展点

- **Provider / 协议：** 实现 runtime 与 protocol adapter，或配置兼容 endpoint。
- **工具：** 实现 tool trait、JSON schema、风险与并行属性并注册。
- **MCP：** 配置外部 MCP server，不把其 schema 耦合进核心工具实现。
- **校验：** 在 `verify.format` / `verify.build` / `verify.test` 下声明项目命令。
- **Skills：** 项目 `.leveler/skills/` 或用户目录下的 skills。

## 配置分层

| 层 | 路径 | 作用 |
| --- | --- | --- |
| 全局 | `~/.leveler/config.toml` | 默认模型、provider、MCP |
| 包配置 | `configs/providers/`、`configs/models/` | 可入库的 provider/model 档案 |
| 项目 | `<repo>/.leveler/config.yaml` | 模型覆盖、权限 profile、verify、ignore、只读根、limits |
| 权限 | `~/.leveler/permissions.yaml`、项目 `.leveler/permissions.yaml` | 持久 allow/ask/deny |
| Hooks | `~/.leveler/hooks.yaml`、项目 `.leveler/hooks.yaml` | 工具前后外部命令 |

示例见同目录 `*.example.yaml` 与 `leveler-config-example.yaml`。
全局/包 schema 见 [`configs/example.yaml`](../configs/example.yaml)。

## 仓库导览

- `crates/` — Rust workspace
- `configs/` — provider/model 兼容示例
- `docs/` — 架构与配置示例
- `evals/` — 评测用例
- `migrations/` — SQLite 迁移
- `.github/workflows/` — 跨平台 CI 与供应链检查

英文版：[`ARCHITECTURE.md`](ARCHITECTURE.md)。入口：[`README.zh-CN.md`](../README.zh-CN.md)。
