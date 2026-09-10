# CodeLeveler 架构

架构的唯一权威文档。`AGENTS.md` 只给出宪法的短版本并指向这里；边界定义、以及当前实现与目标之间的差距，都写在本文。

英文版：[`ARCHITECTURE.md`](ARCHITECTURE.md)，为 canonical 技术文档，本文与之语义一致。

以下内容全部基于 commit `6724268`（31 个 crate）的真实源码与 `cargo metadata` 验证。凡是代码还没到位的地方，直接写明，不把目标当成已完成的现状描述。

---

## 1. 架构原则

过去容易把 CodeLeveler 读成「一个 coding agent，其他都在它下面」。这种读法让 coding 产品成为最高抽象，于是每加一个能力都被往下压进公共 crate 去伺候它。

正确的结构是：

```text
Foundation 提供可复用的 agent runtime 能力。
Harness 定义领域语义。
Product 定义体验、组合与交付。
```

七句话就是全部模型：

```text
Kernel 不懂产品。
Harness 定义领域语义。
Harness 暴露能力，不模拟智能。
Engine 管生命周期，不当 Agent Brain。
Host Authority 独占受控真实副作用。
每一个持久事实只有一个权威 Owner。
机械事实不等于语义满足，更不等于用户验收。
```

简化的含义是把复杂度搬到正确的 Owner，绝不是删掉可靠性。`persist-before-forward`、ownership fence、CAS 写入、stale-write 保护、沙箱、审批、崩溃恢复，一个都不削弱。

### 1.1 模型能力策略

CodeLeveler 过去在架构之外还背着第二个目标：拉平模型能力差距，用额外的 Harness / 工具行为去托住较弱的模型。**这个目标现在撤销。** 不是推迟，也不是有条件保留。

```text
模型拥有推理。
Harness 拥有领域语义与能力暴露。
Runtime 拥有机械正确性。

CodeLeveler 不试图拉平模型智能。
```

```text
                  Model
                    │
                    │ 推理 / 规划 / 工具选择
                    ▼
                 Harness
                    │
                    │ 领域语义 / 工具面
                    ▼
                 Runtime
                    │
                    │ 确定性执行 / 权威
                    ▼
                   Host
```

```text
模型智能是系统的输入，不是运行时不变量。
```

**因此系统欠模型什么。** 清晰的原语、小而好懂的 schema、确定性语义、精确的错误、有界的输出、真实的仓库状态、类型化的机械证据、快而可靠的执行。剩下的交给模型：规划、导航、选工具、编辑、调试、从自己的错误里恢复。

**不欠什么。** 猜一个畸形参数原本想表达什么、失败后悄悄改变工具语义、因为模型选错就替它换一个工具、隐藏的任务级重试、只为让较弱模型更好用而复制出来的第二个工具、因为模型不会规划或不会判断下一步而搭起来的规划 / 评审框架。机械校验不在此列，仍然必须做：JSON 与 schema 校验、路径规范化、真实兼容需要的别名、provider 协议适配。

**分界线是归属，不是难度。**

```text
工程失败     → Runtime 负责。
模型能力上限  → 模型自己负责。
```

机器故障、并发、文件系统竞争、进程死亡、协议与网络失败、安全风险、宿主状态非法，这些是工程失败。本文档里所有可靠性属性都是为它们存在的，这次一个都不削弱：`persist-before-forward`、F7 Grounded Authority、ToolHost 准入、permission、审批、ownership fence、沙箱、路径安全、CAS、stale-write 保护、原子变更、rollback 及其冲突保护、崩溃恢复、取消、持久化、进程生命周期、EvidenceLedger、多 Agent 写安全。

**Provider 能力不是模型智能。** 是否支持 tool calling、streaming、reasoning 传输、vision、结构化输出、强制 tool choice，以及上下文/输出上限和 wire format，这些是协议事实，属于 `leveler-model`、`leveler-protocol`、`leveler-provider` 和能力协商。诚实地协商并上报；缺少某个必需的机械能力，就关掉依赖它的能力并说明。不要从协议差异里长出第二条行为路径——provider 不支持强制 tool choice，就如实上报，而不是发明另一套推理策略去假装它支持。

**错误要精确，不要替模型做计划。** 说清楚什么失败了、为什么、违反了哪条机械约束、现在实际可用的是什么：*文件不是合法 UTF-8*、*路径不存在*、*pattern 不是合法正则*、*写入被拒绝，因为观察到的版本已过期*、*结果超过配置上限*。除非那套步骤本身就是机械契约，否则不要给模型一份多步恢复方案。

**Fallback 仍然允许，语义补偿不允许。** 第二套实现产出**完全相同**的契约、且等价性被机械证明，这是正常工程。改变这次调用**含义**的 fallback 才是被禁止的：正则搜索退化成字面量扫描、patch 失败后变成另一种编辑、结构化操作失败后换一个解法。

**不要把智能分级写进架构。** `weak` / `strong` / `small` / `large` 不是 Foundation 概念。一个已配置的模型，要么具备所需的机械能力，要么不具备。

**判定标准。** 一个特性必须至少满足其一才能加入或保留：它是 canonical 的 coding 能力；它实质改善受支持模型的交互；它提供确定性的运行时正确性；它提供安全或权威；它在不改变语义的前提下实质提升效率；它回应一个被证明的产品需求。「较弱模型需要它」「这样不同模型行为才一致」「这是为了补偿模型弱点」都不在列表里，也不再是保留既有架构的正当理由。

---

## 2. 分层模型

```text
┌─────────────────────────────────────────────────────────────┐
│                          产品层                              │
│   CodeLeveler        Review 产品         未来产品            │
│   app / cli / tui / web / remote / relay                    │
└──────────┬──────────────────────┬───────────────────────────┘
           │                      │
           ▼                      ▼
┌─────────────────────────────────────────────────────────────┐
│                        Harness 层                            │
│   Coding Harness                 Review Harness             │
│   leveler-agent                  （未来）leveler-review      │
│   ├─ tool host（准入）            ├─ 自己的 tool host         │
│   └─ 工具面选择                    └─ 自己的工具面             │
└──────────┬──────────────────────────┬───────────────────────┘
           │                          │
           └────────────┬─────────────┘
                        ▼
┌─────────────────────────────────────────────────────────────┐
│                      Agent Runtime                          │
│   leveler-agent-core                                        │
│   ToolRuntime { definitions, execute }  ← 工具边界           │
└─────────────┬──────────────────────┬────────────────────────┘
              │                      │
              ▼                      ▼
┌──────────────────────┐  ┌──────────────────────────────────┐
│ 持久化 Runtime        │  │ 可复用能力                        │
│ engine / storage     │  │ context / project / vcs / lsp    │
│ lifecycle            │  │ browser / memory / skills / media│
└──────────┬───────────┘  └───────────────┬──────────────────┘
           │                              │
           └───────────────┬──────────────┘
                           ▼
┌─────────────────────────────────────────────────────────────┐
│                      Host Authority                         │
│                    leveler-execution                        │
└──────────────────────────┬──────────────────────────────────┘
                           ▼
                       操作系统
```

有两处，运行中的代码还对不上这张图：

- **Engine 在 Harness 之上，不在它旁边。** `leveler-engine` 依赖 `leveler-agent`，公开 API 里直接出现 Coding 概念。见 §18.1。
- **Tool 适配器与能力实现在同一个 crate。** `leveler-tools` 两者都装，而且好几个工具自己实现了能力行为。见 §5 与 §18。

图里其余部分就是真实依赖形状。

---

## 3. Foundation 原语

| Crate | 职责 |
| --- | --- |
| `leveler-core` | 类型化标识符、时间戳、资源预算、少量基础 trait。无内部依赖。 |
| `leveler-model` | provider 中立的模型词汇：`ModelRequest`、`ModelResponse`、`ModelEvent`、`ModelError`，以及 `ModelRuntime` trait。 |
| `leveler-protocol` | 厂商 wire 协议适配（OpenAI Chat Completions 形状、SSE 解码）。不知道任何 transport 和 agent。 |
| `leveler-provider` | provider 配置、模型目录、带重试的 HTTP transport、实现 `ModelRuntime` 的 `ProviderRegistry`。 |
| `leveler-lifecycle` | 执行生命周期词汇：`SessionStatus`、`TaskOutcome`、`VerificationStatus`、`TurnOutcome`，以及 Coding workflow 类型。无内部依赖。 |

这几个 crate 不得知道 coding、review、finding、仓库工作流、TUI、Web、CLI 或任何产品概念。`leveler-model` 目前知道——见 §18.6。

`leveler-lifecycle` 内部已经做好了切分：`runtime` 模块领域中立，`workflow` 模块放 Coding 词汇，且禁止 `runtime` 引用 `workflow`。

---

## 4. Agent Kernel

`leveler-agent-core` 是产品中立的 agent kernel。它的非 dev 依赖只有 `leveler-model` 一个。

它拥有：

```text
model ↔ tool 循环        round 管理
流式输出                 预算（round / token / 成本 / 时长）
重试与退避               用量与成本记账
deadline                取消
中立的 stop reason       唯一的 tool dispatch 接缝
```

它不拥有：

```text
coding 语义       review 语义        仓库语义
prompt 语义       持久化             UI
验证的含义                          任务完成语义
用户验收          产品策略
```

Kernel 与宿主之间的全部契约就是 `AgentHarness` trait：循环在每一轮的固定位置调用每个接缝，harness 返回一个 `Flow`。除了「声明有哪些工具」和「执行工具」这两个接缝，其余都有中立默认实现。

Kernel 从不判断模型的工作是好、是完成、还是可接受。一次运行的结束点只有三种：模型自己停、宿主让它停、机械上限让它停。

用产品词汇扫描整个 crate，命中全部落在文档注释里，而且每一条都是在说「这件事归别人管」。**Kernel 今天是干净的。**

---

## 5. Tool 架构

### 5.1 Foundation 的工具边界已经存在

本文的上一版把 `leveler-tool-core` 写成目标：从 `leveler-tools` 里抽出一个新 crate，装 tool trait、schema、registry 和 dispatch 契约。

**这个目标现在撤销。** 重读源码后可以确认，Foundation 的工具边界已经在那里了，而且比那个提议中的 crate 更小：

```rust
// leveler-agent-core::tool_runtime
pub trait ToolRuntime: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;
    async fn execute(&self, call: ToolCall, cancellation: CancellationToken)
        -> Result<ToolOutcome, ToolRuntimeError>;
}
```

两个方法。crate 自己的注释把其余契约说清楚了：「kernel 不知道一个工具是怎么被授权、沙箱化或执行的……准入、权限、workspace、副作用持久性，属于宿主。」

所以：

```text
leveler-agent-core::ToolRuntime  =  Foundation Tool Boundary
```

第二个 harness 实现这两个方法，然后拥有它们背后的全部。它不需要 `leveler-tools`，不需要 `Tool` trait，也不需要 `ToolRegistry`。

> **不要为了架构对称再造一个通用 Tool Core。**
> `leveler-tool-core` 是一个**被否决的提案**，不是延后的工作。只有在真的出现第二个消费者、**且** `ToolRuntime` 确实不够用时，才重新讨论。

### 5.2 Tool 是什么

```text
Tool 是能力面向模型的适配器。
Tool 不是实现该能力的运行时。
```

Tool 拥有：

```text
面向模型的 name / schema / description
参数解码与兼容性修复
能力调用
面向模型的结果渲染
机械正确性确实需要的那一点点内在执行元数据
```

Tool 不拥有：

```text
服务发现              权限解析
全局策略              审批编排
ownership 管理        持久性
持久状态所有权         进程生命周期
workspace 事务运行时   LSP 生命周期
浏览器生命周期         provider 配置
runtime 证据存储       UI 状态
```

一句话规则：**Tool 薄，Capability 厚。** 这里的「厚」不是指一个庞大的 service，而是指领域实现要有一个明确的 Owner，且那个 Owner 不是面向模型的适配器。

```text
ReadFileTool      → WorkspaceReader
GrepTool          → WorkspaceSearch
ApplyPatchTool    → WorkspaceEditor
RunCommandTool    → CommandExecution
FindSymbolTool    → CodeIntelligence
BrowserClickTool  → BrowserRuntime
ViewImageTool     → Media
WebSearchTool     → Search provider
```

### 5.3 Capability 是职责边界，不一定是 crate

```text
Capability != crate
```

`WorkspaceReader`、`WorkspaceSearch`、`WorkspaceEditor`、`CommandExecution`、`CodeIntelligence` 是架构职责。它们可以实现成模块、结构体、现有 crate 的子系统或一个 service，代码怎么自然怎么来。

不要为了让图对称就去建 `leveler-workspace-search`、`leveler-workspace-editor`、`leveler-code-intelligence`。拆 crate 需要：两个真实消费者、真实的依赖倒置、独立的安全/运行时/协议边界，或者观察到的耦合缺陷。在第二个实现出现之前，优先用具体 struct 而不是 trait。

### 5.4 ToolHost 准入，Host Execution 执行

```text
ToolHost admits.
Host Execution performs.
```

Coding 的 tool host 是 `crates/leveler-agent/src/executor/host.rs`。它是「模型提议的调用」变成「执行」的唯一路径，而且这一点是被代码强制的，不只是写在文档里：

```text
副作用屏障 → pre-hooks → 权限规则 → profile 策略
→ auto-review / 审批 → 再次屏障 → 执行
```

准入产出 `AdmittedCall`，是 `dispatch` 唯一接受的值——没有准入就执行，根本通不过类型检查；而且 `crates/leveler-agent/tests/tool_host_boundary.rs` 会在本 crate 里任何其他文件直接碰 `registry.execute` 或 hook gate 时失败。屏障跑两次是故意的：审批结果必须先持久化，才能授权它所允许的副作用。

`leveler-execution` 负责执行：文件系统强制、进程执行、沙箱、路径安全、宿主进程机制。

`ToolRegistry`、`Tool` 实现、Capability 实现，都不得再开第三条权限或审批路径。今天 `ToolRegistry` 开了——见 §5.6。

### 5.5 ToolContext：现状与目标

当前形状：

```text
ToolContext = ExecutionResources + ToolPolicy + ToolServices + session_scope
```

`ToolServices` 把 `lsp_sessions`、`lsp_start_locks`、`artifact_store`、`memory_root`、`background_tasks`、`browser` 写成结构体字段。每个工具都会收到全部，无论用不用。

这让 `ToolContext` 同时是 service locator、策略容器、执行容器和会话能力容器。作为债务记录在 §18.2。

目标：

```text
Tool 依赖显式注入。
一个 Tool 只收到它需要的那个能力。
每次调用的 context 只携带真正动态的调用状态，或宿主为这次调用签发的授权。
```

**现在不要设计替代品。** 不要 `BetterToolContext`，不要 `ToolExecutionContextV2`，不要 `CapabilityContext`。目标是 `ToolContext` 作为能力抽离的**结果**自然缩小或消失，而不是先发明一个新容器。

### 5.6 ToolRegistry：现状与目标

今天 `ToolRegistry::execute` 依次做了：read-only 强制、zero-write-authority 强制、权限模式强制、参数归一化、JSON schema 校验、dispatch、以及集中的输出预算截断。模块还持有 observe-class 名单、read-only 子集、MCP 过滤、`core`/`full` 组合和 `expand_tool_category`。

这是一个挂着 registry 名字的策略引擎。

目标：

```text
ToolRegistry
    register
    lookup
    definitions
    schema 校验
    适配器 dispatch
```

其余各归其位：

| 关注点 | 目标 Owner |
| --- | --- |
| 工具选择、work profile、只读集合、动态能力选择 | Harness |
| 权限、审批 | ToolHost |
| ownership、写入范围 | ToolHost / runtime |
| 结果预算 | Harness / runtime 结果处理 |

不要让 Registry 再变回策略引擎。

### 5.7 工具分层图

```text
                         MODEL
                           │
                           ▼
┌──────────────────────────────────────────────────┐
│                AGENT KERNEL                      │
│              leveler-agent-core                  │
│ model loop / retry / budget / cancel             │
│ ToolRuntime { definitions, execute }             │
└───────────────────────┬──────────────────────────┘
                        ▼
┌──────────────────────────────────────────────────┐
│               HARNESS TOOL HOST                  │
│ 准入 / 授权 / 审批                                │
│ ownership / 持久化屏障                            │
│ 执行调度 / runtime 证据                           │
└───────────────────────┬──────────────────────────┘
              ┌─────────┴──────────────┐
              ▼                        ▼
┌──────────────────────────┐  ┌──────────────────────────┐
│ 能力工具适配器             │  │ Harness 控制工具          │
│ read_file  grep          │  │ update_goal              │
│ apply_patch run_command  │  │ update_plan              │
│ browser_*  git_*         │  │ request_user_input       │
│ …                        │  │ request_permissions      │
│                          │  │ spawn_agent              │
│                          │  │ claim_write_scope        │
│                          │  │ report_finding           │
└─────────────┬────────────┘  └──────────────────────────┘
              ▼
┌──────────────────────────────────────────────────┐
│                  CAPABILITIES                    │
│ Workspace 读/搜索        Workspace 编辑           │
│ 命令执行                 Code Intelligence        │
│ Browser Runtime         VCS      Memory          │
│ Skills   Media   Web/Search   MCP Runtime        │
└───────────────────────┬──────────────────────────┘
                        ▼
┌──────────────────────────────────────────────────┐
│               HOST EXECUTION                     │
│              leveler-execution                   │
│ 文件系统 / 进程 / 沙箱 / 路径安全                  │
│ 进程生命周期 / 受控宿主副作用                       │
└──────────────────────────────────────────────────┘
```

右边那一列 Harness 控制工具**在代码里已经是分开的**：它们住在 `crates/leveler-agent/src/injected_tools.rs`，不在 registry 里。`update_plan` 是例外，它和能力适配器一起待在 `leveler-tools`。见 §18.5。

---

## 6. Tool Surface Policy（模型工具面策略）

### 6.1 原则

```text
在不损失能力的前提下，暴露最小的工具面。
```

每一个面向模型的工具都要花：schema token、tool-choice 熵、需要模型自己消歧的重叠语义、路由错误、策略分支、replay 语义、测试面。能力数量和模型工具数量是两个不同的数字，只有第二个是每一轮都要付的成本。

```text
能力很多  ≠  模型每轮看到很多 Tool
```

一个工具值得上工具面，要在下面这几问上够得着「是」：

```text
它是否表达了一个独立的模型意图？
它是否暴露了一个独立的能力？
它的语义是否确定？
它是否替称职的模型省掉了本来要付的 round trip？
它是否实质提升任务成功率或效率？
```

同时在下面这几问上是「否」：

```text
它是否和别的工具重叠、增加 tool-choice 熵？
现有原语能否干净地表达同一个操作？
这件事到底该不该归模型控制，还是归运行时或用户？
```

目标是**最大化有用的模型自主性，同时最小化 Harness / 工具复杂度**（§1.1）。「这对较弱模型有帮助吗」不是标准，也不再被问。

### 6.2 模型今天看到什么

从本次 commit 的 `crates/leveler-tools/src/registry.rs` 实测：

| 集合 | 数量 |
| --- | --- |
| `core_registry()` | 14 |
| `full_registry()` = core + 17 + 12 个 browser | 43 |
| 执行器注入的 Harness 控制工具 | 7 |
| `default_registry()` | 就是 `full_registry()` |

所以默认工具面在五十个量级。`core_registry()` 由 economy work profile 选中，`expand_tools` 让模型在会话中途要更多。

有两个事实值得点名，因为它们和直觉相反：

- **`find_files` 不在 `core_registry()` 里。** economy 工具面有 `grep` 和 `list_files`，但要通过 `expand_tools("search")` 才拿得到 `find_files`。如果 `find_files` 是核心原语——从模型意图的角度看它是——那这是一个工具面组合的错误，不是能力缺失。
- **`replace`、`shell_command`、`update_plan`、`load_skill`、`expand_tools`、`memory` 都在 core 里。** economy 工具面不是「原语集合」，它是一份历史选择。

### 6.3 核心原语基座（Core Primitive Foundation）

七个操作是架构基线。它们之所以是原语，是因为它们表达了最基本的编码操作，而不是因为某个模型需要人扶：

```text
read    ls    find    grep    edit    write    bash
```

当前 CodeLeveler 的映射：

| 原语 | 工具 |
| --- | --- |
| `read` | `read_file` |
| `ls` | `list_files` |
| `find` | `find_files` |
| `grep` | `grep` |
| `edit` | `apply_patch`——canonical 结构化编辑 |
| `write` | `write_file`——整文件落地 |
| `bash` | `run_command` / `shell_command` |

「整文件写入」和「局部编辑」是两个不同的模型意图——新建或有意替换整个文件，对改动文件的一部分——所以两个都是原语。这个区分是稳定的，与「模型能不能造出 patch」无关。

明确一句：

```text
core_registry()  !=  核心原语基座
```

`core_registry()` 是实现与历史的产物——economy work profile 恰好选中的那一组——在单独的 Tool Surface Closure 把它对齐之前，不要拿其中一个当另一个的定义。

### 6.3.1 四个类别

**核心能力工具**——上面的核心原语基座。

`run_command` 和 `shell_command` 是否都留在模型面上，是关于「意图是否独立」和「是否好做安全分析」的语义问题（§6.5），不是关于模型强弱的问题。

**可选能力包**——真实能力，但不必每轮都可见：

```text
Code Intelligence   find_symbol、read_symbol、find_references、
                    diagnostics、blast_radius
VCS                 git_status、git_diff
Browser / Web       browser_*（12 个）、web_fetch、web_search
Media               view_image
Memory              memory、remember
Skills              load_skill
后台进程             get_task、wait_task、kill_task
```

「实现存在」不是「默认暴露」的理由。

**Harness 控制工具**——Coding harness 的控制协议，不是可复用能力：

```text
request_user_input（别名 ask_user）      update_goal
update_plan                            request_permissions
spawn_agent                            claim_write_scope
report_finding
```

Review harness 在这一格会是另一套。这正是重点。

**扩展工具**——MCP 发现的工具。MCP 协议与运行时，和 `McpTool` 适配器，概念上必须分开。

### 6.4 谁决定工具面

```text
Harness 决定这个产品有哪些工具。
```

它根据 work profile、任务类型、已配置的能力和模型 profile 来决定。Kernel 对此一无所知。

这就是 `expand_tools` 在架构上可疑的原因：它把 ownership 反过来了，让模型请求运行时给它更多工具。代价是一次额外工具调用、一个额外 round、动态 registry 状态、更多 replay 与控制语义。除非 Eval 证明省下的 schema token 在成功率和成本两边都盖过那个额外 round，否则 harness 侧选择才是模式，`expand_tools` 不是。

### 6.5 工具面价值是 Eval 决策，不是审美决策

```text
不能仅因为「另一个工具理论上能做同样的事」就删掉一个工具。
```

只有当证据显示以下之一时才删除或降级：成功率没有可测量的提升、语义显著重叠、带来额外路由错误、造成不必要的复杂度，或者这个能力本就属于运行时/用户而不属于模型。

这说的是**产品工具面**，不是实现正确性。实现本身机械上就是错的——语义不确定、第二条通往文件系统的路径、含义随环境改变——那是直接修的，见 §6.6。

Eval 候选，以及各自上榜的理由：

| 工具 | 为什么是候选 |
| --- | --- |
| `expand_tools` | 反转了工具面 ownership；用一个 round 去买 schema token。**强删除候选**。 |
| `create_checkpoint` / `restore_checkpoint` | 运行时本就维护 checkpoint 与恢复。要求模型判断何时该 checkpoint 增加认知负担，还把运行时设施变成了工具语义。除非有明确产品需求，否则应退出默认模型工具面。 |
| `consolidate_memory` | 属于 memory 子系统维护，不是 coding 能力。可以后台跑、会话结束时跑、周期跑，或由用户显式命令触发。 |
| `forget` | 基于模型的语义判断去破坏性修改持久状态，而模型并不处在做这个判断的位置。删除持久 memory 更接近用户动作。 |
| `create_skill` | 系统定制 / 元编程能力。`load_skill` 可以留作可选；「创建」不该是默认 coding 任务里的顺手选项。 |
| `blast_radius` | 高级 Code Intelligence 操作（references → 外层符号 → BFS），不是原语。先移入可选包，再 Eval 它是否真的减少 round 或提升重构召回。 |
| `read_symbol` | `find_symbol` + `read_file` 可复现。只有当它可测量地省 token 或省 round 时才该留。纯 Eval 问题。 |
| `replace` | **REMOVE / SURFACE-EVAL 候选。** 它原本的理由——给较弱模型一条精确 find/replace 路径，避免在 patch 上下文匹配上反复失败——已经撤销（§1.1），不再是保留它的理由。剩下的是一个普通问题：`replace` 是否表达了 `edit` + `write` 覆盖不到的、独立且普遍有用的编码原语？就按重叠度判断；删除它不以「弱模型 A/B」为前提。 |
| `shell_command` vs `run_command` | `run_command` 是 program+args：跨平台、无 shell 引号问题、安全分析简单。`shell_command` 是模型最熟悉的字符串形式。`shell_command` 已经复用 `run_command` 的 `execute_program`，所以「两个适配器、一个能力」在架构上完全没问题。模型面是否只留一个，是 Eval 问题。 |
| `git_status` / `git_diff` | 用 `run_command` 也能做，但 git 检视是高频动作，不需要 shell，输出稳定，可 replay。接口暂时保留；实现迁到 `leveler-vcs`。 |

`list_files` 和 `find_files` **不是**合并候选。它们表达不同的模型意图——「这个目录里有什么」对「仓库里哪里有符合这个模式的文件」。真正该统一的是它们底下的文件系统遍历和 ignore 语义。

### 6.6 两个不同的问题，两种不同的举证责任

原来那条规则——动任何工具之前先测量——太宽了。它把「读代码就能定论」的重构也拖进了「先做使用基线」的仪式。

```text
架构正确性由机械证据确立。
产品工具面价值在价值确实不确定时由 Eval 确立。
```

**直接修，不需要 A/B。** 靠检查就能证明的缺陷就是缺陷：归属错误、语义不确定或随环境改变、运行时实现重复、跨平台行为分叉、service-locator 耦合、运行时能力长在 Tool 适配器里。修这些改变的是代码**是什么**，不是产品**提供什么**。

**先测量。** 当两个都合理的原语发生重叠、当某个可选能力的价值不清楚、当删掉一个工具可能实质改变产品行为时，先从真实会话取一次使用基线：哪些工具真的被调用、成功率与重试率如何、它们的存在是否提升任务成功。`replace`、`read_symbol`、`blast_radius`、`expand_tools` 和 checkpoint 工具是这份测量的优先名单。

---

## 7. Host 执行权威

`leveler-execution` 是宿主副作用的唯一权威。它拥有：

```text
workspace 路径解析与强制         进程执行
权限 profile 与规则              进程树终止
审批策略与 approver              沙箱后端
风险分级                         hooks
可回滚的 checkpoint              信任门禁
超大输出的 artifact 存储          后台任务注册表
```

原则：

```text
Agent 请求副作用。
Host Authority 执行副作用。
```

### 实测状态

统计 `leveler-execution` 之外、生产代码（排除测试）中 `fs::write`、`fs::remove`、`fs::create_dir`、`fs::rename`、`Command::new`、`tokio::process` 的调用点：

| Crate | 生产调用点 | 解读 |
| --- | --- | --- |
| `leveler-agent` | 0 | Coding harness 不直接产生任何副作用。 |
| `leveler-context` | 0 | 只读装配。 |
| `leveler-vcs` | 0 | 每次 git 调用都走 execution 的 runner。 |
| `leveler-tools` | 13 | `replace.rs` 经 `context.execution.workspace` 写入（root fd、防符号链接替换）；`mcp.rs` 直接拉起用户配置的 MCP server。 |
| `leveler-browser` | 15 | driver 安装写在 Leveler home 下；driver 进程直接 spawn。 |
| `leveler-memory` | 11 | 在 Leveler home 下写 memory 存储。 |
| `leveler-lsp` | 4 | 直接 spawn language server。 |
| `leveler-engine` | 3 | 用 `git rev-parse` 打 baseline commit。 |
| `leveler-skills` / `leveler-project` | 2 / 1 | 在 Leveler home 下建状态目录。 |

这张表里其实是两类东西：

1. **模型请求的、作用在用户仓库上的副作用。** 全部走 `Workspace` 和 `CommandRunner`。`leveler-agent` 是 0，这个数字才是关键。
2. **Runtime 自己的状态与 sidecar。** 写在 Leveler home 下的内容，以及长驻 sidecar 进程（MCP server、language server、浏览器 driver），不走权限/审批路径——因为它们不是模型要求的。

第二类是真实且有意为之的边界，但这些 sidecar 确实在 `CommandRunner` 的进程树终止与沙箱语义之外。见 §18.4。

---

## 8. 可复用能力

| Crate | 能力 |
| --- | --- |
| `leveler-context` | 有界的仓库上下文装配：map、候选文件、相关测试、合并后的项目规则、token 估算、重复读防护。 |
| `leveler-project` | 项目语言识别与文件布局。 |
| `leveler-memory` | 持久项目记忆存储与其晋升流水线。 |
| `leveler-skills` | Skill 发现与加载。 |
| `leveler-vcs` | Git 操作，经执行权威落地。 |
| `leveler-lsp` | language server 会话，跨工具调用复用。 |
| `leveler-browser` | 浏览器运行时、driver 安装、按项目隔离的 profile。 |
| `leveler-media` | 有内容类型保障的图片导入：从内容判定真实 MIME、解码与像素上限、剥离 EXIF、降采样、内容寻址存储。 |

Coding Harness 选 context、project、VCS、LSP、browser、memory、文件写入与进程执行。Review Harness 大概率只需要 context、project、VCS、LSP、只读文件系统和 memory。

`leveler-media` 目前只有一个消费者 `leveler-app`。`view_image` 工具没有用它。见 §18.3.F。

---

## 9. 持久化 Runtime

`leveler-engine` 是持久化 runtime。它拥有：

```text
session 与 task 生命周期        checkpoint 与 resume
turn 边界                      崩溃恢复与 reaper
事件顺序                       ownership 注册表
append-only 事件日志            runtime outcome
persist-before-forward         上下文窗口策略
```

真正关键的是 `persist-before-forward`：一个 turn 的事件按发出顺序先落日志，客户端才看得到。所以客户端永远不可能观察到一个 runtime 没有持久记录过的事实。

Engine 不应该成为 agent brain。它不该拥有 coding prompt、review prompt、工具选择、仓库策略，也不该定义某个领域里「完成」是什么意思。

Engine 自己的边界工作做了一半。`TaskSpec` 已经拆开：

```rust
pub struct TaskSpec {
    pub runtime: RuntimeTaskSpec,   // goal、kind、continuation、limits
    pub coding: CodingTaskSpec,     // repository、权限模式、sandbox、
                                    // 验证计划、base commit
}
```

源码里自己把这称为「通往领域中立 engine 的迁移接缝」。它还没有消除对 Coding 的依赖——见 §18.1。

---

## 10. 存储与持久事实

`leveler-storage` 是持久事实的边界：SQLite、内嵌 migration、连接池，每个关注点一个 repository。业务逻辑从不直接发 SQL。

```text
一个持久事实
    → 只有一个权威 Owner
    → 只有一份 canonical 持久表示
```

任务状态、turn 状态、证据、ownership、用量、artifact、完成状态，无一例外。

`leveler-storage` 只依赖 `leveler-core` 和 `leveler-lifecycle`。这正是低层持久化 crate 能讲生命周期词汇、又不需要反向依赖高层 crate 的原因。

---

## 11. 验证与证据

`leveler-verifier` 跑项目声明的检查（格式、构建、测试），采集证据，检查范围，对失败分类。

```text
Verifier 是「验证结论」的权威。
Verifier 不是「语义任务完成」的权威。
```

它能证明配置的检查通过、失败还是被阻塞。它无法单独证明用户的意图已被满足。

用户显式声明的验证命令是权威，不是启发式输入。

这就是为什么 `TaskOutcome` 和 `VerificationStatus` 在 `leveler-lifecycle` 里是两条正交的轴。Runtime 两个都报，绝不合成一个词。

`leveler-verifier/src/lib.rs` 的 crate 级注释目前还是相反的说法。见 §18.8。

---

## 12. Harness 层

`leveler-agent` 就是 **Coding Harness**。crate 名字没有改，本文也不主张改名；重要的是概念。

它拥有 Coding 领域语义：

```text
coding prompt                    压缩策略
仓库上下文策略                    goal 语义
coding 工具选择                   coding 验证策略
工具准入（ToolHost）               委派策略与子 agent profile
写入工作流                        coding 完成契约
跨 agent 的路径 ownership          harness 控制工具面
```

它通过唯一一个接缝接到 kernel：`src/executor/drive.rs` 里的 `Drive` 实现了 `leveler_agent_core::AgentHarness`，填的接缝是 `tool_definitions`、`on_round_start`、`on_round_admitted`、`on_response`、`on_model_error`、`on_quiet`、`execute_calls`、`on_stop`、`on_event`。这就是 kernel 契约的全部，已经被一个真实 harness 用起来了。

未来的 Review harness 是**兄弟**：

```text
              leveler-agent-core
                /            \
               ▼              ▼
        leveler-agent    leveler-review
        Coding Harness   Review Harness
        自己的 ToolHost   自己的 ToolHost
        自己的工具面       自己的工具面
```

`leveler-review → leveler-agent` 这条边是禁止的，而且**不要求**复用 Coding 的工具管路——Review 用自己的方式实现 `ToolRuntime`。

Review 会拥有自己的词汇——`ReviewTarget`、`ReviewScope`、`ReviewPolicy`、`Finding`、`FindingSeverity`、`FindingEvidence`、`FindingLifecycle`、去重、抑制、`ReviewVerdict`、`ReviewReport`——这些类型一个都不许进 agent kernel。

以上不构成任何要做 Review harness 的承诺。它是对「Foundation 允许假设什么」的约束。

---

## 13. 产品层

| Crate | 角色 |
| --- | --- |
| `leveler-app` | 组合根：配置、provider registry、数据库、把引擎事件投影为客户端事件。 |
| `leveler-cli` | 命令行界面。 |
| `leveler-tui` | 终端客户端。只依赖 client protocol、core、model、skills。 |
| `leveler-web` | Web 客户端。 |
| `leveler-client-protocol` | UI 与 runtime 之间的稳定契约：`ClientCommand` 进、`RuntimeEvent` 出、带版本信封。 |
| `leveler-local-transport` / `leveler-remote-protocol` / `leveler-remote-agent` / `leveler-relay` | 本地与远程 transport，以及配对/中继路径。 |
| `leveler-eval` | 能力评测框架。无内部依赖。 |
| `leveler-test-support` | 共享测试夹具，仅作 dev-dependency。 |

这些层投影权威 runtime 状态，不推导新的 runtime 真相。一次工具调用返回 `Ok`，不构成客户端断定任务完成的依据。

---

## 14. 依赖方向

目标规则：

```text
Foundation
    ↑
Capabilities / Runtime
    ↑
Harnesses
    ↑
Products
```

当前依赖图，按拓扑层级排列（只统计 normal dependency）：

| 层级 | Crate | 内部依赖 |
| --- | --- | --- |
| 0 | `leveler-core`、`leveler-lifecycle`、`leveler-memory`、`leveler-media`、`leveler-eval` | 无 |
| 1 | `leveler-model`、`leveler-project`、`leveler-skills`、`leveler-browser`、`leveler-execution` | `core` |
| 1 | `leveler-storage` | `core`、`lifecycle` |
| 2 | `leveler-agent-core` | `model` |
| 2 | `leveler-protocol`、`leveler-client-protocol` | `core`、`model` |
| 2 | `leveler-context` | `core`、`project`、`skills` |
| 2 | `leveler-lsp` | `core`、`project` |
| 2 | `leveler-vcs` | `core`、`execution` |
| 2 | `leveler-verifier` | `core`、`execution`、`lifecycle`、`project` |
| 3 | `leveler-provider` | `core`、`model`、`protocol` |
| 3 | `leveler-tools` | `browser`、`context`、`core`、`execution`、`lsp`、`memory`、`model`、`project`、`skills` |
| 3 | `leveler-local-transport`、`leveler-remote-protocol`、`leveler-tui`、`leveler-web` | client protocol 及以下 |
| 4 | `leveler-agent` | `agent-core`、`context`、`core`、`execution`、`lifecycle`、`memory`、`model`、`skills`、`tools` |
| 4 | `leveler-relay`、`leveler-remote-agent` | remote protocol 及以下 |
| 5 | `leveler-engine` | `agent`、`context`、`core`、`execution`、`lifecycle`、`model`、`storage`、`tools`、`verifier` |
| 6 | `leveler-app` | 18 个内部 crate |
| 7 | `leveler-cli` | 21 个内部 crate |

结论：

- **不存在反向依赖。** 没有任何 crate 依赖 `leveler-app`、`leveler-cli`、`leveler-tui` 或 `leveler-web`。
- `leveler-agent-core` 只依赖一个内部 crate。
- `leveler-agent → leveler-execution` 是**词汇**边：harness 用的是 `PermissionProfile`、`RiskLevel`、`WriteScope`、`HookRunner` 这些类型，它直接产生副作用的调用点是 0。
- `leveler-tools` **不**依赖 `leveler-media`，这正是 `view_image` 自己重新实现图片处理的原因（§18.3.F）。
- `leveler-engine → leveler-agent` 是唯一一条与分层模型冲突的边（§18.1）。

---

## 15. 一次 turn 的运行流程

```text
客户端命令
    │
    ▼
leveler-app                  组合；把配置映射成运行中的 Application
    │
    ▼
leveler-engine               turns 行、turn id 打戳、
    │                        persist-before-forward 的 EventLog、
    │                        把 approver 与 clarifier 包成 recorder
    ├─ ExecutorFactory       唯一一份执行配置推导
    ▼
leveler-agent（Drive）        Coding harness：prompt、上下文、工具面、
    │                        委派、压缩、goal 语义
    ▼
leveler-agent-core           循环：准入一轮 → 组装 model round → 流式 →
    │                        解析 → ToolRuntime::execute → 下一轮
    ▼
leveler-agent（ToolHost）     屏障 → hooks → 规则 → 策略 → 审批 →
    │                        屏障 → AdmittedCall
    ▼
leveler-tools                适配器执行
    │
    ▼
leveler-execution            workspace 解析、权限、审批、
    │                        风险、沙箱、进程执行
    ▼
操作系统
```

回传方向：

```text
事件 → EventLog（先持久化） → 引擎事件 → leveler-app
     → 客户端事件 → leveler-client-protocol → TUI / Web / 远程
```

事实先落盘再外流。

---

## 16. 真相与权威模型

```text
机械事实  ≠  语义满足  ≠  用户验收
```

**Runtime 可以权威证明：** 命令执行了、退出码是多少、文件被修改了、测试结果、构建结果、artifact 存在、事件被持久化了、工具返回了这个结果、观察到的 runtime 状态。

**Verifier 可以权威判定：** 配置的验证结论。

**但这两者都不能自动推出下一步。**

```text
「工具成功了」    不能推出    「目标在语义上被满足了」
「测试通过了」    不能推出    「用户的请求被完成了」
```

```text
Runtime 拥有机械事实。
Model 拥有语义解释。
User 拥有验收。
```

不要重新引入把机械观察当语义完成的捷径。代码库里现在没有这种判定，而 `TaskOutcome` 与 `VerificationStatus` 保持正交，正是把它挡在外面的机制。

对工具的一个推论：模型读到的东西和 runtime 记录的事实是两回事，任何一方都不许悄悄变成另一方。一个在把字节展示给模型之前就把它改写成别的文本的工具，改的是事实本身，不只是呈现（§18.3.A）。

---

## 17. Second Harness Test

> 一个语义上不同的 agent 产品（比如 Review），能否**在不修改 agent kernel 的前提下**建在这套 Foundation 上？

目标答案：

| 问题 | 要求的答案 |
| --- | --- |
| 修改 `leveler-agent-core` | NO |
| 复用 `ToolRuntime` | YES |
| 复用 model 契约 | YES |
| 复用持久化 runtime | YES |
| 复用选定的能力 | YES |
| 复用 Coding 的 `ToolRegistry` | NOT REQUIRED |
| 复用 Coding 的 `Tool` trait | NOT REQUIRED |
| 依赖 `leveler-tools` | NOT REQUIRED |
| 依赖 `leveler-agent` | NO |

核心要求：

```text
Review 必须能在同一个 Agent Kernel 之上，建立它自己的 Harness ToolHost
和它自己的工具面。
```

Foundation 不要求所有 Harness 使用同一套工具管路。

### 当前结论：NOT YET ENFORCED

Kernel 和工具边界这两侧是过的。`leveler-agent-core` 只依赖 `leveler-model`，不带产品词汇，`ToolRuntime` 只有两个方法，Review harness 可以用自己的方式实现。`AgentHarness` 已经被真实 harness 实现过。

还没过的：

- Review harness 若想要持久化、resume、事件顺序和恢复，就得走 `leveler-engine`，而它依赖 `leveler-agent`，公开 API 里还有 `CodingTaskSpec`（§18.1）。
- `leveler-model` 知道 Coding 内置工具的名字和执行分类，所以 Review 的工具集会继承一套为 Coding 写的词汇（§18.6）。

这两条都不强制 kernel 变更，所以结论是「尚未强制成立」，不是「失败」。

**不要为了把这个结论改成 PASS，在一次文档变更里去动代码。**

---

## 18. 已知边界债务

记录下来，不掩盖。每一条都写清当前行为、期望边界、为什么违宪、最小修正、以及风险。

### 18.1 Engine 依赖 Coding Harness

**当前。** `leveler-engine → leveler-agent`。Engine 公开 API 导出 `CodingTaskSpec`，`ExecutorFactory` 直接构造 `leveler_agent::Executor`。`recorders.rs`、`recovery.rs`、`turn.rs`、`policy_resolver.rs` 都在点名 `leveler_agent` 的类型。

**期望。** Engine 通过一层抽象驱动 harness executor，不点名任何领域。

**为什么违宪。** 违反规则 5 和规则 2：第二个 harness 会经由 engine 继承到 Coding harness。

**最小修正。** `RuntimeTaskSpec` / `CodingTaskSpec` 的拆分已经作为迁移接缝存在。下一步是给 engine 一个不必点名 `leveler_agent` 的 executor 抽象，并把 `ExecutorFactory` 上移。

**风险。** 中。`ExecutorFactory` 被刻意设计成执行配置的唯一推导入口；拆得不好会把它当初要消除的「多份推导」bug 放回来。

### 18.2 ToolContext 是通用 service locator

**当前。** 每个工具都收到 `ExecutionResources + ToolPolicy + ToolServices + session_scope`，其中 `ToolServices` 把 `lsp_sessions`、`lsp_start_locks`、`artifact_store`、`memory_root`、`background_tasks`、`browser` 列为字段。

**期望。** 显式依赖注入；工具只收到它需要的能力；每次调用的 context 只带动态调用状态或宿主签发的授权。

**为什么违宪。** 工具拥有了本不该有的服务发现能力，而一个完全用不上这些的 Review 专属工具仍然得接下整个形状。

**最小修正。** 让它作为能力抽离的结果自然缩小。不要先设计替代容器。

**风险。** 跟在抽离之后做，低；抢在抽离之前发明一个 `V2` 容器，高。

### 18.3 具体工具实现债务

以下每一条，都是工具在自己实现本应调用的能力行为。

#### A. `read_file` 做得太多，其中两条策略本身也不对

`crates/leveler-tools/src/tools/read_file.rs` 目前拥有：文件读取、分页、二进制检测、全文件指纹、重复读策略、stale-write 前置状态、输出预算、模型引导。

已验证的后果：

- **窄 range 读仍然扫全文件。** 源码自己写着：「完整流式读一遍文件：这让窄 range 在内存上保持 O(行长)，同时仍产出 stale-write 保护需要的全文件指纹和分页文案要用的总行数。」内存有界，时间是 O(文件)。
- **超过 10 MB 的文件被直接拒绝**，在考虑 range 之前就拒了，然后告诉模型「用 `grep`……或者 `run_command` 配 sed/head/tail 切片」。对一个 100 MB 文件做有界的 50 行读是合理请求，工具却做不到；而且建议的替代方案不跨平台——考虑到本项目支持 Windows，这个问题是具体的。
- **非法 UTF-8 被静默改写。** 每行用 `String::from_utf8_lossy` 渲染，非法字节以 `U+FFFD` 到达模型。二进制保护只扫前 8 KB 找 NUL，所以一个「非 UTF-8 但开头 8 KB 没有 NUL」的文件会走 lossy 路径。磁盘上的字节和展示给模型的文本不一致，而且没有任何提示。
- **重复读策略住在读工具里。** 模型是不是在浪费 round 重复读同一段，这是 harness 的行为策略，不是文件系统读语义。
- **stale-write 追踪住在读工具里。** `read_file` 把指纹记进 `FileStateTracker`，好让 `apply_patch` 之后能拒绝过期写入。这正是窄读也必须扫全文件的原因：读，为未来可能的编辑承担了前置状态。

**目标契约**（行为，不是 API）：

```text
读是有界的。
读语义是确定的。
窄 range 读不应要求无关的全文件工作，除非机械正确性证明确有必要。
读不拥有编辑策略。
读不拥有重复读策略。
读不拥有结果预算策略。
非法文本或二进制数据要如实报告，而不是静默改写成另一段文本。
大文件应当能通过有界读来查看，而不是仅仅因为体积大就必须改用 shell 命令。
```

**目标 ownership。** `ReadFileTool → WorkspaceReader`，返回有界的结构化读结果。stale-write 保护变成「workspace 读观测」与 `WorkspaceEditor` 之间的显式机制，而不是藏在读工具里的副作用。重复读提醒移交 harness，它本来就看得到工具历史。

**修正风险。** 中。stale-write 保护是真实的安全属性，搬家时必须原样保留，不能为了让读更快而削弱它。

#### B. Workspace 搜索的语义随环境变

`list_files`、`find_files`、`grep` 和符号回退扫描各自带着一套目录遍历、ignore 规则和结果上限。

更严重的是 `grep` 会随机器改变查询语义：装了 `rg` 时 pattern 是正则；没装时，内置回退把它当字面子串匹配。工具在 pattern 看起来像正则时确实会追加一行 `[note] ripgrep unavailable …`，所以不算静默——但同一个工具调用在不同机器上仍然是不同的意思。

**目标。** 在 macOS、Linux、Windows 上一套确定的 workspace 搜索语义，`list_files`、`find_files`、`grep` 底下共用一套遍历和 ignore 规则。

**风险。** 中。内置一个正则引擎会改变现有用户的匹配结果，这个变更需要是有意为之并且被公告的。

#### C. Workspace 编辑逻辑在两个工具里重复

`apply_patch` 和 `replace` 各自带着 stale 保护、CAS、原子修改、回滚和 diff/证据。

**目标。** `ApplyPatchTool` 和 `ReplaceTool` 都调用同一个 `WorkspaceEditor`。CAS、stale 保护、回滚、all-or-nothing 行为原样保留，只换 Owner。

**风险。** 中高。这是一旦有 bug 就会损坏用户文件的路径。只能在现有测试保护下搬，且同一步内不做行为变更。

#### D. `run_command` 拥有了命令执行的大部分

`run_command` 目前承担沙箱、环境、网络策略、后台进程、快照、mutation 记账、写入范围、回滚、命令闸门、进程生命周期，以及自 `6724268` 起，把 HEAD 移动所解释的路径从「这次运行写了什么」里减去。

**目标。** `RunCommandTool` 和 `ShellCommandTool` 是 `CommandExecution` 能力之上的适配器，该能力再调用 `leveler-execution`。

**风险。** 中。取消与进程树终止语义不得改变。

#### E. Code Intelligence 的生命周期住在工具里

`find_symbol`、`read_symbol`、`find_references`、`diagnostics`、`blast_radius` 各自包含 LSP 发现、会话生命周期、启动和回退扫描。

**目标。** 一个 `CodeIntelligence` 能力统一拥有 LSP 生命周期、符号查询、引用、诊断和确定性回退。工具只做面向模型的调用。

**风险。** 低到中。

#### F. `view_image` 重复实现了 `leveler-media`，而且更弱

`view_image` 根据扩展名判断 MIME，读字节，base64 编码。`leveler-media` 从内容判定真实 MIME，对解码分配和像素数设上限以防解压炸弹，通过重新编码剥离 EXIF，对超大图降采样，并做内容寻址存储。

`leveler-tools` 根本不依赖 `leveler-media`，而 `leveler-media` 唯一的消费者是 `leveler-app`。也就是说：用户附件路径是加固的，面向模型的工具路径不是。

**目标。** `ViewImageTool → Media 能力`。

**风险。** 低。这一条已经接近纯缺陷。

#### G. `web_search` 拥有 provider 配置

工具自己从环境读 `LEVELER_SEARCH_API_KEY`、`LEVELER_SEARCH_PROVIDER`、`LEVELER_SEARCH_CX`，自己建 HTTP 客户端，并且自己实现了 Bing 和 Google Custom Search 两套请求与响应形状。

**目标。** `WebSearchTool → Search 能力 / provider`。工具不知道 provider 凭据和配置。

**风险。** 低。

#### H. `wait_task` 拥有 workspace 结算

`WaitTaskTool` 刻意不是 `RiskLevel::Safe`，因为它在 wait 结束时会跑 `account_background_mutations`，可能把整个 workspace 恢复到快照。它自己的源码注释写明了这一点。

一个名字叫「wait」的模型工具，不应该拥有 workspace 回滚语义。

**目标。** 由 `BackgroundTaskRuntime` 拥有结算。`get_task` 和 `wait_task` 只读取或等待状态。

**风险。** 中。后台任务结算和 dev-server 安全规则相互纠缠，那些规则的存在是有原因的。

### 18.4 Sidecar 进程绕开 CommandRunner

**当前。** MCP server（`leveler-tools/src/mcp.rs`）、浏览器 driver（`leveler-browser/src/driver.rs`）和 language server（`leveler-lsp/src/client.rs`、`registry.rs`）都是用 `Command::new` 直接拉起，不经 `leveler_execution::CommandRunner`。

**期望。** 要么让它们进入宿主权威的进程树终止与沙箱语义，要么把这个豁免写成一条显式命名的策略——「runtime sidecar」——并给它确定的生命周期 Owner。

**为什么部分违宪。** 违反规则 4。它们不是模型请求的命令，所以权限与审批不适用；但它们仍是宿主进程，而宿主权威本应拥有每一个宿主进程。

**风险。** 低到中。Sidecar 存活期已经和 daemon 关停时的 reaping 缠在一起。

### 18.5 `update_plan` 和能力适配器待在一起

**当前。** 七个 harness 控制工具里有六个住在 `crates/leveler-agent/src/injected_tools.rs`。`update_plan` 却注册在 `leveler-tools` 的 `core_registry()` 里，和 `read_file`、`grep` 并排。

**期望。** Harness 控制工具是 harness 的控制协议，应该跟 harness 在一起。

**为什么违宪。** 未来的 Review harness 若取用 Coding 的能力适配器，会连 Coding 的 plan 协议一起继承过去。

**最小修正。** 把它移到其他注入工具旁边。

**风险。** 低。

### 18.6 `leveler-model` 知道 Coding 工具名

**当前。** `crates/leveler-model/src/tool_catalog.rs` 硬编码了 `grep`、`find_files`、`find_symbol`、`read_symbol`、`find_references`、`list_files`、`read_file`、`git_status`、`git_diff`、`view_image`、`web_search`、`web_fetch`、`apply_patch`、`replace`、`run_command`、`shell_command`，并由此推导执行分类（`Search` / `Read` / `Write`）、replay 安全性、主参数和 observe key。

它的真实消费者是 `leveler-agent`（observe key）、`leveler-client-protocol`（safe-replay 判定）和 `leveler-tui`（展示）。crate 注释把动机说得很清楚：「执行策略不应在多个 crate 之间重复名单和参数字段猜测。」动机成立，位置不成立。

**期望。** `leveler-model` 只知道 `ToolDefinition`、`ToolCall`、`ToolResult`、`ToolChoice`。内置 Coding 工具的元数据属于 harness，或属于拥有这些工具的 tool composition。

**为什么违宪。** 直接违反规则 1。一个 Foundation 原语在枚举产品工具，于是每一个建在 `leveler-model` 上的 harness 都继承了一套 Coding 词汇。

**最小修正。** 把 catalog 移到拥有该工具集的那一层，并给三个消费者一条不经 Foundation 原语的取用路径。另外注意，导出的 `is_search_tool` 在 crate 之外没有调用者。

**风险。** 中。三个跨层消费者目前共享这一份；替代方案不能变成三份同样的名单。

### 18.7 `ToolOutput.metadata` 是无类型内部侧信道

**当前。**

```rust
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub metadata: serde_json::Value,
}
```

生产者在 `leveler-tools` 写入字符串键的 JSON；消费者在 `leveler-agent/src/executor/dispatch.rs` 按键读回：`plan`、`image`、`applied_diff`、`executed_commands`、`modified_files`、`outcome`，以及工具扩展请求。生产者和消费者在不同 crate，只靠一个字符串约定连接。

四类完全不同的东西挤在同一个字段里：

```text
模型输出   ≠   Runtime 事实   ≠   Harness 控制   ≠   UI 载荷
```

`modified_files` 是驱动验证义务的 runtime 事实。`plan` 是 harness 控制。`image` 是模型可见内容。`applied_diff` 是 UI 与证据。全都没有类型，键名打错会静默失败。

**期望。** 这四类在类型系统里区分开。

**现在不要设计最终的 enum。** 形状应该跟在能力抽离之后，而不是抢在前面。

**风险。** 中。它一次性触及每一个工具和 dispatch 路径，所以应该是最后一步而不是第一步。

### 18.8 Verifier 的注释宣称拥有完成权

**当前。** `crates/leveler-verifier/src/lib.rs` 开头写着「只有 verifier 能标记任务完成」。

**期望。** Verifier 是验证结论的权威。

**最小修正。** 改写这段注释。代码早就把两条轴分开了，注释停留在拆分之前。

**风险。** 无。它只是注释。

### 18.9 Engine 直接 shell 调 git

**当前。** `leveler-engine/src/baseline.rs` 和 `engine.rs` 直接 `Command::new("git")`，而 `leveler-vcs` 存在且直接 spawn 数为 0。

**期望。** Engine 问 VCS 能力，VCS 问宿主权威。

**风险。** 低。

---

## 19. 开放设计问题

记录下来，免得被第一个碰到它的人悄悄定掉。

### 19.1 工具结果是否该携带类型化的 part？

Kernel 的工具结果是：

```rust
pub struct ToolOutcome { pub content: String, pub is_error: bool }
```

而 model 词汇已经支持 `ContentPart::Image`。今天一张图要经由 `ToolOutput.metadata` 传出去，再由 harness 重新挂回（§18.7）。

问题：kernel 的工具结果是否应该支持类型化的 text/image part？

约束：**不要为了优雅而修改 agent kernel。** 只有当 provider 协议支持、并且有真实的图片或多模态用例证明 metadata 路径不够用时，才动它。

### 19.2 canonical 编辑工具是一个还是两个？

`apply_patch` 是 canonical 结构化编辑，`write_file` 是 canonical 整文件写入，`replace` 与两者都重叠。它当初成立的理由——在 patch 上下文匹配反复失败时给模型一条精确 find/replace 路径——已经撤销（§1.1）。剩下的是一个普通的工具面问题：`replace` 是否表达了 `edit` 和 `write` 覆盖不到的独立模型意图？现在的默认答案是「否」。

### 19.3 模型控制的动态工具扩展划得来吗？

`expand_tools` 用一个额外 round 加动态 registry 状态去买 schema token。Harness 侧选择用同样的 token 收益换来零动态状态，代价是不能会话中途自适应。只有 Eval 能定哪个更值，而且举证责任在动态方案这一边，因为是它反转了 ownership。

---

## 20. 架构变更规则

1. **先在 Foundation 之上扩展，再考虑改它。**
2. **改 Foundation 需要证据**，不是优雅：两个真实实现、一个真实的依赖倒置边界、一处观察到的耦合或 ownership 缺陷，或一条独立的协议 / 安全 / 持久化 / runtime 边界。
3. **动手前先回答 `AGENTS.md` 里的架构决策测试**，写在 PR 里，不是事后补。
4. **不要把目标写成现状。** 如果一次变更朝某个边界推进但没到位，去更新 §18，而不是删掉那一条。
5. **不要为不存在的产品预建接口。**
6. **不要靠删可靠性来简化。** 要把复杂度搬到正确的 Owner。§18 里点名的每一条安全属性都必须在搬家后完好无损。
7. **本文是唯一 canonical 架构文档。** 不要新建 `ARCHITECTURE_V2.md`、`FOUNDATION_*.md` 或所谓 final 版本。

Roadmap 类内容——多 agent 方向、浏览器方向、Review 产品、云、ACP、远程 worker、NPC 工作流、未来 provider、未来 UI——都不是架构。本文可以描述扩展点，但不承诺功能。

---

## 21. 文档状态

```text
FOUNDATION_ARCHITECTURE_DEFINED   YES
TOOL_ARCHITECTURE_DEFINED         YES
MODEL_CAPABILITY_POLICY_DEFINED   YES
WEAK_MODEL_COMPENSATION_GOAL      REMOVED
CORE_PRIMITIVE_FOUNDATION_DEFINED YES

TOOL_IMPLEMENTATION_ALIGNED       NO
ENGINE_IMPLEMENTATION_ALIGNED     NO
CORE_PRIMITIVE_FOUNDATION_ALIGNED NO

SECOND_HARNESS_TEST               NOT_YET_ENFORCED
FOUNDATION_FROZEN                 NO
```

架构和工具边界已经定了。实现还没对齐，本文写明了差在哪里。没有为了让这张表里任何一行好看而修改源码。
