# Explicit Completion Contract V1

## 为什么需要它

Completion Truth 过去由**最后一次阅读**决定：模型重读目标、重读证据、说 satisfied。
两个已实证的缺陷都住在这次阅读里。

| 探针 | 失败链 |
| --- | --- |
| icg-6r | 执行模型发现目标不可满足 → 悄悄重解释 → 完成被替换的版本 → 终判放行 |
| scale-s800 | 原文明写"边界规则要被测试覆盖" → 实现改了、测试没写 → 终判仍说 satisfied |

同一个形状：**要求在它变得不方便的那一刻停止存在**。

前一轮尝试用"更强的异构 Judge"解决，结论是同步 v4-pro 的产品形态不可用
（见 `COMPLETION_RECONCILIATION.md`：`V4_PRO_SYNC_OPERATIONAL_VIABILITY=FAIL`，
语义假设 `UNMEASURED`）。所以这一版不换 Judge，而是换**完成的定义**。

## 架构

```
OriginalGoal（权威，不可变）
    ↓ 目标开始时派生一次
CompletionContract（durable，随 EvidenceLedger 持久化）
    ↓ 执行期每轮注入，用户原话可见
Evidence（复用既有 mutations / verifications / freshness）
    ↓ 完成时逐条对账 + 机械下限（runtime 判，不是 judge 判）
既有语义 gate（二级守卫）
    ↓
Completed
```

**不新建**：Planner、第二个 Goal runtime、第二个 EventLog、验证器调度器、
requirement 依赖图、异步 judge 生命周期、多数投票。

## 数据

`leveler-lifecycle::CompletionContract`（与既有的 `TaskContract` 文本解析器是两回事）：

```rust
CompletionRequirement { id, text, kind, source, status, evidence }
kind:     Behavior | Verification | Constraint | Deliverable | Other
source:   OriginalGoal | ExplicitUserFollowup
status:   Pending | Satisfied | Blocked
evidence: RequirementEvidence { strength, detail }
strength: Mechanical > Observed > Semantic
```

`text` 保留用户原话；模型的转述不是权威。

## 派生时机（§12）

**目标开始、第一个模型轮之前**，一次 bounded 调用（low effort / 4096，执行模型自己，
不是 v4-pro）。这是设计的核心：派生发生在执行模型遇到障碍**之前**，
所以障碍无法反过来塑造义务清单。派生失败 = `None` = **UNAVAILABLE，不是空契约**——
"没找到要求"绝不能从失败里读出来。

## 持久化（§14/§42）

契约挂在 `EvidenceLedger` 上（serde-default，旧快照可重放），
**在循环执行任何 mutating 工具之前落库**。崩溃后重启沿用同一份义务，
不会因为进程死过一次就获得重新解释目标的机会。

## 机械下限（§18/§20/§36/§37）

`Verification` / `Deliverable` 类义务：**判定说 satisfied 不算数**，
必须有 ledger 里的机械事实支撑，且遵守既有 freshness——
被后续改动作废的绿检查不算证据。

无任何文件名启发式（没有 `_test.go` 规则），判定依据是 kind + 真实记录的事实。

`Behavior` / `Constraint` / `Other` 类允许语义证据——目标不是把 Agent 变成定理证明器，
而是**能机械证明的东西不要再靠感觉放行**。

## 完成门（§34/§35/§72）

拒绝完成的三种情况：义务 Pending、义务 Blocked、机械义务缺机械证据。
另加：**契约本身建立不起来也不能完成**（完成时补派生一次，仍失败则拒绝）。

拒绝文案点名具体义务（`R2 (the boundary rule is covered by a test)`），
模型可以据此把活干完再提交。

## 测试锁定

| 层 | 内容 |
| --- | --- |
| lifecycle ×8 | Pending/Blocked 不放行；Behavior 语义证据可放行；Verification 仅 prose 不放行；新鲜绿检查可放行；被作废的绿检查不放行；Deliverable 需 mutation；空契约自陈"什么都没核对" |
| agent 派生 ×4 | 派生出 kind 正确的义务、全部 Pending/OriginalGoal/有 id；畸形回复 → UNAVAILABLE；空 requirements → UNAVAILABLE；请求用 bounded profile 且只有一次调用 |
| agent e2e ×4 | 仅 prose 支撑的测试义务拒绝完成且点名 R2；机械支撑的义务可完成；无契约不得完成；重启后义务存活并继续拦截 |

## 首轮付费门结果：INSUFFICIENT（成因是终局映射，不是契约）

冻结二进制 `7fbefd6b979f`，Dogfood Lab，串行。

### Gate 1 — HC-002 ×3：通过

| 指标 | 值 |
| --- | --- |
| Completed / 隐藏验收 / rc0 | 3 / 3 / 3 |
| 假阴性、correct-work-underclaimed | 0、0 |
| 契约抽取失败 | 0（3 次派生全成功，义务数 16 / 9 / 9） |
| 完成尝试 / 被契约退回 | 4 / 2，退回后模型把活干完再提交 |
| 花费 | $0.352 |

### Gate 2 — scale-s800：跑到第 3 次停止，出现 1 次假完成

| 跑 | 终局路径 | 外部验收 |
| --- | --- | --- |
| #1 | **CloseoutForced** | ✓ |
| #2 | Completed（经过契约门） | ✓ |
| #3 | **CloseoutForced** | ✗ **假完成** |

### 契约本身是有效的

三次跑里 `verification_requirements=2` 全部捕获到"边界规则要被测试覆盖"那条义务；
机械下限按设计否决了只有 prose 支撑的声称（#1 退回 1 条、#3 退回 2 条，理由逐条写明
"no check demonstrates it over the current tree"）。#3 的两次拒绝都正确。

### 真正的洞：一条不经过契约的终局路径

`#3` 最终不是"通过完成门"，而是**跑满 closeout 预算被强制收尾**（`stop_reason=CloseoutForced`），
而 harness 把"强制收尾 + 工作区验证通过"映射为 `outcome=verified`，eval 据此记为完成。
这条路径**从不查契约**：事件流里 `goal_intercepted` 两条都在，义务 R3/R4 明确还欠着，
工作区最终只改了 `internal/window/window.go`，测试没留下。

这违反 §35「不存在任何 requirement 未满足却能 Completed 的路由」。

**它不是本次引入的**——强制收尾映射为 verified 是既有行为。是契约第一次让"收尾那一刻还欠什么"
成为可查事实，才把它暴露出来。同一模式在 6 次跑里出现 3 次（s800 #1/#3、HC-002 #2），
所以"Gate 1 通过"这个结论也要打折：那 3/3 里有 1 次并非经由契约门放行，只是外部验收恰好通过。

### 建议的修法（未实施，等裁决）

强制收尾时若尚有 material 义务未清，终局不得记为 verified/completed——应为 Incomplete 或
Blocked。这是终局映射的一处收口，不是新架构。

```
EXPLICIT_COMPLETION_CONTRACT_V1=INSUFFICIENT
CONTRACT_MECHANISM=WORKING
FALSE_COMPLETION_ROUTE=CLOSEOUT_FORCED_TERMINAL_MAPPING
```

## 尚未实现（不假装做了）

- §27–§29 契约 revision 与"用户显式改需求"路径
- §32/§82 把既有 reconciliation 收窄成"遗漏/矛盾"专用二级守卫（当前仍做全量评估 + 逐条交账）
- §53 `source_text` 对原文的溯源校验
- §55/§93 契约可观测指标（CONTRACT_* 系列）
- §65 抽取质量 fixtures A–F
- §87–§92 付费门（HC-002 ×3 / scale-s800 ×10 / icg-6r ×10 / yq-doc-count / affected lanes）

## 已知风险

判定模型若忽略 schema 不交账，所有义务停留 Pending → 全部 fail-closed。
这是 §99/§122 假阴性门要测的真实风险，不是理论问题。
