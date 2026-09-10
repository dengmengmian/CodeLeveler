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

这几个 crate 不得知道 coding、review、finding、仓库工作流、TUI、Web、CLI 或任何产品概念。`leveler-model` 以前知道——它带着一份 Coding 工具名表——现在不知道了（§18.6）；`crates/leveler-tools/tests/ownership_boundaries.rs` 是那根绊线。

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
ShellCommandTool  → CommandExecution
FindSymbolTool    → LspSessions
GitStatusTool     → GitWorkflow
BrowserTabTool    → Browser
ViewImageTool     → leveler_media::process_image
MemoryTool        → MemoryStore
McpTool           → McpClient
WebSearchTool     → Tavily，构造时注入 key
```

箭头是**构造函数**，不是查找。每个工具只持有自己箭头左边那一个句柄，别的都拿不到——见 §5.5。

### 5.3 Capability 是职责边界，不一定是 crate

```text
Capability != crate
```

`WorkspaceReader`、`WorkspaceSearch`、`WorkspaceEditor`、`CommandExecution`、`CodeIntelligence` 是架构职责。它们可以实现成模块、结构体、现有 crate 的子系统或一个 service，代码怎么自然怎么来。

不要为了让图对称就去建 `leveler-workspace-search`、`leveler-workspace-editor`、`leveler-code-intelligence`。拆 crate 需要：两个真实消费者、真实的依赖倒置、独立的安全/运行时/协议边界，或者观察到的耦合缺陷。在第二个实现出现之前，优先用具体 struct 而不是 trait。

五个全部落地，而且就是字面意义上的具体 struct——没有 trait、没有 registry、没有新 crate：

| 职责 | 落在哪 |
| --- | --- |
| `WorkspaceReader`、`WorkspaceSearch`、`WorkspaceEditor` | `crates/leveler-tools/src/workspace/`（crate 私有） |
| `CommandExecution` | `crates/leveler-tools/src/tools/command_execution.rs` |
| `CodeIntelligence` | `leveler_lsp::LspSessions`——拥有 `LspClient` 的那个 crate |
| VCS | `leveler_vcs::GitWorkflow` |
| Media | `leveler_media::process_image` |
| Browser | `leveler_browser::Browser`，每种协议一个 `BrowserBackend`（CDP、WebDriver） |
| Memory | `leveler_memory::MemoryStore` |
| Web search | Tavily，直接调用。一个后端只写一处，前面不加接缝（§18.3 G） |
| MCP | `leveler_tools::mcp::McpClient` |

`CommandExecution` 留在 `leveler-tools` 而没有下沉到 `leveler-execution`：它要从 `ToolContext` 上读这次调用的授权，并返回 `ToolOutput`，而位于工具层之下的 `leveler-execution` 不能依赖这两个类型。它是一个两个命令工具都被注入的模块，谁也不拥有谁——以前 `shell_command` 是 import `run_command` 的内部实现来跑自己的运行时的。

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

`ToolRegistry`、`Tool` 实现、Capability 实现，都不得再开第二条权限或审批路径。`ToolRegistry` 以前开了一条——read-only 覆盖层、zero-write-authority 拒绝、profile 硬禁止都在那里被重新判断了一遍，于是一个构建里有三个必须互相同意的授权 owner。三条现在都在 `resolve_policy` 里，`crates/leveler-tools/tests/ownership_boundaries.rs` 会在任何一条重新出现在 registry 时失败。

有一种宿主根本无法强制的授权，现在是明确拒绝而不是假装执行：MCP 工具是一个不受沙箱约束的独立进程，所以一次「禁网」的运行会拒绝它，而不是让它在一条到不了它的禁令下跑。同样的理由早就让 MCP 对被委派的子 agent 不可用——它也无法遵守子 agent 声明的写入范围。

### 5.5 ToolContext

```text
ToolContext = ExecutionResources + ToolPolicy + session_scope
```

- `ExecutionResources`——这次调用所锚定的执行底座：写入范围要以其 root 解析的 workspace、进程 runner、回滚 checkpoint、读取指纹、工作区级命令闸门。进程级 `Arc`，整个 run（含子 agent）只有一份。
- `ToolPolicy`——这次调用的授权：实时权限 profile、只读覆盖层、写入白名单与预算，以及 ToolHost 在准入时冻结的 `ResolvedExecutionPolicy`。
- `session_scope`——这次调用属于哪个会话。

**它不携带任何 Capability 句柄。** 以前有第三个 facet `ToolServices`，装着语言服务器池、浏览器运行时、memory root、artifact store、后台任务注册表。每个工具都会拿到这六个：`read_file` 被递上了浏览器，`grep` 可以启动语言服务器。那就是 service locator，而 locator 对谁都会应答。

现在每个工具在构造时只拿它真正使用的句柄：

| Tool | 构造时注入 |
| --- | --- |
| `find_symbol`、`read_symbol`、`find_references`、`diagnostics`、`blast_radius` | `leveler_lsp::LspSessions` |
| `run_command`、`shell_command` | 共享的 `CommandExecution` |
| `get_task`、`wait_task`、`kill_task` | `BackgroundTaskRegistry` |
| `memory`、`remember`、`forget` | memory store root |
| `browser_tab`、`browser_act`、`browser_inspect` | `leveler_browser::Browser` |
| 核心读/搜/编辑工具、`git_*`、`view_image`、`load_skill`、`web_*` | 除 context 外什么都不要 |

这些句柄以 `leveler_tools::Capabilities` 的形式交给组装方，由组装根持有、被 `model_surface` 消费一次。一个与某能力无关的工具，没有任何途径能拿到它；`crates/leveler-tools/tests/ownership_boundaries.rs` 会在句柄重新出现在 context 上时失败。

有两个 **Runtime 自己需要**的事实也搬回了 Runtime，而不是搭在某个工具的句柄上：executor 自己持有它用于每轮记忆召回、以及在无人审批时寄存 `remember` 的 memory root；`ExecutorFactory` 自己持有 engine 在终态清算时回收后台进程用的注册表。

**反增长规则。** 不允许新增顶层字段，而且一个新能力根本不是候选——它去构造那些使用它的工具。能住在这里的只有「本次调用的授权」或「本次调用的身份」，并且必须在评审里说出它的 owner。

### 5.6 ToolRegistry

```text
ToolRegistry
    register
    lookup
    definitions
    normalize_input
    schema 校验
    dispatch
    唯一的结果上限
```

`ToolRegistry::execute` 做四件事：归一化参数、按工具的 JSON Schema 校验、dispatch、截断结果。它不判断这次调用是否可以发生——那由「持有 `AdmittedCall`」来证明，而只有 ToolHost 能产出它。

参数处理留在这里，而这不是 policy：JSON 与 schema 校验、字段别名、路径语法归一化、历史兼容别名、精确的非法参数报错，都是机械处理。Registry 永远不许做的是：猜一个畸形调用的意图、替换成另一个工具、改变调用的语义。

结果上限是机械的运行时保证，不是调用方可以谈判的预算：每一次 dispatch 都被截到该模型的结果预算，工具不能豁免，而且这样的上限只有一个。某个 Capability 仍可以有自己的**内在**上限——`run_command` 会把超长输出溢写到 artifact store 并回传一个恢复定位串——这两者是不同的东西：内在上限说的是这个能力能产出什么，中央上限说的是模型上下文装得下什么。

搬走了什么，搬去了哪：

| 关注点 | 现在的 Owner |
| --- | --- |
| 只读覆盖层、profile 硬禁止、zero-write-authority 拒绝 | ToolHost（`resolve_policy`） |
| 到底有哪些工具存在 | Harness 组装根 |
| Capability 句柄 | 每个工具，在构造时 |
| Harness 控制 | `leveler_agent::register_harness_controls` |

不要让 Registry 再变回策略引擎。`crates/leveler-tools/tests/ownership_boundaries.rs` 是那根绊线。

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
│ Browser                 VCS      Memory          │
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

右边那一列 Harness 控制工具在代码里是分开的：它们住在 `crates/leveler-agent`，不在 tool crate 里。其中七个在循环内部由注入的 `ToolDefinition` 回答（`injected_tools.rs`）；`update_plan` 是一个注册的 `Tool`（`update_plan.rs`），因为它有真实结果要渲染，由 `register_harness_controls` 在能力 pack 之后放上工具面。它复用 registry 那唯一一条机械接缝——归一化、schema 校验、dispatch、结果封顶——而不是重新实现一遍。

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

工具面是**组合出来的**，不是继承来的。`leveler-app` 在回合开始前，按这台机器实际具备的能力组合一次：

```text
CORE                 永远在
HARNESS CONTROLS     执行器注入，各有各的条件
OPTIONAL PACKS       该能力的前置条件成立时
EXTENSIONS           已配置 MCP server 的工具
```

| 集合 | 数量 |
| --- | --- |
| `core_surface(&capabilities)` | 11 |
| `model_surface(CapabilityPacks::ALL, &capabilities)` = core + 26 | 37 |
| 同上，但宿主没有搜索 key | 36 |
| Harness 控制（`update_plan` + 1–7 个注入，按条件） | 2–8 |
| MCP 扩展 | 按配置 |

两个数字都重要。一个 pack 需要产品要它**并且**宿主能提供它，而对 `web_search` 来说「能提供」就是手上有 key——所以没有 key 的宿主组合出 36 个，而 `default_registry()` 存在的全部意义就是「不需要宿主给任何东西」，它正是其中之一。

一个 pack 只有在「产品模式启用了它」**且**「这台宿主能提供它」时才到模型面前（§6.4）。两个答案都绝不关于任务或模型：

| Pack | 工具 | AVAILABLE 条件 | ENABLED 条件 |
| --- | --- | --- | --- |
| Code Intelligence | `find_symbol`、`read_symbol`、`find_references`、`diagnostics`、`blast_radius` | 恒真——扫描 fallback 不需要装任何东西 | 非 Economy |
| VCS | `git_status`、`git_diff` | `PATH` 上有 `git` | 非 Economy |
| Web fetch | `web_fetch` | 恒真 | 非 Economy |
| Web search | `web_search` | `LEVELER_SEARCH_API_KEY` 里有一个非空的 Tavily key | 非 Economy |
| Media | `view_image` | 模型 profile 声明了 `vision` | 非 Economy |
| Memory | `memory`、`remember`、`forget` | 恒真——app 会把 store root 交给工具 | 非 Economy |
| Skills | `load_skill` | 恒真 | 非 Economy |
| Browser | `browser_tab`、`browser_act`、`browser_inspect`（3 个） | 本机**选中**的那个浏览器——先看调用、再看 `[browser].default`、最后看系统默认——是它能驱动的 | 非 Economy |

`WorkProfile::Economy` 启用 `CapabilityPacks::NONE`：只有原语和协议。这是用户对成本的决定，不是对任务难度的推断——而且**无论这台机器多强都成立**。一台装了浏览器运行时、配了搜索 key、用着视觉模型的笔记本，Economy 回合看到的仍然是 11 个原语加控制。

**「机器支持」不再等于「暴露给模型」。** 两个答案是两个输入不同的函数，交集是它们唯一能到达模型的方式：`Application::capability_availability`（机械事实）、`Application::capability_selection`（产品选择）、`CapabilityPacks::intersect`。

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

`core_registry()` 和 `full_registry()` 曾经是这个问题的历史答案，而且两者并不是同一个集合。它们已经没了：现在的组合是 `core_surface(&capabilities)` 加显式的 `CapabilityPacks`（§6.2），所以「原语基线」和「模型可见工具面」是分开陈述的，谁也不从谁推导。

`get_task` / `wait_task` / `kill_task` 也在 core surface 里，但不是第八个原语：`run_command` 能起后台任务，而一个调用方既看不见也停不掉的任务就是孤儿——它们是那个原语的生命周期。core surface 里没有任何 Harness 控制——控制不是宿主可以关掉的能力，所以由 `leveler_agent::register_harness_controls` 单独加上（§18.5）。

### 6.3.1 五个类别

每一个面向模型的能力，最终只能落在其中一个类别里。

**CORE**——上面的核心原语基座，加上它必然带来的：

```text
read_file  list_files  find_files  grep  apply_patch  write_file
run_command  shell_command
get_task  wait_task  kill_task
```

`run_command` 能起后台任务，而一个调用方看不见也停不掉的任务就是孤儿——所以这三个生命周期工具属于命令原语本身，不是可选能力。

**OPTIONAL CAPABILITY PACKS**——真实能力，带真实前置条件，由 Harness 从宿主事实组合（§6.2）。「可选」不等于「藏在模型触发的发现机制后面」，而是「这个能力在，产品就把它放出来」。

**HARNESS CONTROLS**——Coding harness 自己的控制协议，不是可复用能力。`crates/leveler-agent/src/injected_tools.rs` 在 registry 之外注入，各有条件：

```text
request_user_input（别名 ask_user）    永远
request_permissions                   非 full-access
spawn_agent                           配置了委派，且 depth < 上限
claim_write_scope                     有写权限的子 Agent 回合
report_finding                        子 Agent 回合
update_goal                           goal 模式
```

`update_plan` 属于这一类，也注册在这一类（`leveler-agent`），见 §18.5。

**EXTENSIONS**——MCP 发现的工具，从已配置的 server 注册。这是**唯一**的扩展边界，而且刻意是一条进程边界：MCP server 跑在自己的进程里、用 stdio 上的 JSON-RPC 说话，只以 `McpTool` 适配器的形式到模型面前，并且像任何其他调用一样要过 ToolHost 准入。没有原生插件 SDK，也不打算有——第三方进程内插件就是「跑在 runtime 里、拥有 runtime 权限的代码」，那正是准入机制存在的理由。

这条边界的代价，直说：MCP server 在 OS 沙箱之外、在任何 claimed write scope 之外，所以有两种授权无法对它强制，而这两种情况下调用都是**拒绝**而不是在一条到不了它的授权下执行——被委派的 agent 完全不能用 MCP，禁网的运行也不能。在受限 profile 下每次 MCP 调用都需要审批；长期信任只能来自权限规则，绝不来自配置默认值。

**根本不是模型工具**——这些实现过、W1 把它们从工具面移除、这一轮删掉了。一个没被注册的 `Tool` 实现就是一个等着被重新注册的第二答案：

```text
create_checkpoint / restore_checkpoint   运行时本来就在每次写入前 checkpoint，
                                         并且拥有回滚与崩溃恢复
consolidate_memory                       memory 子系统维护
create_skill                             系统定制：技能由用户创建，模型只加载
```

`run_command` 和 `shell_command` 是否都留，是关于「意图是否独立」和「是否好做安全分析」的语义问题（§6.5），结论是都留。

### 6.4 谁决定工具面

```text
Harness 决定这个产品有哪些工具。
```

三个必须分开的问题：

```text
AVAILABLE   这台机器到底能不能提供这个能力？
            装了浏览器运行时、配了搜索 key、PATH 上有 git、模型能读图

ENABLED     当前产品模式 / session 是不是要它？

EXPOSED     模型真正看到什么 = ENABLED ∩ AVAILABLE
```

**浏览器的联网授权来自暴露本身。** 其它每一个会拨网络的能力都在 `execute` 里复查 `network_denied`，因为对进程内的 `reqwest` 来说那是唯一执行点。浏览器不查，而这个「不查」是决定本身，不是遗漏：这个能力存在的意义就是打开真实页面——dev server、staging、文档站——去验证用户自己的浏览器会看到什么。一个只能去策略逐次放行之处的浏览器，是另一种、有用得多的能力；而要把那套强制做成真的（正向代理、DNS pin、按页 grant、两套协议各自的请求拦截），CodeLeveler 就得再拥有并维护第二套网络运行时。

所以规则只在能力面上写一次：

```text
Browser Pack EXPOSED  =  浏览器网络出口已授权
```

localhost、局域网 dev server、公网都可达；点击触发的跳转、页面里的 `fetch`、WebSocket、子资源加载，行为都和用户自己的浏览器一致。只留下一条导航目标规则，而它不是网络边界：`browser_tab navigate` 拒绝 link-local 和云实例元数据地址，因为把浏览器指向 `169.254.169.254` 是一次穿着 URL 外衣的凭证读取。它不约束、也不声称约束一个已经打开的页面在做什么。

「可用」本身什么也不买。`Economy` 不启用任何可选 pack，所以一台装了浏览器运行时的机器，在普通 Economy 回合里看到的浏览器工具是零个。反过来也一样：要一个这台机器做不到的能力也什么都不买——没有搜索 key 的宿主不会暴露 `web_search`，无论产品模式多想要。

两边都不能放大另一边，这才让交集是一条**边界**而不是一句建议（`crates/leveler-tools/tests/capability_composition.rs`）。

代码里：`Application::capability_availability` 只用机械事实回答第一个问题，`Application::capability_selection` 用 work profile 回答第二个，`CapabilityPacks::intersect` 产出第三个，`model_surface(packs, &capabilities)` 在回合开始前组合一次。之后 `register_harness_controls` 加上控制——控制不是宿主可以关掉的能力。

三者都不查模型能力，也不查任务难度（§1.1）。`expand_tools` 把这个 ownership 反了过来，现在已删除；架构上的反对本来就站得住，而实现层面它其实从来没有真正工作过（§6.5）。

### 6.5 工具面价值是 Eval 决策，不是审美决策

```text
不能仅因为「另一个工具理论上能做同样的事」就删掉一个工具。
```

只有当证据显示以下之一时才删除或降级：成功率没有可测量的提升、语义显著重叠、带来额外路由错误、造成不必要的复杂度，或者这个能力本就属于运行时/用户而不属于模型。

这说的是**产品工具面**，不是实现正确性。实现本身机械上就是错的——语义不确定、第二条通往文件系统的路径、含义随环境改变——那是直接修的，见 §6.6。

各工具的处置，以及背后的证据。使用数据来自 `evals/baselines/tool-surface-t0-e623f53/`：99 个会话共 1725 次工具调用，一个模型、一份 profile——足以否定一个说法，不足以确立一个说法。

| 工具 | 处置 | 理由 |
| --- | --- | --- |
| `expand_tools` | **已删除** | 这不是工具面判断，而是它根本不可能工作：没有任何消费者读它的 `expand_categories` metadata，它声称要扩张的 registry 是不可变的 `Arc`，而 definitions 在每次 drive 只快照一次——所以「host 会在后续 round 注册匹配工具」这句话没有任何东西兑现。它还宣传 `mcp` 和 `subagent`（两者什么都不注册），并拒绝 `browser`（唯一真有实现的类别）。整个证据集里只有一次调用，还失败了。 |
| `replace` | **已删除** | 1725 次调用里零次，包括在六次 `apply_patch` 上下文匹配失败中——那正是它被造出来吸收的场景，而模型每一次都改成重试 `apply_patch`。它的弱模型理由已撤销（§1.1），`edit` + `write` 覆盖了这个意图。 |
| `read_symbol` | **OPTIONAL + EVAL_LOCKED** | 零调用，但 `find_symbol` 和 `find_references` 同样是零，这更像是 language server 从没起来，而不是这个工具冗余。它随 Code Intelligence pack 离开默认面；A/B-READ-SYMBOL 仍然欠着。 |
| `blast_radius` | **OPTIONAL + EVAL_LOCKED** | 高级派生操作，不是原语。证据问题与 `read_symbol` 相同，处置相同。 |
| `create_checkpoint` / `restore_checkpoint` | **RUNTIME OR USER ONLY** | 归属问题，不是使用量问题：运行时本来就在每次写入前 checkpoint，并拥有回滚与崩溃恢复。让模型判断何时该 checkpoint 是在重复一个运行时设施。零调用与此一致。 |
| `consolidate_memory` | **RUNTIME OR USER ONLY** | 子系统维护。可以后台跑、会话结束跑，或由用户命令触发。 |
| `create_skill` | **RUNTIME OR USER ONLY** | 系统定制。`load_skill` 留下；创建技能是用户的动作。 |
| `forget` | **OPTIONAL（Memory pack）** | 破坏性，但是 consent-gated：它会弹审批，而修正一条已经被仓库淘汰的记忆本来就是工作的一部分。按操作分类，不按 crate 分类（§25）。 |
| `shell_command` vs `run_command` | **都留** | 两个不同的模型意图，不是同一件事的两种写法。`run_command` 是 program+args：跨平台、无 shell 引号问题、审批时易做安全分析，而且只有它能起后台。`shell_command` 是一整行 shell——管道、`&&` 链本来就是这个形状。剩下的不对称——`shell_command` 不能起后台——是记录，不是这次修。 |
| `git_status` / `git_diff` | **OPTIONAL（VCS pack）** | 用 `run_command` 也能做，但 git 检视高频、不需要 shell、可 replay。接口保留；实现迁到 `leveler-vcs`。 |

`list_files` 和 `find_files` **不是**合并候选。它们表达不同的模型意图——「这个目录里有什么」对「仓库里哪里有符合这个模式的文件」。真正该统一的是它们底下的文件系统遍历和 ignore 语义。

### 6.6 两个不同的问题，两种不同的举证责任

原来那条规则——动任何工具之前先测量——太宽了。它把「读代码就能定论」的重构也拖进了「先做使用基线」的仪式。

```text
架构正确性由机械证据确立。
产品工具面价值在价值确实不确定时由 Eval 确立。
```

**直接修，不需要 A/B。** 靠检查就能证明的缺陷就是缺陷：归属错误、语义不确定或随环境改变、运行时实现重复、跨平台行为分叉、service-locator 耦合、运行时能力长在 Tool 适配器里。修这些改变的是代码**是什么**，不是产品**提供什么**。

**先测量。** 当两个都合理的原语发生重叠、当某个可选能力的价值不清楚、当删掉一个工具可能实质改变产品行为时，先从真实会话取一次使用基线：哪些工具真的被调用、成功率与重试率如何、它们的存在是否提升任务成功。原来那份优先名单里，`replace`、`expand_tools` 和 checkpoint 工具最后是被归属或机械缺陷定的，不是被测量定的（§6.5）；`read_symbol` 和 `blast_radius` 仍然欠着自己的那一份。

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
| `leveler-browser` | 3 | 浏览器进程直接 spawn（Unix 进程组、Windows Job Object）；profile 目录建在 Leveler home 下。 |
| `leveler-memory` | 11 | 在 Leveler home 下写 memory 存储。 |
| `leveler-lsp` | 4 | 直接 spawn language server。 |
| `leveler-engine` | 3 | 用 `git rev-parse` 打 baseline commit。 |
| `leveler-skills` / `leveler-project` | 2 / 1 | 在 Leveler home 下建状态目录。 |

这张表里其实是两类东西：

1. **模型请求的、作用在用户仓库上的副作用。** 全部走 `Workspace` 和 `CommandRunner`。`leveler-agent` 是 0，这个数字才是关键。
2. **Runtime 自己的状态与 sidecar。** 写在 Leveler home 下的内容，以及长驻 sidecar 进程（MCP server、language server、浏览器），不走权限/审批路径——因为它们不是模型要求的。

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
| `leveler-browser` | 浏览器产品选择、ref 与标签页归属，以及两个协议适配器（Chrome/Edge/Chromium 走 CDP，Safari 走 WebDriver）。 |
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

### 12.1 Harness 告诉模型什么，不告诉什么

System prompt 是一份**契约**，不是操作手册。它只承载四类东西，每一类的检验标准都是「模型有没有别的办法知道这件事」：

```text
身份与权威边界      谁压过谁、编辑工具提供了 shell 提供不了的什么保证、
                    审批被拒意味着什么
运行时状态          cwd、权限模式、网络、项目规则、workspace 清单
Harness 协议        决策怎么送到用户、goal 怎么结束、委派与 ownership 怎么运作、
                    memory 的同意机制怎么走
产品约束            用户语言、消息长度、不要重复叙述、不要过程性收尾
```

它**不承载方法**。W1 移除的有：何时该做计划、计划怎么同步；宣告完成前必须先跑验证；进度叙述模板（`current step k/n · evidence → next`）；什么形状的活该优先用哪个工具；理解类问题该怎么调查；失败该怎么诊断；何时该坚持、何时该停止重试。每一条都是 Harness 在替模型思考。

回合中途注入的内容也守同一条线。**协议修复保留**——goal 未解决、tool 调用畸形、响应被截断、子 Agent 已结算、发现了规则文件、预算位置——因为每一条都在陈述一个机械事实并点名解决它的操作。**读模型推理的提示已删除**：计划提醒、计划过期提醒，以及那个 payload 是「换个做法」的重复调用 loop guard。

约束跑飞循环的东西没有变，而且是无条件的：round 上限、token/成本/时长预算、wall clock、取消，以及「连续多个 round 里每一次调用都被拒绝」时的 no-progress 停止。

**计划是能力，不是强制。** `update_plan` 每一轮都可用，状态会持久化，UI 会渲染。没有任何东西对任务分类，没有任何东西统计「多少轮没有计划」，也没有任何东西开口要一份计划。

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

现在过了、以前没过的：

- `leveler-model` 已经不知道任何 Coding 工具名（§18.6），Review 的工具集除了协议什么都不继承。
- 组装一个工具面不再需要 Coding 的任何工具管路：想要自己工具的 harness 就注册自己的，而 `register_harness_controls` 展示了形状——harness 在宿主组合出的能力之上，加上「操纵自己」的那部分。

还没过的：

- Review harness 若想要持久化、resume、事件顺序和恢复，就得走 `leveler-engine`，而它依赖 `leveler-agent`，公开 API 里还有 `CodingTaskSpec`（§18.1）。这就是剩下的全部，也就是 W3。

它不强制 kernel 变更，所以结论是「尚未强制成立」，不是「失败」。

**没有写第二个 Harness。** 这里没有任何一条是靠真的建一个来证明的；为了验证设计而造一个 demo harness 会是假消费者（§5.3）。这里主张的只是「Foundation 不再要求 Coding 的工具管路」，不是「第二个 harness 已经存在」。

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

### 18.2 ToolContext 曾是通用 service locator（已关闭）

**曾经。** 每个工具都收到 `ExecutionResources + ToolPolicy + ToolServices + session_scope`，`ToolServices` 把 `lsp_sessions`、`lsp_start_locks`、`artifact_store`、`memory_root`、`background_tasks`、`browser` 写成字段。

**现在。** `ToolServices` 已删除。每个工具在构造时拿到它使用的句柄；`ToolContext` 只携带执行底座、这次调用的授权和会话身份。形状与逐工具对照表见 §5.5，绊线见 `crates/leveler-tools/tests/ownership_boundaries.rs`。

**仍然开放。** `ExecutionResources` 还是一个共享 facet——workspace、runner、environment、checkpoint、读取指纹、命令闸门。它们是「一次调用所锚定的底座」而不是「工具去发现的服务」，而且写入范围要以 workspace root 解析，所以原地保留。runner 和命令闸门是否应该只到命令工具手里，是一个真实但更小的问题；无论怎么答，它都不是 service locator。

### 18.3 具体工具实现债务

以下每一条，都是工具在自己实现本应调用的能力行为。A–C 和 H 由核心原语基座那轮关闭；D、E、F 这一轮关闭；G 是**主动决定**不做，不是漏了。每条都保留，写明实际做了什么以及各自还剩什么。

#### A. `read_file` 仍然承担 stale-write 观测（大部分已关闭）

`crates/leveler-tools/src/tools/read_file.rs` 现在是模型面的适配器：schema、渲染、分页文案。读本身归 `leveler-tools::workspace::WorkspaceReader`，它返回有界内容加一个 `ReadObservation`。

已关闭：

- **体积不再让读失败。** 10 MB 上限已删除，工具也不再把模型推去用 `sed`/`head`/`tail`——对 100 MB 文件做一次有界窗口读就是普通调用，分页规则写在工具自己的契约里。
- **非法 UTF-8 如实上报，不再改写。** 全文件逐行解码；第一处非法行是显式错误并给出行号，含 NUL 字节的文件报「不是文本文件」。没有任何 `U+FFFD` 冒充文件内容到达模型。
- **重复读策略已删除。** `RepeatedReadGuard` 及其配置管线一并删除。模型重复读同一段是对它推理的判断，harness 不做这种判断（§1.1）。
- **结果预算是显式的。** 调用方把「我能渲染多少字节，以及每行装饰要多少字节」告诉 reader，所以渲染后的结果真的装得下预算。

仍然开着：

- **窄窗口读仍然流式扫全文件。** 保护后续编辑的指纹覆盖全文件，目前没有同等强度而更便宜的做法。内存是 O(最长行 + 窗口)，时间是 O(文件)。正确性优先于这个优化，在替代方案自证之前保持现状。

**剩余目标 ownership。** stale-write 保护已经是 `ReadObservation` 与 `WorkspaceEditor` 之间的显式机制，不再是藏起来的副作用；但指纹仍由读工具记进共享的 `FileStateTracker`，而不是随观测一起传递。

#### B. Workspace 搜索已经确定（四个读原语已关闭）

`list_files`、`find_files`、`grep` 都是 `leveler-tools::workspace::WorkspaceSearch` 的适配器：一套遍历、一套 ignore 规则、一种 glob 方言、一种正则方言，全部在进程内。

- **不起子进程。** `git ls-files` 和 `rg` 都没了。四个读原语现在都声明 `replay_is_side_effect_free`，这是「不依赖任何外部二进制」的机械证明。
- **一次调用一个含义。** `grep` 的 pattern 就是正则，`literal` 和 `ignore_case` 是显式参数；不会再因为没装 `rg` 就退化成子串扫描。`find_files` 的 pattern 就是 glob，锚定规则用 gitignore/ripgrep 那一套（不含 `/` 匹配文件名，含 `/` 匹配相对路径）；`auto`/`substring`/`glob` 模式开关已删除。
- **候选集合不再取决于 Git。** `.gitignore` 和 `.ignore` 由 `ignore` crate 解析，与「是不是仓库」「装没装 Git」无关；用户的全局 gitignore 刻意不读，否则同一个仓库在两台机器上会搜出不同结果。
- **`list_files` 就是目录检视。** 它只列一个目录的直接子项，并且什么都不隐藏——`max_depth` 没了，构建产物过滤也没了：一层列表本来就不会下钻进 `target/`，藏起来只是让模型少知道一个事实。

仍然开着：`locate_hint` 和符号回退扫描各自带着遍历。它们不是读原语，这次没动。

#### C. Workspace 编辑只有一个 Owner（已关闭）

`leveler-tools::workspace::WorkspaceEditor` 是工具通往磁盘文件的唯一受保护路径：跨进程 advisory 锁横跨比较与 rename、CAS、不可猜的暂存名、capability/描述符相对写、checkpoint 捕获，以及在锁内重新校验写入范围。`apply_patch` 和 `write_file` 调它，`replace` 在从工具面移除前也调它（§6.5）。

代码本身没变——它本来就是共享提交路径，只是住在 `replace` 工具里，于是共享 runtime 被以它的一个调用方命名。这次只搬了 Owner。

#### D. 命令执行只有一个 Owner（已关闭）

`run_command` 曾经承担沙箱、环境、网络策略、后台进程、快照、mutation 记账、写入范围、回滚、命令闸门、进程生命周期——而 `shell_command` 是 import `run_command::execute_program` 来跑自己的运行时的，于是一个工具看起来像另一个工具的 owner。

那个运行时现在是 `crates/leveler-tools/src/tools/command_execution.rs`。两个工具都被注入它，谁也不拥有它。各自只留自己的模型接口：`run_command` 解码 argv 并说出那两句「你要的其实是另一个工具」的拒绝，`shell_command` 把 shell 行映射到平台 shell 并跑 hang 守卫。

**仍然开放。** 这个模块住在 `leveler-tools` 而不是 `leveler-execution`：它要从 `ToolContext` 读这次调用的授权、并返回 `ToolOutput`——工具层之下的那一层不能依赖这两个类型。要再往下搬，授权和结果形状得跟着搬，那是比这一条大得多的问题。

**`shell_command` 没有 `background=true`，这是主动决定。** 机械上它可以有：shell 行就是 `program + args`，而 `proven_executed_commands` 早就能归因一段 shell 脚本。坏掉的是生命周期保证。`run_command(background=true)` 注册的是**那个长期进程本身**，所以 `kill_task` 和会话回收真的杀得掉。而一个 detach 的 shell 可以在 spawn 完自己的子进程后立刻退出——`shell_command(cmd="python app.py &")`——注册表手上就剩一个报告 `Exited` 的任务，真进程还在跑，且无法回收。这个不对称不是外观问题；现有守卫已经把模型指向生命周期诚实的那个工具。

#### E. Code Intelligence 只有一个 Owner（已关闭）

`find_symbol`、`read_symbol`、`find_references`、`diagnostics`、`blast_radius` 各自包含 LSP 发现、会话生命周期和启动——而 `find_symbol` 还带着一份 `symbols.rs` 里已经有的 locate 逻辑的副本。其中三个还各带一份**不同的**源码遍历：一个按 `repo_map::is_source`，两个按一份更短的硬编码扩展名列表，于是同一个仓库有两种「什么算源码」。

现在 `leveler_lsp::LspSessions` 拥有会话池、启动、死服务驱逐和 `locate`，五个工具全部在构造时被注入它。源码遍历是 `tools/symbols.rs` 里的一个函数。

**无依赖回退保留，并且保持标注。** 没装语言服务器时，`find_symbol` 和 `read_symbol` 用扫描回答，结果写 `(via scan)`，而精确答案写 `(via rust-analyzer)`。它们回答的是一个**更弱**的问题——哪些文件定义了这个名字，而不是定义在哪一行——把这一点说出来正是重点。错的是「把回退伪装成服务器的答案」；`LspSessions::locate` 返回 `None` 的意思只是「没有语言服务器的答案」，不多也不少。

#### F. `view_image` 重复实现且弱化了 `leveler-media`（已关闭）

`view_image` 过去根据文件**扩展名**判断 MIME，读字节，base64 编码：没有内容嗅探、没有像素上限、没有剥 EXIF。于是一个叫 `.png` 的 JPEG 会被当成 `image/png` 报给 provider，一枚解压炸弹只被 5 MB 的字节上限挡着，一张照片的 GPS 标签就跟着发出去了。而 `leveler-media`——当时唯一的消费者是用户附件路径——三件事全做。

现在这条流水线是一个函数 `leveler_media::process_image`：内容判定真实类型、解码器分配之前先设字节与像素上限、按最长边降采样、重编码为 PNG。`MediaStore::import_bytes` 调它然后哈希入库；`view_image` 调它然后 base64。为此 `leveler-tools` 新增了对 `leveler-media` 的依赖，方向是对的：工具层调用能力。

#### G. `web_search` 拥有 provider 配置（已关闭）

工具过去自己从环境快照读 `LEVELER_SEARCH_API_KEY`、`LEVELER_SEARCH_PROVIDER`、`LEVELER_SEARCH_CX`，自己实现了 Bing 和 Google Custom Search 两套请求与响应形状，还在调用时自己判断「我到底配没配」——而那是 composition root 的答案，不是工具的。

**靠删除关闭，不是靠抽象关闭。** 两套 provider 形状删了，只剩一个后端（Tavily），只写一处，前面不加 `SearchProvider`——一个只有一个实现、一个使用者的 trait 正是 §5.3 要拦的 wrapper。配置现在只有一个 owner：`leveler-app` 读一次 key，空字符串等同未配置，同一个答案同时喂给 `capability_availability` 和 `WebSearchTool::new`。没有 key 的宿主根本不注册 `web_search`，所以工具里那条「未配置」分支已经没有了——那个状态到不了它。

留下来的那条不是重复。`execute` 里的 `network_denied` 检查，是进程内直接拨网络的工具唯一的执行点：ToolHost 会把 `network_allowed` 冻进 resolved policy，但真正执行它的 OS 沙箱管的是 `run_command` 子进程，管不到本进程里的 `reqwest`。`web_fetch` 和 `web_search` 出于同样理由带着同样的检查。浏览器**不带**：它是一个被明确授权联网的能力，暴露它本身就是授权（§5.3）。

等真的必须支持第二个后端时，再抽 provider 缝。

#### H. 后台结算归运行时（已关闭）

`WaitTaskTool` 过去在 wait 结束时跑 `account_background_mutations`，可能把整个 workspace 恢复到快照。于是结算取决于模型是否愿意等：回合先结束了、或模型根本没调 wait，一次越权写入就留在磁盘上。

现在 `BackgroundTaskRegistry` 在任务进程退出时结算：reaper 取走 `MutationBaseline`，diff workspace，并且**仅在存在显式写入白名单时**恢复该任务无权改动的部分，然后在发布终态之前存下 `BackgroundSettlement`。白名单随 baseline 一起传，因为后台任务运行时依据的授权就是它 spawn 时的那份；它比这一回合活得久，运行时结算时没有「后来的 scope」可查。`wait_task` 只读取这份结算一次并渲染。

dev-server 安全不变：恢复仍然只在显式白名单下发生，所以默认后台任务只被记账、永不回滚（K17）。

`wait_task` 仍然不是 `Safe`。它不再执行结算，但它会消费那份一次性报告，而崩溃重放会阻塞恢复最多两分钟并把报告吞掉。

### 18.4 Sidecar 进程绕开 CommandRunner

**当前。** MCP server（`leveler-tools/src/mcp.rs`）、浏览器（`leveler-browser/src/cdp.rs`、`webdriver.rs`）和 language server（`leveler-lsp/src/client.rs`、`registry.rs`）都是用 `Command::new` 直接拉起，不经 `leveler_execution::CommandRunner`。

**期望。** 要么让它们进入宿主权威的进程树终止与沙箱语义，要么把这个豁免写成一条显式命名的策略——「runtime sidecar」——并给它确定的生命周期 Owner。

**为什么部分违宪。** 违反规则 4。它们不是模型请求的命令，所以权限与审批不适用；但它们仍是宿主进程，而宿主权威本应拥有每一个宿主进程。

**风险。** 低到中。Sidecar 存活期已经和 daemon 关停时的 reaping 缠在一起。

### 18.5 `update_plan` 曾和能力适配器待在一起（已关闭）

`update_plan` 是一个 Harness 控制：它不携带任何能力，不碰任何能力句柄，存在的唯一理由是 Coding harness 有一套计划协议。另外七个控制住在 `leveler-agent`，而它当时和能力适配器一起注册在 `leveler-tools`。

**W1 为什么没搬。** 注入路径完全绕开 registry，所以当时搬它意味着为一个工具手写重实现四项 registry 服务：`normalize_input`（它修复模型会发出的嵌套信封形状）、JSON-Schema 校验、`schemars` 生成的 schema、输出封顶。

**搬了什么。** 它现在是 `crates/leveler-agent/src/update_plan.rs`，由 `register_harness_controls` 在能力 pack 之后放到工具面上。它仍然是一个注册的 `Tool`，没有变成第八个注入定义——因为 registry 现在是机械接缝而不是策略引擎，复用它一分钱不花，而重实现要花四份重复服务。校验、生成的 schema、dispatch、结果封顶都是 registry 的；`metadata.plan` → `PlanUpdated` 那条路没有变。

`core_surface` 不再注册任何控制，harness 也不注册任何能力。`leveler-tools` 组合「宿主能做什么」，`leveler-agent` 组合「什么在操纵 harness」。

### 18.6 `leveler-model` 曾知道 Coding 工具名（靠删除关闭）

**曾经。** `crates/leveler-model/src/tool_catalog.rs` 硬编码了 `grep`、`find_files`、`find_symbol`、`read_symbol`、`find_references`、`list_files`、`read_file`、`git_status`、`git_diff`、`view_image`、`web_search`、`web_fetch`、`apply_patch`、`replace`、`run_command`、`shell_command`，并由此推导执行分类、replay 安全性、主参数和 observe key。它里面还留着 `replace`——那个工具早就被删了——正是「一份名单的第二副本」必然的失效方式。

**怎么关的。** 靠删，不是靠搬。审计发现这份 catalog 几乎没有活着的消费者：

| 导出 | 消费者 | 处置 |
| --- | --- | --- |
| `is_safe_replay_tool` | `leveler_client_protocol::recovery_for_tool` | `recovery_for_tool` 和它的 `Recovery` enum 本身就是死代码——只被 re-export，从未被调用。两个一起删。活着的答案是 `Tool::replay_is_side_effect_free`，registry 去问它，未知名字答 `false`。 |
| `builtin_tool_metadata(…).primary_argument` | `leveler-tui` 的 `find_files` 标签 | 内联成 `s("pattern")`，就放在同一个 match 里另外四十个工具标签旁边。展示元数据属于客户端（§5.2）。 |
| `is_search_tool`、`builtin_observe_key`、`BuiltinToolClass` | 无 | 删除。 |

所以任何地方都不存在第二份名单——也不存在一份「搬过去的」。`crates/leveler-tools/tests/ownership_boundaries.rs` 会在 Coding 工具名重新出现在 `leveler-model` 的生产代码里时失败。

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

### 18.10 Browser 实现已被取代（已关闭）

**曾经。** `leveler-browser` 通过 Playwright 驱动 Chromium，中间隔着一个 CodeLeveler 自己写的 Node 桥，在 stdio 上说自定义 JSON-RPC，SSRF 边界就在那个桥里强制。

**现在。** Browser Capability Closure 已经把它换掉。Chrome、Edge、Chromium 直接走 CDP；Safari 走 W3C WebDriver 经 `safaridriver`。没有 Node、没有 Playwright、没有 npm 安装、没有托管浏览器下载、没有自定义 RPC。跑哪个浏览器由用户的系统默认浏览器决定，除非调用或 `[browser].default` 另有指定；选中的浏览器驱动不了就报错，不换成另一个。

**跟着一起关闭的。** 之前挂起的 `loopback_ws_from_a_granted_dev_page_connects` 抖动，是关于桥里那个 JavaScript WebSocket gate 的发现。那个 gate 连同它所属的整套 page-scoped loopback grant 都已删除：浏览器是被授权联网的，已经不存在需要逐请求做的 loopback 判定，也就没有可竞态的东西。这条发现是被删除关闭的，不是被修复关闭的。

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

### 19.2 已关闭：canonical 编辑工具是 `apply_patch` 加 `write_file`

`replace` 已删除。它与两者都重叠，弱模型理由已撤销（§1.1），而且在它本该吸收的 patch 失败里一次也没被用到（§6.5）。

### 19.3 已关闭：patch 上下文匹配是精确匹配

`seek_sequence` 原来用五轮逐级放松来定位 hunk。这个问题一度被当成「松到什么程度」的分寸问题；读完 apply 路径之后，它是正确性问题。

定位到的 hunk 是用 `file.splice(start..end, replacement)` 应用的，而未改动的（` `）上下文行**也在这个 splice 里**。所以一次松匹配会把文件真实字节改写成模型的写法——尾部空白消失、typographic 引号变成 ASCII、空格被重排——全都发生在一个报告成功、且对此只字不提的调用里。它还让两个编辑工具直接互相矛盾：`replace` 拒绝的 typographic 近似匹配，`apply_patch` 会静默折叠掉。

现在比较是逐字节精确的。留下的容忍都属于**表示层**而不是语义层：BOM 剥离与恢复、CRLF 折叠与恢复、trailing-blank 重试、结尾换行规则——每一条要么保持字节不变，要么明确写明了它会改变什么。不精确的 patch 会失败，文件原样不动，错误里给出锚点处文件真正的内容。

有一个后果是记录而不是修复：在 LF 占多数的混合换行文件里，游离的 `\r` 会留在行内，这样的行不再匹配没有 `\r` 的 patch。要做到精确，需要写入时逐行保留原始换行符。

### 19.4 已关闭：模型控制的动态工具扩展不划算

`expand_tools` 用一个额外 round、动态 registry 状态和反转的工具面 ownership 去买 schema token。它已删除，而且决定性的事实不是这笔交易：没有任何东西消费它的输出，所以它从来没有扩张过任何东西（§6.5）。

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

TOOL_IMPLEMENTATION_ALIGNED       YES
ENGINE_IMPLEMENTATION_ALIGNED     NO
CORE_PRIMITIVE_FOUNDATION_ALIGNED YES
TOOL_SURFACE_CLOSED               YES
CAPABILITY_MODEL_CLOSED           YES
AVAILABLE_ENABLED_EXPOSED         SEPARATED
PLAN_ENFORCEMENT                  REMOVED
EDIT_MATCHING                     EXACT
TOOLREGISTRY_CLOSED               YES
TOOLCONTEXT_CLOSED                YES
FOUNDATION_TOOL_NAME_LEAKAGE      NONE
WORK_PROFILE_AUTHORITY            SESSION_ROW

BROWSER_IMPLEMENTATION            SUPERSEDED_PENDING_REPLACEMENT

SECOND_HARNESS_TEST               NOT_YET_ENFORCED
SECOND_HARNESS_WRITTEN            NO
FOUNDATION_FROZEN                 NO
```

架构、工具边界和模型可见的工具面都已经定了；七个核心原语、能力 ownership 和组合方式也按它实现了。剩下的是 Engine：它仍然点名 `leveler_agent` 和 `CodingTaskSpec`（§18.1），而这也是 `SECOND_HARNESS_TEST` 还没强制成立的唯一原因。

有四行从 NO 变成 YES，是因为代码变了而不是措辞变了：`ToolServices` 已删除、registry 不再做任何授权判断、`leveler-model` 不含任何工具名、一个回合的 work profile 来自 session 行。每一条都在自己那节里点了绊线的名字。没有为了让这张表里任何一行好看而修改源码。

`BROWSER_IMPLEMENTATION` 已关闭。Node 桥、Playwright、自定义 RPC 和那些 JavaScript 网络 gate 都没了；`leveler-browser` 直接说 CDP 和 WebDriver，§18.10 记录了这次关闭了什么。
