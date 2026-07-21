# CodeLeveler 架构

本文描述 CodeLeveler 的稳定边界。故意不绑定源码行号和随版本变动的实现细节，
以便仓库演进后仍然可用。

## 设计目标

1. **模型无关运行时。** Provider / 线协议差异不渗入编排、工具或 TUI。
2. **单向依赖。** 面向用户的层组合底层库；基础 crate 不反向依赖应用层。
3. **类型化失败边界。** 库 crate 用独立错误类型区分 provider、协议、工具、执行、存储与校验失败。
4. **确定性安全控制。** 路径检查、权限、限额、取消与校验由宿主代码执行，而不是模型指令。
5. **可恢复的本地状态。** Session 与运行时事件可持久化并恢复，不依赖远程控制面。

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
          leveler-agent   leveler-orchestrator  leveler-verifier
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

### 3. Turn 与编排

`leveler-engine` 拥有 task/turn 生命周期。直接运行驱动一条 agent 循环；编排运行
增加需求抽取、定位、任务图与评审。生命周期词汇在 `leveler-lifecycle` 共享。

模型可以提议动作，但状态迁移、资源预算、取消、权限决策与完成规则由宿主代码拥有。

### 4. 工具与命令执行

`leveler-tools` 定义内置与 MCP 工具的 schema 与分发。参数先 schema 校验再执行。
写操作与命令类工具在必要时串行，避免冲突修改。

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
