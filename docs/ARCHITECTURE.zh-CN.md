# CodeLeveler 架构

架构的唯一权威文档。`AGENTS.md` 只给出宪法的短版本并指向这里；边界定义、以及当前实现与目标之间的差距，都写在本文。

英文版：[`ARCHITECTURE.md`](ARCHITECTURE.md)，为 canonical 技术文档，本文与之语义一致。

以下内容全部基于 commit `494ed1b`（31 个 crate）的真实源码与 `cargo metadata` 验证。凡是代码还没到位的地方，直接写明，不把目标当成已完成的现状描述。

---

## 1. 架构原则

过去容易把 CodeLeveler 读成「一个 coding agent，其他都在它下面」。这种读法让 coding 产品成为最高抽象，于是每加一个能力都被往下压进公共 crate 去伺候它。

正确的结构是：

```text
Foundation 提供可复用的 agent runtime 能力。
Harness 定义领域语义。
Product 定义体验、组合与交付。
```

六句话就是全部模型：

```text
Kernel 不懂产品。
Harness 定义领域语义。
Engine 管生命周期，不当 Agent Brain。
Host Authority 独占受控真实副作用。
每一个持久事实只有一个权威 Owner。
机械事实不等于语义满足，更不等于用户验收。
```

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
└──────────┬──────────────────────────┬───────────────────────┘
           │                          │
           └────────────┬─────────────┘
                        ▼
┌─────────────────────────────────────────────────────────────┐
│                      Agent Runtime                          │
│   leveler-agent-core              Tool 运行时契约            │
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

- **Engine 在 Harness 之上，不在它旁边。** `leveler-engine` 依赖 `leveler-agent`，公开 API 里直接出现 Coding 概念。见 §17.1。
- **Tool 契约和具体能力在同一个 crate。** 图里「Tool 运行时契约」这个盒子，和「可复用能力」那一排的大部分，今天都是通过 `leveler-tools` 拿到的。见 §17.2。

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

这几个 crate 不得知道 coding、review、finding、仓库工作流、TUI、Web、CLI 或任何产品概念。

`leveler-lifecycle` 内部已经做好了切分：`runtime` 模块是领域中立的，`workflow` 模块放 Coding 词汇，且禁止 `runtime` 引用 `workflow`。未来的非 Coding 领域只依赖 `runtime`，不会连带拽进 Coding 语义。

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

Kernel 与宿主之间的全部契约就是 `AgentHarness` trait：循环在每一轮的固定位置调用每个接缝，harness 返回一个 `Flow`（继续、进入下一轮、或用 harness 自己的 outcome 结束）。除了「声明有哪些工具」和「执行工具」这两个接缝，其余都有中立默认实现——所以一个普通的 tool-calling agent 就是 `ToolRuntime` 外面套一层 `BasicHarness`，再无其他。

Kernel 从不判断模型的工作是好、是完成、还是可接受。一次运行的结束点只有三种：模型自己停、宿主让它停、机械上限让它停。

用产品词汇（`coding`、`repository`、`permission`、`prompt`、`review`、`finding`、`verify`、`patch`、`filesystem`）扫描整个 crate，命中全部落在文档注释里，而且每一条都是在说「这件事归别人管」。**Kernel 今天是干净的。**

---

## 5. Tool 运行时

有两件事值得在读者脑子里分开，因为代码目前还没分开。

**Tool 运行时契约**——工具「是什么」：

```text
Tool trait          ToolSchema          ToolRegistry
ToolCall            ToolResult          ToolContext 契约
ToolHost 契约        准入                dispatch 契约
```

**具体 agent 能力**——「有哪些」工具：`read_file`、`list_files`、`grep`、`apply_patch`、`replace`、`run_command`、`shell_command`、`find_symbol`、`find_references`、`diagnostics`、`blast_radius`、`git_status`、`git_diff`、浏览器、memory、skills、web 抓取与搜索、看图、任务控制，以及 MCP 发现的工具。

### 当前状态

`leveler-tools` 两件事都装。`src/tool.rs` 和 `src/registry.rs` 是契约；`src/tools/` 是 29 个具体能力。这个 crate 依赖 `leveler-browser`、`leveler-context`、`leveler-execution`、`leveler-lsp`、`leveler-memory`、`leveler-project`、`leveler-skills`——这些边属于具体工具，不属于契约。

耦合具体表现在 `ToolContext` 上：`ToolServices` 把 `lsp_sessions`、`artifact_store`、`memory_root`、`background_tasks`、`browser` 写成了结构体字段。任何实现 `Tool` trait 的人——包括未来一个完全用不上这些的 Review 专属工具——都得接下整个形状。这些字段是 `Option`，调用方**可以**传 `None`；但类型上，每个工具仍然看得见全部能力。

`ToolRegistry` 本身是可自由组合的：`ToolRegistry::new()` 加 `register`，`core_registry()` 和 `full_registry()` 是两个预制选择。今天另一个 harness 就能构造出不同的 registry。

### 目标状态

```text
leveler-tool-core   → Tool、ToolSchema、ToolRegistry、ToolCall、ToolResult、
                      ToolContext 契约、ToolHost 契约、准入、dispatch 契约
leveler-tools       → 具体内置能力
```

**这只是写下来的目标，不是排期中的工作。** 不要为了让本文看起来完整就去建 `leveler-tool-core`。这个拆分要等到真的有第二个 harness 需要「只要契约、不要能力」时才成立——见 §16。

另外注意：kernel 侧已经有一个更窄的接缝。`leveler-agent-core` 里的 `ToolRuntime` 只需要两样东西：模型能看见的工具定义，以及把一次 `ToolCall` 变成模型下一步要读的文本的方法。`leveler-tools` 坐在那个接缝后面，不在 kernel 里面。

---

## 6. Host 执行权威

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

不允许存在第二条从 harness 或 tool 通往文件系统、通往进程的路径。也不要让 tool 层和 execution 层各自持有一套安全策略。

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

这张表里其实是两类东西，混为一谈就会把边界说错：

1. **模型请求的、作用在用户仓库上的副作用。** 全部走 `Workspace` 和 `CommandRunner`。`leveler-agent` 是 0，这个数字才是关键。
2. **Runtime 自己的状态与 sidecar。** 写在 Leveler home 下的内容（memory、skills、project 状态、浏览器 driver），以及长驻 sidecar 进程（MCP server、language server、浏览器 driver），不走权限/审批路径——因为它们不是模型要求的。

第二类是真实且有意为之的边界。但这些 sidecar 确实在 `CommandRunner` 的进程树终止与沙箱语义之外。见 §17.4。

---

## 7. 可复用能力

以下是 harness 可以按需挑选的能力，不是 kernel 的一部分：

| Crate | 能力 |
| --- | --- |
| `leveler-context` | 有界的仓库上下文装配：map、候选文件、相关测试、合并后的项目规则、token 估算、重复读防护。 |
| `leveler-project` | 项目语言识别与文件布局（配置与状态位置）。 |
| `leveler-memory` | 持久项目记忆存储与其晋升流水线。 |
| `leveler-skills` | Skill 发现与加载。 |
| `leveler-vcs` | Git 操作，经执行权威落地。 |
| `leveler-lsp` | language server 会话，跨工具调用复用。 |
| `leveler-browser` | 浏览器运行时、driver 安装、按项目隔离的 profile。 |
| `leveler-media` | 多媒体处理。无内部依赖。 |

Coding Harness 选的是 context、project、VCS、LSP、browser、memory、文件写入与进程执行。Review Harness 大概率只需要 context、project、VCS、LSP、只读文件系统和 memory，其余不要。不要为了替 harness 省掉「挑选」这一步，就把全套能力绑死在 kernel 上。

---

## 8. 持久化 Runtime

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

源码里自己把这称为「通往领域中立 engine 的迁移接缝」。这个拆分让每条代码路径必须声明自己读的是哪一半，但还没有消除对 Coding 的依赖——见 §17.1。

---

## 9. 存储与持久事实

`leveler-storage` 是持久事实的边界：SQLite、内嵌 migration、连接池，每个关注点一个 repository。业务逻辑从不直接发 SQL。

```text
一个持久事实
    → 只有一个权威 Owner
    → 只有一份 canonical 持久表示
```

任务状态、turn 状态、证据、ownership、用量、artifact、完成状态，无一例外。不允许出现平行真相源。

`leveler-storage` 只依赖 `leveler-core` 和 `leveler-lifecycle`。这正是低层持久化 crate 能讲生命周期词汇、又不需要反向依赖高层 crate 的原因——也是这套词汇要单独成 crate 的理由。

---

## 10. 验证与证据

`leveler-verifier` 跑项目声明的检查（格式、构建、测试），采集证据，检查范围，对失败分类。

它的权威范围必须这样表述：

```text
Verifier 是「验证结论」的权威。
Verifier 不是「语义任务完成」的权威。
```

它能证明配置的检查通过、失败还是被阻塞。它无法单独证明用户的意图已被满足。

用户显式声明的验证命令是权威，不是启发式输入——discovery 层会这样标记它，harness 不得用自己的猜测替换它。

这就是为什么 `TaskOutcome` 和 `VerificationStatus` 在 `leveler-lifecycle` 里是两条正交的轴。`TaskOutcome::Completed` 表示模型宣告目标完成；`VerificationStatus` 表示项目自己的检查对最终代码树说了什么。Runtime 两个都报，绝不合成一个词。

`leveler-verifier/src/lib.rs` 的 crate 级注释目前还是相反的说法。见 §17.3。

---

## 11. Harness 层

`leveler-agent` 就是 **Coding Harness**。crate 名字没有改，本文也不主张改名；重要的是概念。

它拥有 Coding 领域语义：

```text
coding prompt                    压缩策略
仓库上下文策略                    goal 语义
coding 工具选择                   coding 验证策略
写入工作流                        委派策略与子 agent profile
跨 agent 的路径 ownership          coding 完成契约
```

它通过唯一一个接缝接到 kernel：`src/executor/drive.rs` 里的 `Drive` 实现了 `leveler_agent_core::AgentHarness`。它填的接缝是 `tool_definitions`、`on_round_start`、`on_round_admitted`、`on_response`、`on_model_error`、`on_quiet`、`execute_calls`、`on_stop`、`on_event`。这就是 kernel 契约的全部，而且已经被一个真实 harness 用起来了，不是一个假设中的扩展点。

未来的 Review harness 是**兄弟**：

```text
              leveler-agent-core
                /            \
               ▼              ▼
        leveler-agent    leveler-review
        Coding Harness   Review Harness
```

`leveler-review → leveler-agent` 这条边是禁止的。Review 复用的是 Foundation，不是 Coding 产品。

Review 会拥有自己的词汇——`ReviewTarget`、`ReviewScope`、`ReviewPolicy`、`Finding`、`FindingSeverity`、`FindingEvidence`、`FindingLifecycle`、去重、抑制、`ReviewVerdict`、`ReviewReport`——这些类型一个都不许进 agent kernel。

以上不构成任何要做 Review harness 的承诺。它是对「Foundation 允许假设什么」的约束。

---

## 12. 产品层

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

这些层投影权威 runtime 状态，不推导新的 runtime 真相。一次工具调用返回 `Ok`，不构成客户端断定任务完成的依据；那个词只有一个 owner，客户端只负责读。

保证这一点的是 client protocol：UI 代码依赖 `leveler-client-protocol`，从不依赖具体 runtime、provider、tools 或 storage。`leveler-tui` 就是证明——它的依赖只有 client protocol、core、model、skills，再无其他。

---

## 13. 依赖方向

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

低层不得依赖任何面向用户的层。

当前依赖图，按拓扑层级排列（只统计 normal dependency，排除 dev-dependency）：

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

- **不存在反向依赖。** 没有任何 crate 依赖 `leveler-app`、`leveler-cli`、`leveler-tui` 或 `leveler-web`。方向规则成立。
- `leveler-agent-core` 只依赖一个内部 crate。Kernel 已经窄到宪法要求的程度。
- `leveler-agent → leveler-execution` 是**词汇**边，不是执行边：harness 用的是 `PermissionProfile`、`RiskLevel`、`WriteScope`、`HookRunner` 这些类型，它直接产生副作用的调用点是 0。
- `leveler-engine → leveler-agent` 是唯一一条与分层模型冲突的边。它就是 §17.1 的债务。

---

## 14. 一次 turn 的运行流程

一个 turn，从头到尾：

```text
客户端命令
    │
    ▼
leveler-app                  组合；把配置映射成一个运行中的 Application
    │
    ▼
leveler-engine               开 turns 行、给消息打 turn id、接上
    │                        persist-before-forward 的 EventLog、
    │                        把 approver 与 clarifier 包成 recorder
    │
    ├─ ExecutorFactory       从已解析策略与 turn profile 出发的
    │                        唯一一份执行配置推导
    ▼
leveler-agent（Drive）        Coding harness：prompt、上下文、工具选择、
    │                        委派、压缩、goal 语义
    ▼
leveler-agent-core           循环：准入一轮 → 组装 model round → 流式 →
    │                        解析 → dispatch 工具 → 下一轮，
    │                        全程在预算、deadline 与取消之下
    ▼
leveler-tools                具体工具执行
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

事实先落盘再外流。正是这个顺序，保证客户端不可能显示一个重启后 runtime 无法重现的状态。

---

## 15. 真相与权威模型

```text
机械事实  ≠  语义满足  ≠  用户验收
```

**Runtime 可以权威证明：**

```text
命令执行了              文件被修改了
退出码是多少            测试结果
构建结果                artifact 存在
事件被持久化了          工具返回了这个结果
观察到的 runtime 状态
```

**Verifier 可以权威判定：** 配置的验证结论。

**但这两者都不能自动推出下一步。**

```text
「工具成功了」    不能推出    「目标在语义上被满足了」
「测试通过了」    不能推出    「用户的请求被完成了」
```

语义判断由模型做。最终验收权在用户。

```text
Runtime 拥有机械事实。
Model 拥有语义解释。
User 拥有验收。
```

不要重新引入把机械观察当语义完成的捷径——比如一个 `observed_the_changed_tree()` 式的判定，把「树变了」读成「活干完了」。代码库里现在没有这种判定，而 `TaskOutcome` 与 `VerificationStatus` 保持正交，正是把它挡在外面的机制。

---

## 16. Second Harness Test

Foundation 的架构验收测试。

> 一个语义上不同的 agent 产品（比如 Review），能否**在不修改 agent kernel 的前提下**建在这套 Foundation 上？

目标答案：

| 问题 | 要求的答案 |
| --- | --- |
| 修改 `leveler-agent-core` | NO |
| 复用 model 与 runtime 词汇 | YES |
| 复用 tool 契约 | YES |
| 复用选定的能力 | YES |
| 加入 Review 语义 | YES |
| 加入 Review 专属工具 | ALLOWED |
| 依赖 Coding Harness | NO |

如果将来 Review harness 必须改 `leveler-agent-core` 才能跑起来，那就是 **foundation leak**。正确反应是分析原因，而不是往 kernel 里塞新的业务概念。

### 当前结论：NOT YET ENFORCED

Kernel 这一侧是过的。`leveler-agent-core` 只依赖 `leveler-model`，不带任何产品词汇，`AgentHarness` 接缝已经被真实 harness 实现过。Review harness 可以实现同一个 trait，不必碰它。

围绕它的 Foundation 还没过：

- Review harness 若想要持久化、resume、事件顺序和恢复，就得走 `leveler-engine`，而它依赖 `leveler-agent`，公开 API 里还有 `CodingTaskSpec`（§17.1）。
- Review harness 若想要 tool 契约，就得连具体能力集和 `ToolServices` 形状一起接下（§17.2）。

这两条都不强制 kernel 变更，所以结论是「尚未强制成立」，不是「失败」。它们是 Foundation Hardening 的输入。

**不要为了把这个结论改成 PASS，在一次文档变更里去动代码。**

---

## 17. 已知边界债务

记录下来，不掩盖。每一条都写清当前行为、期望边界、为什么违宪、最小修正、以及修正的风险。

### 17.1 Engine 依赖 Coding Harness

**当前。** `leveler-engine → leveler-agent`。Engine 的公开 API 导出 `CodingTaskSpec`，`ExecutorFactory` 直接构造 `leveler_agent::Executor`。`recorders.rs`、`recovery.rs`、`turn.rs`、`policy_resolver.rs` 都在点名 `leveler_agent` 的类型。

**期望。** Engine 通过一层抽象驱动 harness executor，不点名任何领域。`TaskSpec` 分成 runtime 一半和领域一半，engine 只读 runtime 那一半。

**为什么违宪。** 违反规则 5（Engine 管 runtime 机制，不管产品语义）和规则 2（Harness 是兄弟）：第二个 harness 会经由 engine 继承到 Coding harness。

**最小修正。** `RuntimeTaskSpec` / `CodingTaskSpec` 的拆分已经存在，源码里也写明它是迁移接缝。下一步是给 engine 一个不必点名 `leveler_agent` 的 executor 抽象，并把 `ExecutorFactory` 上移到 engine 之上。

**风险。** 中。`ExecutorFactory` 被刻意设计成执行配置的唯一推导入口；拆得不好会把它当初要消除的「多份推导」bug 放回来。

### 17.2 Tool 契约与具体能力同处一个 crate

**当前。** `leveler-tools` 同时装着 `Tool` trait、`ToolRegistry`、dispatch 和 29 个具体工具，并依赖 browser、context、execution、LSP、memory、project、skills。`ToolServices` 把 `lsp_sessions`、`artifact_store`、`memory_root`、`background_tasks`、`browser` 作为字段放进每个工具都会收到的 context。

**期望。** `leveler-tool-core` 放契约，`leveler-tools` 放能力。Harness 可以只取契约，不接整张能力图。

**为什么违宪。** 违反规则 3。一个完全不需要这些服务的 Review 专属工具，仍然得接下整个形状。

**最小修正。** 等第二个 harness 真的需要时再抽契约，不要提前。Registry 本身已经可自由组合，所以今天的实际代价在 `ToolContext` 的形状上，而不在工具集合上。

**风险。** 推迟做，低；投机地做，中——只面向一个消费者设计的契约 crate，通常要为第二个消费者重新设计一遍。

### 17.3 Verifier 的注释宣称拥有完成权

**当前。** `crates/leveler-verifier/src/lib.rs` 开头写着「只有 verifier 能标记任务完成」。

**期望。** Verifier 是验证结论的权威。任务 outcome 与验证状态正交，这正是 `leveler-lifecycle` 实现的模型。

**为什么违宪。** 违反规则 6。这是整个代码树里唯一一处仍在断言「检查绿了就是完成」的文本。

**最小修正。** 改写这段注释。无行为变更——代码早就把两条轴分开了，注释停留在拆分之前。

**风险。** 无。它只是注释。

### 17.4 Sidecar 进程绕开 CommandRunner

**当前。** MCP server（`leveler-tools/src/mcp.rs`）、浏览器 driver（`leveler-browser/src/driver.rs`）和 language server（`leveler-lsp/src/client.rs`、`registry.rs`）都是用 `Command::new` 直接拉起，不经 `leveler_execution::CommandRunner`。

**期望。** 要么让它们进入宿主权威的进程树终止与沙箱语义，要么把这个豁免写成一条显式命名的策略，而不是实现上的偶然。

**为什么部分违宪。** 违反规则 4。它们不是模型请求的命令，所以权限与审批路径本就不适用；但它们仍是宿主进程，而宿主权威本应拥有每一个宿主进程。

**最小修正。** 给这一类命名——「runtime sidecar」——并给它一个确定的生命周期 owner，而不是三个各自为政的 spawn 点。

**风险。** 低到中。Sidecar 的存活期已经和 daemon 关停时的 reaping 缠在一起，改 spawn 路径会碰到那部分。

### 17.5 Engine 直接 shell 出去调 git

**当前。** `leveler-engine/src/baseline.rs` 和 `engine.rs` 直接 `Command::new("git")` 来打 base commit，而 `leveler-vcs` 存在，且直接 spawn 进程数为 0。

**期望。** Engine 问 VCS 能力，VCS 问宿主权威。

**为什么违宪。** 违反规则 4，同时把一个领域操作（git）放进了 runtime 层。

**最小修正。** 把 baseline 读取改走 `leveler-vcs`。

**风险。** 低。只有一次只读调用。

---

## 18. 架构变更规则

1. **先在 Foundation 之上扩展，再考虑改它。** 能放在 harness 或产品里的变更，就该放在那儿。
2. **改 Foundation 需要证据**，不是优雅：两个真实实现、一个真实的依赖倒置边界、一处观察到的耦合或 ownership 缺陷，或一条独立的协议 / 安全 / 持久化 / runtime 边界。
3. **动手前先回答 `AGENTS.md` 里的架构决策测试**，写在 PR 里，不是事后补。
4. **不要把目标写成现状。** 如果一次变更朝某个边界推进但没到位，去更新 §17，而不是删掉那一条。
5. **不要为不存在的产品预建接口。** 架构必须允许 Review harness 出现，但这不等于现在就把它建出来。
6. **本文是唯一 canonical 架构文档。** 不要新建 `ARCHITECTURE_V2.md`、`FOUNDATION_*.md` 或所谓 final 版本。要改就改本文；中文版跟随英文版。

Roadmap 类内容——多 agent 方向、浏览器方向、Review 产品、云、ACP、远程 worker、NPC 工作流、未来 provider、未来 UI——都不是架构。本文可以描述扩展点，但不承诺功能。
