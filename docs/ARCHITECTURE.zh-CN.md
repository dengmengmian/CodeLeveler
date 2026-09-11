# CodeLeveler 架构

本文描述 CodeLeveler 的长期架构模型、核心设计思想、职责边界、架构不变量、允许与禁止的架构变化，以及未来演进方向。

英文版：[`ARCHITECTURE.md`](ARCHITECTURE.md)。中英文版本应保持语义一致。

本文只回答四个问题：

1. CodeLeveler 是什么样的系统？
2. 为什么要这样划分职责与边界？
3. 在这套架构下，什么可以做，什么不可以做？
4. 未来应该沿什么架构方向演进？

本文不记录阶段性实现状态、迁移历史、已知缺陷、验证结果、具体源码位置或某一版本的实现细节。

---

## 1. 架构愿景

CodeLeveler 不应被理解成一个不断堆叠能力的 Coding Agent。

它的长期定位是：

> **一个可复用的 Agent Runtime / Harness Foundation。Coding Agent 是建立在它之上的第一个产品，而不是系统的最高抽象。**

系统应该允许不同领域的 Agent 产品建立在同一套 Runtime、Capability、Authority 与 Persistence 基础之上，而不要求这些产品继承 Coding 语义。

最简模型如下：

```text
Model
  ↓
Harness
  ↓
Agent Runtime
  ↓
Capabilities
  ↓
Host Authority
  ↓
Operating System
```

加入产品层后：

```text
                         Products
                            │
              ┌─────────────┼─────────────┐
              ▼             ▼             ▼
            Coding        Review        Future
              │             │             │
              └─────────────┼─────────────┘
                            ▼
                         Harnesses
                            │
                            ▼
                     Agent Runtime
                            │
                  ┌─────────┴─────────┐
                  ▼                   ▼
             Capabilities        Persistence
                  │                   │
                  └─────────┬─────────┘
                            ▼
                     Host Authority
                            │
                            ▼
                     Operating System
```

这套结构的目标不是追求抽象数量，而是让每一类复杂度拥有正确的 Owner。

---

## 2. 核心设计思想

CodeLeveler 的架构可以压缩为七条原则。

```text
Model owns intelligence.
Harness owns domain semantics.
Runtime owns lifecycle and mechanical correctness.
Capability owns reusable domain capability.
Host Authority owns controlled real side effects.
Every persistent fact has one authoritative owner.
Product projects runtime truth; it does not create it.
```

中文表达：

```text
模型拥有智能。
Harness 拥有领域语义。
Runtime 拥有生命周期与机械正确性。
Capability 拥有可复用的领域能力。
Host Authority 拥有受控真实副作用。
每一个持久事实只有一个权威 Owner。
Product 只投影视图，不创造 Runtime 真相。
```

其中最重要的边界之一是：

> **Engine owns lifecycle, not agent intelligence.**

也就是：Engine 管运行，不替 Agent 思考。

---

## 3. Model：智能属于模型

模型负责需要推理的部分：

```text
理解目标
规划
调查
工具选择
代码与内容生成
调试
权衡
语义判断
从失败中恢复
```

Runtime 和 Harness 应该给模型提供：

```text
清晰的能力
稳定的语义
精确的错误
真实的环境状态
确定性的机械约束
可靠的执行
```

但不应该因为模型能力不足而额外模拟一套“替模型思考”的系统。

因此：

```text
模型能力上限
    ≠
Runtime 缺陷
```

模型智能是系统输入，不是 Runtime 必须拉平的变量。

---

## 4. Harness：领域属于 Harness

Harness 是 Model 与通用 Runtime 之间的领域层。

它负责解释：

> 在这个产品领域里，Agent 可以做什么、看到什么、如何与 Runtime 交互，以及什么叫领域上的完成。

例如 Coding Harness 可以拥有：

```text
Coding 领域契约
仓库语义
Coding Tool Surface
验证语义
委派语义
写入协作语义
Coding Completion Semantics
```

未来 Review Harness 可以拥有：

```text
Review Target
Review Scope
Finding
Severity
Evidence
Review Verdict
Review Completion Semantics
```

这些 Harness 是兄弟关系：

```text
                 Agent Runtime
               /       |        \
              ▼        ▼         ▼
           Coding    Review    Research
           Harness   Harness   Harness
```

一个 Harness 不应继承另一个 Harness 的领域语义。

---

## 5. Agent Runtime

Agent Runtime 是 CodeLeveler 的通用运行基础。

它由两个逻辑部分组成：

```text
Agent Kernel
    +
Persistent Runtime
    =
Reusable Agent Runtime
```

### 5.1 Agent Kernel

Agent Kernel 负责 Agent 与模型交互的通用循环：

```text
model interaction
tool loop
streaming
round lifecycle
budget
usage
retry
backoff
deadline
cancel
stop
```

它不应该知道：

```text
Coding
Review
Repository Workflow
某种产品 Prompt
某种领域的完成定义
UI
用户验收
```

Kernel 的价值在于：任何 Harness 都可以在同一 Agent Loop 上工作。

### 5.2 Persistent Runtime / Engine

Engine 负责长期运行需要的机械生命周期：

```text
Session
Task
Turn
Event
Persistence
Resume
Recovery
Cancellation
Background Lifecycle
Ownership
Runtime Outcome
```

核心边界：

```text
Engine owns lifecycle.
Harness owns domain semantics.
Model owns reasoning.
```

Engine 可以知道一个 Task 是否运行、暂停、取消、恢复或结束，但不应该自己定义 Coding 任务“是否真正完成”。

---

## 6. Capability Architecture

Capability 表达系统真正拥有的可复用能力。

典型能力包括：

```text
Workspace
Command Execution
Browser
Search
Code Intelligence
Version Control
Memory
Media
Skills
Remote Execution
External Services
```

Capability 的核心原则是：

> **Capability 表达系统会做什么，Tool 表达模型如何调用它。**

### 6.1 Capability 不等于 Tool

例如：

```text
Model
  ↓
read_file
  ↓
Workspace Capability
  ↓
Host filesystem authority
```

`read_file` 是模型接口。

Workspace 才是真正的可复用能力。

因此：

```text
Tool ≠ Capability
```

也不要求：

```text
One Capability = One Tool
```

一个 Capability 可以被多个 Tool 暴露，也可以被 Harness、Runtime 或 Product 在适当边界直接使用。

### 6.2 Tool thin, Capability thick

Tool 应尽量薄。

Tool 负责：

```text
模型可见的名称与描述
schema
输入解码
能力调用
结果渲染
精确错误
```

Capability 负责：

```text
真实领域行为
可复用逻辑
能力自身状态
能力生命周期
一致性
```

长期状态、服务发现、共享 Runtime、权限裁决不应该因为“工具需要”而被塞进 Tool。

### 6.3 Capability 是职责，不是强制物理模块

Capability 首先是职责边界，不代表必须拥有一个独立 crate、服务、进程或 trait。

物理拆分应该由真实边界驱动，例如：

```text
多个真实消费者
独立生命周期
独立安全边界
独立协议边界
独立持久化边界
远程部署需求
```

而不是为了让架构图对称。

---

## 7. Tool Surface

模型不应该看到系统中所有内部能力。

模型只应该看到当前 Harness 为当前产品选择并暴露的 Tool Surface。

```text
Capabilities
     │
     ▼
Harness Selection
     │
     ▼
Tool Surface
     │
     ▼
Model
```

Tool Surface 的目标是：

> 在不损失必要能力的前提下，提供尽可能清晰、稳定、低歧义的模型操作面。

好的模型工具应该：

```text
表达清晰意图
具有稳定语义
输入输出可预测
错误精确
与其他工具边界清楚
```

系统可以根据真实机械能力决定某个 Tool 是否可用，但不能根据“这个模型比较弱”或“这个任务看起来比较难”偷偷切换工具语义。

---

## 8. Host Authority

Agent 可以提出副作用请求，但不拥有真实宿主权力。

核心原则：

```text
Agent proposes.
Host Authority decides and performs.
```

逻辑关系：

```text
Model / Harness
      ↓
Side-effect request
      ↓
Host Authority
      ↓
Operating System
```

Host Authority 统一拥有受控真实副作用，例如：

```text
filesystem mutation
process execution
network authority
sandbox
permission
approval
workspace boundary
process lifecycle
```

这使“Agent 想做什么”和“宿主允许发生什么”保持分离。

任何模型可见接口都不能成为绕开 Authority 的第二条执行路径。

---

## 9. Authority Model

CodeLeveler 区分三种不同层级的真相：

```text
Mechanical Truth
      ≠
Semantic Satisfaction
      ≠
User Acceptance
```

### 9.1 Runtime Authority

Runtime 可以权威记录机械事实，例如：

```text
某个命令是否执行
退出状态
某个文件是否变化
某个事件是否发生
某项验证是否运行
某个 artifact 是否存在
某个进程是否结束
```

### 9.2 Model Semantic Authority

模型负责解释这些事实是否足以满足当前领域目标。

例如：

```text
测试通过
```

不能自动推出：

```text
用户要的功能已经语义完整
```

### 9.3 User Acceptance

用户拥有最终验收权。

因此：

```text
Runtime owns facts.
Model owns semantic interpretation.
User owns acceptance.
```

---

## 10. Persistence Architecture

长生命周期 Agent 必须建立在可靠持久化之上。

核心原则：

```text
One persistent fact
      ↓
One authoritative owner
      ↓
One canonical representation
```

Task 状态、Turn 状态、Ownership、Evidence、Usage、Artifact、Background Work 等持久事实都遵守这一原则。

### 10.1 Persist Before Forward

Runtime 产生的权威事件应遵循：

```text
Runtime Fact
    ↓
Persist
    ↓
Forward / Project
    ↓
Client
```

客户端不应先看到一个 Runtime 自己还没有可靠记录的权威事实。

### 10.2 Persistence 提供连续性

Persistence 的长期目的不仅是“保存聊天记录”，而是提供：

```text
resume
recovery
cross-device continuity
background work
long-running tasks
child session durability
auditability
```

---

## 11. Product Architecture

Product 层负责体验、组合与交付。

例如：

```text
CLI
TUI
Web
Desktop
Mobile
Remote Client
```

Product 可以决定：

```text
信息如何展示
交互如何组织
哪些能力组合成一个产品
默认体验
导航
可视化
```

但 Product 不拥有 Runtime Truth。

核心原则：

> **UI is a projection of Runtime truth.**

Product 可以拥有本地视图状态，但任务、运行、权限、工具执行、持久化事实等权威状态必须来自对应 Owner。

---

## 12. Client / Runtime Boundary

客户端与 Runtime 之间应该通过稳定协议连接，而不是共享内部状态。

```text
Client Command
      ↓
Runtime
      ↓
Runtime Event
      ↓
Client
```

同一个 Runtime 可以拥有多个客户端：

```text
             Runtime
          /     |      \
         ▼      ▼       ▼
       TUI     Web    Mobile
```

客户端也可以和 Runtime 位于不同机器：

```text
Client
  ↓
Transport
  ↓
Remote Runtime
```

这使本地、远程和云端运行共享同一套产品协议思想。

---

## 13. Multi-Agent Architecture

Multi-Agent 不是简单的“一个 Agent 再调用几个模型”。

它是多个 Agent 生命周期在统一 Runtime Authority 下协作。

```text
                 Parent Agent
                /      |      \
               ▼       ▼       ▼
          Explorer   Worker   Reviewer
               │       │       │
               └───────┼───────┘
                       ▼
                 Shared Runtime
                       │
          ┌────────────┼────────────┐
          ▼            ▼            ▼
      Persistence   Ownership   Capabilities
```

Multi-Agent Runtime 负责统一的机械协作基础：

```text
child lifecycle
session relationship
ownership
write scope
capability access
background execution
settlement
cancellation
persistence
result handoff
```

角色语义属于 Harness；生命周期与机械正确性属于 Runtime。

Multi-Agent 不应建立一套绕开主 Runtime 的第二套生命周期、权限或持久化系统。

---

## 14. Provider & Model Boundary

不同模型 Provider 具有不同协议和机械能力。

这些差异属于 Provider / Protocol 层，而不是 Harness 智能补偿层。

可协商的事实包括：

```text
tool calling
streaming
reasoning transport
vision
structured output
forced tool choice
context window
output limit
wire format
```

系统应该：

```text
发现能力
显式协商
如实上报
根据机械条件启用或关闭依赖能力
```

而不是：

```text
为了模拟缺失能力而改变工具语义
为了拉平模型水平而增加隐藏行为
```

---

## 15. Dependency Direction

架构依赖方向必须保持单向。

```text
Foundation
    ↑
Runtime / Capabilities
    ↑
Harnesses
    ↑
Products
```

换一个角度：

```text
Product
   ↓
Harness
   ↓
Runtime / Capabilities
   ↓
Foundation
```

核心要求：

```text
Runtime 不依赖具体 Harness。
Harness 不依赖具体 Product UI。
Foundation 不依赖上层领域概念。
Capability 不依赖调用它的具体 Tool 或 Product。
```

下层可以提供机制，上层负责组合与语义。

---

## 16. 一次任务的概念运行流

从用户请求到宿主副作用：

```text
User
  ↓
Product
  ↓
Harness
  ↓
Agent Runtime
  ↓
Model
  ↓
Tool Intent
  ↓
Harness Tool Surface
  ↓
Capability
  ↓
Host Authority
  ↓
Operating System
```

结果返回：

```text
Operating System
  ↓
Capability Result
  ↓
Runtime Fact
  ↓
Persistence
  ↓
Runtime Event
  ↓
Harness / Product
  ↓
User
```

这条链路体现三个核心约束：

1. Model 不直接拥有宿主副作用。
2. Runtime 记录机械事实，但不取代领域语义。
3. Product 展示事实，但不重新定义事实。

---

## 17. 架构不变量

以下规则是 CodeLeveler 的长期架构宪法。

### 17.1 Intelligence Boundary

```text
Model owns intelligence.
Runtime must not simulate agent intelligence.
```

### 17.2 Harness Boundary

```text
Harness owns domain semantics.
Runtime must not define Coding, Review, or other product semantics.
```

### 17.3 Lifecycle Boundary

```text
Engine owns lifecycle, not agent intelligence.
```

### 17.4 Capability Boundary

```text
Tool is an adapter.
Capability is the reusable ability.
```

### 17.5 Authority Boundary

```text
Agent proposes.
Host Authority decides and performs.
```

### 17.6 Persistence Boundary

```text
One persistent fact, one authoritative owner.
```

### 17.7 Product Boundary

```text
Product projects truth.
Product does not create runtime truth.
```

### 17.8 Dependency Boundary

```text
Dependencies point downward toward more general layers.
Lower layers do not depend on product-specific layers.
```

### 17.9 Multi-Harness Boundary

```text
A new Harness must not require redesigning Agent Runtime.
```

### 17.10 Reliability Boundary

```text
Moving responsibility between layers must not weaken mechanical correctness,
authority, persistence, cancellation, recovery, or safety guarantees.
```

---

## 18. 架构允许与禁止

这一章是架构变更的直接判定规则。

不是“建议”，而是对后续设计与实现的边界约束。

### 18.1 可以做

#### A. 在 Product / Harness 层增加领域能力

允许：

```text
增加 Coding 专属工作流
增加 Review 语义
增加 Research 等新 Harness
调整某个产品的 Tool Surface
增加领域 Completion Semantics
```

前提是这些语义留在对应 Harness / Product，不向通用 Runtime 下沉。

#### B. 扩展领域中立的 Runtime 机制

允许 Runtime 增加真正通用的机械能力，例如：

```text
新的生命周期状态
更可靠的 resume / recovery
更好的 cancellation
通用 background lifecycle
通用 parent / child session 关系
通用资源预算
通用事件与持久化机制
```

前提是能力不需要理解 Coding、Review 或其他具体领域语义。

#### C. 增加可复用 Capability

允许增加新的 Capability，例如：

```text
Browser
Remote Execution
Search
External Service
New Code Intelligence
New Workspace Ability
```

当它代表真实可复用能力，并拥有明确职责边界时，可以独立演进。

#### D. 为 Capability 增加新的 Tool Adapter

允许针对新的模型意图暴露 Tool。

前提是：

```text
Tool 只是适配器
语义稳定
不复制 Capability Runtime
不拥有新的 Authority
不成为持久状态 Owner
```

#### E. 扩展 Provider / Protocol Adapter

允许适配新的模型、协议和 wire format。

Provider 差异可以被规范化，但不能通过改变 Agent 行为语义来伪造模型不存在的机械能力。

#### F. 增加新的 Product / Client

允许构建：

```text
新的 TUI / Web / Desktop / Mobile 客户端
远程控制端
新的 Agent 产品
新的领域 Harness
```

只要它们消费已有 Runtime Truth，而不是创建第二套 Runtime Truth。

#### G. 增加 Local / Remote / Cloud 执行后端

允许 Host Authority 支持不同执行位置：

```text
Local Host
Remote Host
Container
VM
Cloud Worker
```

执行位置可以变化，但 Authority 与 Agent 语义边界保持一致。

#### H. 在有真实证据时引入新抽象

新的 crate、trait、registry、manager、adapter layer、通用 framework 可以出现，但至少应该存在一种真实驱动力：

```text
两个真实实现或消费者
真实 dependency inversion
独立安全边界
独立协议边界
独立持久化边界
独立生命周期
远程部署边界
已观察到的 ownership / coupling 问题
```

抽象来自真实边界，不来自想象中的未来。

---

### 18.2 不可以做

#### A. 不得把产品语义下沉进通用 Runtime

禁止让 Kernel、Engine 或 Foundation 理解：

```text
Coding Prompt
Review Finding
Repository Workflow
某种产品的 Tool Preference
某种领域的 Done 定义
```

这些属于 Harness。

#### B. 不得让 Runtime 替模型思考

禁止因为模型能力不足而在 Runtime / Harness 中加入隐藏推理补偿，例如：

```text
替模型决定什么时候规划
替模型决定下一步调查什么
因为模型选错 Tool 自动换另一个 Tool
隐藏的任务级解题重试
为了弱模型复制第二套工具语义
根据模型强弱改变领域行为
```

机械重试、网络恢复、协议适配、schema 校验等工程可靠性不属于此禁令。

#### C. 不得建立第二条 Host Side-effect Authority

禁止 Tool、Harness、Product 或插件绕开统一 Authority，直接建立平行的：

```text
filesystem mutation path
process execution path
permission path
approval path
sandbox path
workspace write authority
```

同一种受控副作用不能存在多个互相竞争的 Authority。

#### D. 不得让 Tool 成为 Runtime 或 Service Locator

禁止把通用服务集合、长期状态、权限决策、进程生命周期、全局策略塞进每一个 Tool。

Tool 不应该因为拿到一个万能 Context 就能访问所有 Capability。

#### E. 不得建立多个持久事实真相源

禁止：

```text
UI 自己推导 Task 真状态
Harness 和 Engine 各保存一份权威状态
事件流和数据库分别成为独立权威
多个组件互相覆盖同一个事实
```

一个事实必须有一个 canonical Owner。

#### F. 不得产生反向依赖

禁止：

```text
Foundation → Harness
Runtime →具体 Product
Capability →具体 UI
通用 Harness →另一个领域 Harness
```

下层不能为了方便直接依赖上层产品。

#### G. 不得用改变语义的 fallback 假装能力存在

禁止：

```text
正则搜索失败后悄悄变成字面量搜索
结构化编辑失败后偷偷执行另一种编辑语义
Provider 不支持某能力却通过另一条行为路径假装支持
```

Fallback 可以替换实现，但必须保持同一个契约和同一个含义。

#### H. 不得根据任务或模型“聪明程度”动态改变机械能力边界

Capability 是否可用应该由真实机械条件决定，例如：

```text
模型协议能力
宿主能力
Runtime 能力
Harness 要求
用户配置
```

不能由：

```text
任务看起来很难
模型看起来比较弱
模型这轮表现不好
```

来决定系统悄悄换一套语义。

#### I. 不得为假想未来预建框架

禁止仅因为：

```text
以后可能需要
更通用
Clean Architecture 看起来更完整
未来也许有第二个实现
```

就增加新的通用抽象。

架构必须允许未来扩展，但不等于提前实现未来。

#### J. 不得让 Multi-Agent 绕开统一 Runtime

禁止为 Child Agent / Reviewer / Worker 单独建立另一套：

```text
生命周期
权限
Ownership
Persistence
Cancellation
Settlement
```

角色可以不同，机械运行规则必须统一。

#### K. 不得把 Local-only 假设写进 Agent 语义

Agent、Harness 与 Runtime 的核心语义不应该假设执行一定发生在当前机器。

本地、远程、容器与云端应该是 Execution Authority 的部署差异，而不是四种不同 Agent 架构。

---

### 18.3 架构变更判定

一个改动进入 Foundation / Runtime / Capability 前，应能回答：

```text
它的 Owner 是谁？
它是领域语义还是机械机制？
它是否能被第二个 Harness 合理复用？
它是否创造了新的 Authority？
它是否创造了新的持久事实真相源？
它是否产生反向依赖？
它是否改变已有能力语义？
它是否只是为了假想未来而抽象？
```

如果一个功能可以自然留在 Harness，就不应该为了“通用”而下沉到 Foundation。

如果一个新 Harness 需要先修改 Agent Kernel 才能成立，应首先怀疑边界设计，而不是默认 Kernel 需要继续吸收领域概念。

---

## 19. 演进方向

CodeLeveler 的未来演进重点不是继续扩大单个 Coding Agent，而是让这套 Foundation 支撑更广泛的 Agent 产品与运行形态。

这些是长期架构方向，不是具体版本 Roadmap。

### 19.1 Multi-Harness

从单一 Coding 产品演进到多领域 Harness：

```text
                    Agent Runtime
                 /       |        \
                ▼        ▼         ▼
             Coding    Review    Research
             Harness   Harness   Harness
```

每个 Harness：

```text
拥有自己的领域语义
选择自己的 Capability
定义自己的 Tool Surface
拥有自己的 Completion Semantics
```

Runtime 保持领域中立。

### 19.2 Multi-Agent Runtime

从单 Agent 生命周期扩展到 Agent 协作图：

```text
Single Agent
    ↓
Parent / Child
    ↓
Role-based Agents
    ↓
Multi-Agent Runtime
```

未来角色可以包括：

```text
Planner
Explorer
Worker
Reviewer
Specialist
```

但统一共享：

```text
lifecycle
persistence
ownership
authority
capability negotiation
cancellation
settlement
```

Multi-Agent 是 Runtime 能力的扩展，而不是第二套系统。

### 19.3 Capability Platform

Capability 将从“Coding Agent 使用的一组后端”演进为独立可组合的平台能力。

```text
                  Capabilities
              /        |         \
             ▼         ▼          ▼
          Coding     Review      Other
```

长期覆盖：

```text
Workspace
Execution
Browser
Search
Memory
VCS
Code Intelligence
Media
Remote Compute
External Services
```

Harness 根据领域需要组合能力，而不是让所有 Agent 继承一个巨大工具集合。

### 19.4 Local → Remote → Cloud

Runtime 与 Host Authority 不绑定在同一台机器。

长期结构：

```text
Harness
   ↓
Agent Runtime
   ↓
Execution Authority
   ├── Local Host
   ├── Remote Host
   ├── Container
   ├── VM / Isolated Worker
   └── Cloud Worker
```

区别只在执行位置，不在 Agent 核心语义。

### 19.5 Durable Agents

Agent 生命周期会越来越长。

Runtime 应天然支持：

```text
long-running goals
pause / resume
background execution
cross-device continuation
child session durability
recoverable work
persistent context
```

Agent 不再等价于一次聊天请求，而是可持续运行、可恢复的任务实体。

### 19.6 Capability Negotiation

不同模型、宿主和 Worker 拥有不同机械能力。

系统应该显式协商：

```text
model capabilities
host capabilities
runtime capabilities
harness requirements
user configuration
```

最终能力来自这些条件的交集，而不是隐藏猜测。

```text
Available Capability
    =
Model ∩ Host ∩ Runtime ∩ Harness ∩ Configuration
```

缺少某项机械能力时，诚实关闭依赖它的功能，而不是改变语义假装能力存在。

### 19.7 Agent Platform

最终形态不是“更复杂的 Coding Agent”，而是一套可以构建不同 Agent 产品的运行平台。

```text
                         Products
             ┌─────────────┼─────────────┐
             ▼             ▼             ▼
          Coding         Review         Others
             │             │             │
             └─────────────┼─────────────┘
                           ▼
                        Harnesses
                           │
                           ▼
                     Agent Runtime
                           │
                 ┌─────────┴─────────┐
                 ▼                   ▼
          Capability Platform    Persistence
                 │                   │
                 └─────────┬─────────┘
                           ▼
                     Host Authority
                           │
                ┌──────────┼──────────┐
                ▼          ▼          ▼
              Local      Remote      Cloud
```

在这个模型里：

```text
Model 提供智能。
Harness 提供领域。
Runtime 提供生命周期与机械正确性。
Capability 提供可复用能力。
Authority 提供受控真实执行。
Persistence 提供连续性。
Product 提供体验。
```

这就是 CodeLeveler 长期架构的核心方向。
