# CodeLeveler 架构

本文描述 CodeLeveler 的长期架构模型、核心设计思想、职责边界、架构不变量以及未来演进方向。

本文只回答三个问题：

1. CodeLeveler 是什么样的系统？
2. 为什么要这样划分职责与边界？
3. 未来应该沿什么架构方向演进？

本文不记录阶段性实现状态、迁移历史、已知缺陷、验证结果或具体代码实现。

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
                         Harness
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
Host Authority owns real side effects.
Persistent facts have one authoritative owner.
Product projects runtime truth.
```

对应到系统职责：

```text
模型拥有智能。
Harness 拥有领域语义。
Runtime 拥有生命周期与机械正确性。
Capability 拥有可复用领域能力。
Host Authority 拥有真实副作用。
持久事实只有一个权威 Owner。
Product 表达 Runtime 真相，而不创造新的 Runtime 真相。
```

其中最重要的一条边界是：

> **Engine owns lifecycle, not agent intelligence.**

Runtime 可以保证一个任务如何开始、持续、取消、恢复、记录和结束，但不应该替模型决定如何思考，也不应该替 Harness 定义某个领域中“完成”意味着什么。

---

## 3. 智能、语义与机械正确性

Agent 系统最容易出现的问题，是把模型智能、领域语义和运行时可靠性混在一起。

CodeLeveler 将三者严格分开。

### 3.1 Model：智能

Model 负责：

```text
理解目标
推理
规划
选择能力
决定工具调用
解释观察结果
调试
从错误中恢复
判断下一步
```

模型能力是系统的输入，不是 Runtime 必须抹平的变量。

Runtime 不应该通过隐藏的行为补偿去模拟一个更聪明的模型。

---

### 3.2 Harness：领域语义

Harness 负责告诉模型：

```text
当前是什么领域
有哪些领域能力
领域中的关键协议是什么
什么状态具有领域意义
什么行为是允许的
什么行为代表领域层面的结束
```

例如 Coding Harness 可以理解 repository、goal、verification、delegation、write ownership；Review Harness 可以理解 finding、severity、scope、verdict。

这些语义不属于通用 Runtime。

---

### 3.3 Runtime：机械正确性

Runtime 负责可机械证明的行为：

```text
生命周期
事件顺序
取消
预算
超时
持久化
恢复
并发安全
权限
进程状态
副作用边界
事实记录
```

Runtime 保证的是系统“如何可靠地运行”，不是模型“应该如何完成任务”。

---

## 4. 总体分层

CodeLeveler 采用六层逻辑架构。

```text
┌──────────────────────────────────────────────┐
│                   Product                    │
│      CLI / TUI / Web / Desktop / Mobile      │
└──────────────────────┬───────────────────────┘
                       │
┌──────────────────────▼───────────────────────┐
│                   Harness                    │
│        Coding / Review / Future Agents        │
└──────────────────────┬───────────────────────┘
                       │
┌──────────────────────▼───────────────────────┐
│                Agent Runtime                 │
│   Model Loop / Session / Turn / Task / Life  │
└──────────────┬──────────────────┬────────────┘
               │                  │
               ▼                  ▼
┌──────────────────────┐  ┌──────────────────────┐
│     Capabilities     │  │     Persistence      │
│ Workspace / Browser  │  │ Event / State / Fact │
│ Execution / VCS / …  │  │ Resume / Recovery    │
└──────────────┬───────┘  └───────────┬──────────┘
               │                      │
               └──────────┬───────────┘
                          ▼
┌──────────────────────────────────────────────┐
│               Host Authority                 │
│ FS / Process / Network / Sandbox / Permission│
└──────────────────────┬───────────────────────┘
                       ▼
                Operating System
```

每一层只拥有自己的问题。

下层提供确定性能力；上层组合并赋予语义。

---

## 5. Agent Runtime

Agent Runtime 是 CodeLeveler 的通用执行基础。

它由两个概念组成：

```text
Agent Kernel
    +
Persistent Runtime
    =
Reusable Agent Runtime
```

---

### 5.1 Agent Kernel

Agent Kernel 管理一次 Agent 执行过程中与模型交互有关的机械循环。

它负责：

```text
model interaction
streaming
round lifecycle
tool loop
retry
budget
cancel
deadline
stop
usage accounting
```

它不理解：

```text
Coding
Review
repository workflow
finding
用户验收
领域完成语义
产品 UI
```

Kernel 的作用是提供一个稳定的 Agent execution loop，而不是提供某个具体 Agent 的“大脑”。

---

### 5.2 Persistent Runtime

Persistent Runtime 管理跨一次模型调用之外仍然存在的生命周期。

它拥有：

```text
Session
Task
Turn
Event
Lifecycle
Persistence
Resume
Recovery
Cancellation
Background lifecycle
Ownership state
Runtime facts
```

核心思想是：

> Runtime 管运行事实，Harness 管领域意义。

例如 Runtime 可以知道一个 Turn 已经结束，但不需要知道 Coding 任务是否满足了用户的语义目标。

---

### 5.3 生命周期是一等公民

Agent 不应被实现成一次不可恢复的函数调用。

长期任务天然具有生命周期：

```text
created
  ↓
running
  ↓
waiting / blocked / suspended
  ↓
running
  ↓
completed / cancelled / failed
```

因此 Session、Task、Turn、Event、Resume、Recovery 都是 Runtime 的一级概念，而不是 UI 的附加状态。

---

## 6. Harness 架构

Harness 是 Model 与通用 Runtime 之间的领域层。

它的职责不是替模型思考，而是定义一个 Agent 产品的语义空间。

```text
Model
  ↓
Domain Harness
  ↓
Agent Runtime
```

### 6.1 Harness 拥有什么

一个 Harness 通常拥有：

```text
领域身份
领域 prompt contract
领域状态
领域工具面
领域能力组合
领域协议
领域权限约束
领域 completion semantics
领域 delegation semantics
领域 verification semantics
```

不同 Harness 可以共享 Runtime 和 Capability，但不共享彼此的领域语义。

---

### 6.2 Coding Harness

Coding Harness 的领域是软件工程。

它理解：

```text
repository
goal
code change
verification
workspace ownership
delegation
coding completion
```

Coding Harness 可以组合 Workspace、Command Execution、VCS、Code Intelligence、Browser、Memory 等能力。

但这些能力本身不应该被定义成“只能给 Coding 使用”。

---

### 6.3 Multi-Harness

长期架构要求不同 Harness 是兄弟关系。

```text
                  Agent Runtime
               /       |        \
              ▼        ▼         ▼
           Coding    Review     Other
           Harness   Harness    Harness
```

不应该形成：

```text
Coding Agent
    └── Review
        └── Research
            └── Other
```

否则 Coding 会再次成为公共系统的最高抽象。

一个新的 Harness 应该能够选择自己需要的能力、定义自己的工具面和领域协议，而不要求继承 Coding Harness。

---

## 7. Capability 架构

Capability 是系统真正可复用的领域能力。

典型 Capability 包括：

```text
Workspace
Command Execution
Browser
Code Intelligence
Version Control
Memory
Search
Media
Skills
Remote Execution
```

Capability 的核心原则是：

> **Capability 表达系统会做什么，Tool 表达模型如何调用它。**

---

### 7.1 Capability 不等于 Tool

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

Workspace 才是真正负责文件读取语义和边界的能力。

因此：

```text
Tool ≠ Capability
```

也不要求：

```text
One Capability = One Tool
```

同一个 Capability 可以通过多个工具暴露，也可以被 Runtime 或 Product 直接使用。

---

### 7.2 Tool thin, Capability thick

Tool 应该尽量薄。

Tool 负责：

```text
name
schema
input decoding
result rendering
模型可理解的错误
```

Capability 负责：

```text
真实领域行为
状态
生命周期
一致性
可复用逻辑
```

这可以避免把长期状态、服务发现、权限逻辑、运行时生命周期不断塞进模型工具层。

---

### 7.3 Capability 是职责，不是强制的物理模块

架构中的 Capability 首先是职责边界。

它不要求每一个能力都必须成为独立包、独立服务或独立进程。

物理拆分应该由真实边界驱动，例如：

```text
独立生命周期
独立安全边界
独立协议边界
多个真实消费者
远程部署需求
```

而不是为了让架构图对称。

---

## 8. Tool Surface

模型不应该看到系统中所有内部能力。

模型只应该看到当前 Harness 为当前产品暴露的能力面。

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

Tool Surface 的设计目标是：

> 在不损失必要能力的前提下，暴露尽可能清晰、稳定、低歧义的模型操作面。

一个好的模型工具应该：

```text
表达清晰意图
具有稳定语义
输入输出可预测
错误精确
与其他工具边界清楚
```

Runtime 不应该为了补偿模型能力而偷偷改变一次工具调用的含义。

---

## 9. Host Authority

Agent 可以提出副作用请求，但不拥有真实宿主权力。

核心原则：

```text
Agent proposes.
Host decides and performs.
```

或者：

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
filesystem
process
network
sandbox
permission
approval
workspace boundary
process lifecycle
```

这一层的意义是把“模型想做什么”和“宿主允许发生什么”彻底分开。

模型不能通过选择某个不同工具绕开权限、沙箱或作用域限制。

---

## 10. Authority Model

CodeLeveler 区分三种不同层级的真相。

```text
机械事实
    ≠
语义满足
    ≠
用户验收
```

### 10.1 Runtime Authority

Runtime 可以权威记录：

```text
某个命令是否执行
退出状态
某个文件是否变化
某个事件是否发生
某个验证是否执行
某个 artifact 是否存在
某个进程是否结束
```

这些都是机械事实。

---

### 10.2 Model Semantic Authority

Model 负责解释这些机械事实与用户目标之间的关系。

例如：

```text
测试通过
```

并不能机械推出：

```text
用户真正想要的功能已经完整实现
```

模型需要结合目标、上下文和观察结果做语义判断。

---

### 10.3 User Acceptance Authority

最终验收属于用户。

因此整个体系是：

```text
Runtime owns mechanical truth.
Model owns semantic interpretation.
User owns acceptance.
```

这条边界可以避免 Runtime 越权把局部机械成功解释成任务完成。

---

## 11. Persistence 架构

Agent Runtime 中重要的状态不应该只存在于进程内存。

持久化的核心原则是：

```text
One persistent fact
    ↓
One authoritative owner
    ↓
One canonical representation
```

系统需要持久化的不是 UI 快照，而是 Runtime 事实。

典型内容包括：

```text
Session state
Task state
Turn state
Events
Usage
Ownership
Artifacts
Verification facts
Lifecycle outcome
```

---

### 11.1 Persist before forward

对外可观察的 Runtime 事实应该先成为持久事实，再被客户端观察。

```text
Runtime Event
    ↓
Persist
    ↓
Forward
    ↓
Client
```

这使客户端看到的状态可以恢复、重放和审计。

UI 不应成为 Runtime 状态的唯一保存者。

---

### 11.2 Event-driven projection

产品层看到的是 Runtime 的投影。

```text
Runtime Facts
     ↓
Events
     ↓
Client Projection
     ↓
TUI / Web / Desktop / Mobile
```

不同客户端可以拥有不同展示方式，但不能因为展示方式不同而创造不同的 Runtime 真相。

---

## 12. Product 架构

Product 层负责用户体验、组合和交付。

它包括：

```text
Application composition
CLI
TUI
Web
Desktop
Mobile
Remote control
```

Product 层可以决定：

```text
如何展示任务
如何组织工作区
如何呈现计划
如何展示 diff
如何请求审批
如何切换模型
如何暴露产品模式
```

但 Product 不应拥有：

```text
任务真实生命周期
真实权限状态
真实工具执行结果
真实持久事实
领域 completion truth
```

核心原则：

> **UI is a projection of Runtime truth.**

---

## 13. Client / Runtime Boundary

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

这种边界允许同一个 Runtime 被不同客户端连接：

```text
             Runtime
          /     |      \
         ▼      ▼       ▼
       TUI     Web    Mobile
```

也允许客户端与 Runtime 不在同一台机器：

```text
Client
  ↓
Transport
  ↓
Remote Runtime
```

这为本地、远程和云端运行提供统一基础。

---

## 14. Multi-Agent 架构

Multi-Agent 不应该被理解成“一个 Agent 可以再调用几个模型”。

它本质上是多个 Agent 生命周期在同一个 Runtime Authority 下协作。

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

Multi-Agent Runtime 需要统一管理：

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

Agent 之间可以具有不同角色和能力，但仍然共享同一套机械正确性与 Authority 规则。

---

## 15. Dependency Direction

架构依赖方向应该保持单向。

```text
Foundation
    ↑
Runtime / Capabilities
    ↑
Harnesses
    ↑
Products
```

更具体地说：

```text
Product
   ↓
Harness
   ↓
Runtime
   ↓
Foundation
```

Capability 与 Runtime 可以共享 Foundation，但不应反向依赖具体 Product。

Harness 可以依赖 Runtime；Runtime 不应该依赖某个具体 Harness。

Product 可以依赖 Harness；Harness 不应该依赖某个具体 UI。

这是系统可复用性的基础。

---

## 16. 一次任务的概念运行流

从用户请求到宿主副作用，逻辑流程如下：

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

这条链路体现了三个关键思想：

1. Model 不直接拥有宿主副作用。
2. Runtime 记录机械事实，但不取代领域语义。
3. Product 展示事实，但不重新定义事实。

---

## 17. 架构不变量

下面这些规则应视为 CodeLeveler 的长期架构宪法。

### 17.1 Intelligence Boundary

```text
Model owns intelligence.
Runtime must not simulate agent intelligence.
```

Runtime 不负责决定模型应该怎样规划、调查或推理。

---

### 17.2 Harness Boundary

```text
Harness owns domain semantics.
Runtime must not define Coding semantics.
```

Coding、Review 或未来领域的词汇不应成为通用 Runtime 的基础概念。

---

### 17.3 Lifecycle Boundary

```text
Engine owns lifecycle, not agent intelligence.
```

Session、Task、Turn、Event、Resume、Recovery 属于 Runtime。

领域目标如何完成属于 Harness 与 Model。

---

### 17.4 Capability Boundary

```text
Tool is an adapter.
Capability is the reusable ability.
```

模型工具不应成为长期状态、服务发现或共享运行时逻辑的 Owner。

---

### 17.5 Authority Boundary

```text
Agent proposes.
Host Authority performs.
```

所有受控真实副作用必须服从统一宿主 Authority。

---

### 17.6 Persistence Boundary

```text
One fact, one authoritative owner.
```

同一个持久事实不能存在多个互相推导、互相覆盖的权威来源。

---

### 17.7 Product Boundary

```text
Product projects truth.
Product does not create runtime truth.
```

客户端可以有自己的视图状态，但不能成为任务真实状态的权威。

---

### 17.8 Multi-Harness Boundary

```text
A new Harness must not require redesigning Agent Runtime.
```

一个语义不同的新 Agent 产品应该能够建立在相同 Runtime 上，而不要求 Runtime 获得该产品的领域知识。

---

## 18. 演进方向

CodeLeveler 的未来演进重点不是继续扩大单个 Coding Agent，而是让这套 Foundation 支撑更广泛的 Agent 产品与运行形态。

这些方向是架构演进，不是具体版本 Roadmap。

---

### 18.1 Multi-Harness

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
拥有自己的 completion semantics
```

Runtime 保持领域中立。

---

### 18.2 Multi-Agent Runtime

从单 Agent 生命周期扩展到 Agent 协作图。

```text
Single Agent
    ↓
Parent / Child
    ↓
Role-based Agents
    ↓
Multi-Agent Runtime
```

未来 Agent 可以形成：

```text
Planner
Explorer
Worker
Reviewer
Specialist
```

但无论角色如何变化，都共享统一的：

```text
lifecycle
persistence
ownership
authority
capability negotiation
cancellation
settlement
```

Multi-Agent 是 Runtime 能力的扩展，而不是绕过 Runtime 的第二套系统。

---

### 18.3 Capability Platform

Capability 将从“Coding Agent 使用的一组工具后端”演进为独立可组合的平台能力。

```text
                  Capabilities
              /        |         \
             ▼         ▼          ▼
          Coding     Review      Other
```

长期可以覆盖：

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

Harness 根据领域需要组合能力，而不是让所有 Agent 使用同一套巨大工具集合。

---

### 18.4 Local → Remote → Cloud

Runtime 与 Host Authority 不应绑定在同一台本地机器。

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
   ├── VM / isolated worker
   └── Cloud Worker
```

这样本地 Agent、远程开发机、移动控制端和云端 Worker 可以共享同一个架构模型。

区别只在执行位置，不在 Agent 语义本身。

---

### 18.5 Durable Agents

Agent 生命周期会越来越长。

未来 Runtime 应天然支持：

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

---

### 18.6 Capability Negotiation

不同模型、宿主和 Worker 拥有不同机械能力。

系统应该显式协商：

```text
model capabilities
host capabilities
runtime capabilities
harness requirements
```

最终可用能力是这些条件的交集。

```text
Available Capability
    =
Model ∩ Host ∩ Runtime ∩ Harness
```

缺少某项机械能力时，应诚实关闭依赖它的功能，而不是通过改变语义的 fallback 假装能力存在。

---

### 18.7 Agent Platform

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
Runtime 提供生命周期。
Capability 提供能力。
Authority 提供安全执行。
Persistence 提供连续性。
Product 提供体验。
```

这就是 CodeLeveler 长期架构的核心方向。
