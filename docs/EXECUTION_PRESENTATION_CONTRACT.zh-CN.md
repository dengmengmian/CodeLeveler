# Execution Presentation Contract v1

本文冻结 CodeLeveler 的**执行展示语义**，使 TUI / Web / Desktop / App 对同一份
runtime facts 得到同一棵语义树。它不是视觉规范：颜色、间距、字形、边框、动画由各
surface 自己决定。

> 共享语义，不共享像素。
>
> Share semantics, never pixels.

- 状态：**FROZEN**（v1）
- 权威实现参考：`crates/leveler-tui`（本 contract 由它的真实行为冻结）
- 可执行契约：`testdata/execution_presentation/v1/*.json` + 各 surface 的 conformance 测试
- 英文版：[`EXECUTION_PRESENTATION_CONTRACT.md`](EXECUTION_PRESENTATION_CONTRACT.md)

架构总纲仍以 [`ARCHITECTURE.zh-CN.md`](ARCHITECTURE.zh-CN.md) 为准；本文若与其冲突，
以架构文档为准并修正本文。

---

## 1. 目的与范围

目标：

1. 冻结当前已经验证过的执行展示语义。
2. 让四个 surface 对同一份 runtime facts 得到一致的 `AssistantText`、`ExecutionRound`、
   tool lifecycle/status、failure truth、`FinalAnswer`。
3. 各 renderer 保留自己的视觉样式，但**不得各自猜语义**。

范围（本轮只做这些）：

- 执行展示（conversation transcript 的执行部分）。
- 跨端语义一致性与 conformance。

不在范围内：

- Task Context / Plan / Progress 独立 surface（Contract v2）。
- Thought / raw reasoning disclosure UI。
- Stage、自动 narration、历史 Round 自动折叠或降权。
- 新配色、新紧凑 header 文案、TUI 重做。

---

## 2. 语义树

```text
Turn
├─ AssistantText?            模型公开 content
├─ ExecutionRound*           model_step 的真实边界
│  ├─ Run                   串行的单次/单类工作
│  ├─ Batch                 可观测的并发突发
│  └─ ToolRow               单次工具调用及其真实状态
├─ FinalAnswer?             本回合被 commit 的回答
└─ TurnEnd?                 回合终态
```

`ExecutionRound` 就是真实 `model_step`：

```text
different model_step              -> different ExecutionRound
same model_step parallel batch    -> same ExecutionRound
```

禁止把下列东西当作 Round 边界：

- Stage / 调查阶段 / 修复阶段 / 验证阶段；
- 时间窗口；
- tool activity class（工具类型）；
- assistant narration 边界；
- 前端自行推断出的任何分组。

`ExecutionRound` 是否为「Stage」由模型语义决定，Harness 不推断。

---

## 3. 冻结 invariant

编号与 fixture 对应；每条都能由 `testdata/execution_presentation/v1/C*.json` 与各
surface 的 conformance 测试机械验证。

### I1 `model_step` 决定 Round（C1）

不同 `model_step` → 不同 `ExecutionRound`。没有 assistant prose 时，
Round 1 / Round 2 / Round 3 / FinalAnswer 连续出现是合法且正常的。

### I2 同步 batch 属于同一 Round（C2）

同一 `model_step` 的并发行，无论 `parallel` 与否，都属于同一 Round。

### I3 Run / Batch 是 Round 内的聚合（C3）

`batch` 身份来自**观测**：一个调用在另一个仍 Running 时开始，二者才共享一个 batch。
时间接近但不是并发，绝不共享 batch。batch 不是新的 Round。

### I4 公开文本逐字保留（C4）

`AssistantText` 只来自模型的公开 `content`（`assistant_text_delta` /
`assistant_message_completed`）。文本必须逐字保留，不做改写、截断或合成。

### I5 raw reasoning 不进 transcript（C5）

`reasoning_delta` / CoT 不进入 conversation transcript，也不作为
`AssistantText` / `FinalAnswer`。允许 live status 表达 `Thinking` / `思考中` /
`Waiting for model`；**不得展示 raw reasoning body**。不得按 model name 分支。

### I6 不按工具类型拆 Round（C6）

同一真实 `model_step` 内的 read / search / command 属于同一 Round。

### I7 status truth（C7）

Tool lifecycle 状态集合固定为：

```text
Running | Ok | Failed | Cancelled | Unknown
```

- `Cancelled` = 停止且 runtime 确认进程树已消失（`stop = confirmed`）。
- `Unknown` = 未收到终态，或停止无法确认（`stop = unconfirmed`）。
- **`all_ok` 仅当 EVERY visible ToolCall == Ok 时成立。**
  `Ok + Failed`、`Ok + Cancelled`、`Ok + Unknown`、`Cancelled + Cancelled` 均不是
  全部成功。
- 零失败 ≠ 全部成功：cancelled / unknown 什么都没有失败，但也没有成功。

### I8 failure truth（C8）

UI 必须表达真正的 failure attribution。

`runtime metadata / execution policy note`（`[execution policy]`、`[mutation rejected]`、
`[note]`、`exit: N`、`--- stream ---`、`[timed out after Ns]`）说的是命令**如何**运行，
不能作为命令**为什么**失败的原因。优先级：

```text
真实 stderr / structured failure  >  runtime metadata
```

Tool failed ≠ Tool implementation bug。UI 不得把下列情况统一写成「工具错误」：

- command exit non-zero；
- environment missing；
- permission denied；
- model bad args；
- background child failure。

Runtime 继续写这些 notes；这是展示层解释规则，不是执行语义变更。

### I9 FinalAnswer 不被 bookkeeping 覆盖（C9）

Assistant message 的 Final/Progress 分类由**事件顺序**决定，不读文案：

- 一条消息之后出现了*工作*调用 → 它是 Progress（interim narration）；
- 回合在一条 pending 消息上终态（Completed / Answered）→ 它是 FinalAnswer；
- 只有 `update_plan` / `update_goal` / 静默观测类 bookkeeping 跟在后面，
  **不降级**已 commit 的 FinalAnswer；
- 真实继续执行 read / search / edit / shell → 已提交的回答降级为 Progress
  （沿用当前已验证逻辑）。

若 Completed / Answered 回合没有任何 commit 的 FinalAnswer，终态必须表达为
`no_final_answer`，**不得**显示绿色「任务已完成」。工具跑完不等于任务做完。

### I10 live / reconnect / replay 语义一致（C10）

同一 runtime fact sequence，经：

```text
live
reconnect snapshot
replay (durable history)
resume
```

必须投影为同样的语义结构。视觉允许不同，语义不允许漂移。

锁定的关键字段：

- `UiActiveToolCall.model_step` —— reconnect 必须恢复，不得重新推断 Round；
- reconnect snapshot 与 replay 的 Round identity 一致；
- replay 不得铸造 orphan Round，也不得重复回答。

### I11 legacy round identity（C11）

`model_step = None`（旧 session / 旧协议）继续走既有兼容 fallback，不 crash、不破坏
旧 session、不发明 Round。不得为旧协议增加新的 heuristic。

### I12 BTW isolation（C12）

`/btw` side question 必须是 side surface：

- 不进入主 `ExecutionRound`；
- 不改主 transcript authority；
- 不改 `FinalAnswer`；
- 不改 Goal / Plan 主状态；
- 不混进主 replay。

### I13 runtime injection ≠ user message（C13）

`ProtocolRepair` / runtime closeout：

- 可以继续进入模型上下文；
- **不是** user authored transcript；
- resume 后不得重新显示成用户消息。

判定必须使用结构化 origin / event semantics，**不得**用英文字符串匹配过滤。

Runtime notice（`kind = runtime_notice`）是 runtime 写进模型上下文的 user-role 消息，
在 UI 里是 note，不是用户发言。

---

## 4. Shared Projection 决策

**结论：本轮不新增 shared production projection type。**

理由：

1. **wire 已经携带全部事实。** `RuntimeEvent` 携带 `model_step`、`parallel`、
   `ok`、`stop`、`exit_code`、`preview`、`applied_diff`、
   `UiMessage.kind`/`ordinal`/`images`；`UiSessionSnapshot.active_tools` 携带
   `model_step`。协议没有缺口。
2. **没有 Rust 侧第二消费者。** Web/Desktop/App 分别是 TypeScript / JavaScript /
   Dart。新增一个 Rust 生产类型对它们不可用，只会多一份没有消费者的抽象，
   违反反过度设计。
3. **语义树本身就是投影规则。** 它是一个**纯函数**：`runtime facts -> semantic tree`。
   把它冻结为规格 + 语言无关的 fixture corpus，并在每个 surface 上用 conformance
   测试锁定，比新增跨语言抽象层更直接。

因此一致性由以下三者共同保证：

```text
Runtime facts (wire)
      ↓  冻结的纯函数（本文 §2-§3）
Semantic tree
      ↓  各自实现 + 同一份 fixtures
TUI / Web / Desktop / App conformance tests
```

**已知架构债（NON-BLOCKER，需在 v2 处理）：** FinalAnswer 的
work / bookkeeping 分类目前 owner 在 `leveler-tui::tool_taxonomy`（`acts_on_answer` +
`ActivityVisibility`）。跨端一致性要求这个分类有唯一 owner。v1 的处理方式：

- 本文冻结分类**结果**；
- fixture corpus 覆盖它（C4 / C5 / C9）；
- 每个 surface 必须实现同一分类；
- 若未来出现第三个消费者，应把它提升为共享 owner（runtime 标注或共享 crate），
  而不是继续复制。

---

## 5. Fixture corpus 与 conformance

fixture 目录：`testdata/execution_presentation/v1/`。

每份 fixture：

```json
{
  "id": "C4",
  "title": "assistant text plus tools",
  "invariant": "…",
  "paths": { "<name>": [ step, ... ] },
  "rendered": { "contains": [...], "excludes": [...] },
  "expect": { "items": [...], "user_texts": [...], "reasoning_visible": false }
}
```

- `paths` 是**真实 wire facts**，step 形态为 `{"event": …}`、
  `{"snapshot": …}` 或 `{"history": [UiHistoryEntry, …]}`。
- 若有多条 path，它们**必须投影出同一棵 `expect`**（C10 / C12 就是靠这个锁住的）。
- `expect` 是结构化语义树，不是截图。
- `rendered` 只在语义树无法承载的场合使用（例如 C8 的 failure attribution）。

参考实现（TUI）的 conformance：

```sh
cargo test -p leveler-tui --test execution_presentation_contract
```

其中 `UPDATE_EXECUTION_PRESENTATION_FIXTURES=1` 可以从参考实现重新生成 `expect`。
生成器记录产品当前行为，不决定产品应当如何行为；每次重生成都必须人工 review。

各 surface 的 conformance 测试读取**同一份 JSON**，把自己的投影映射到同一棵语义树
再比对。允许在测试内做「surface 类型 → 语义树」的适配；不允许在适配里放宽 status、
round identity 或 failure attribution。

---

## 6. 各 surface 义务

| Surface | 语义树来源 | 允许不同 | 必须一致 |
| --- | --- | --- | --- |
| TUI | 参考实现 | 字形、颜色、布局 | 全部 invariant |
| Web | wire facts → 自己的投影 | card / collapse / icon / hover | Round identity、status、failure、FinalAnswer、reasoning hidden |
| Desktop | wire facts → 自己的投影 | compact list / disclosure / 原生窗口 | 同上；Renderer 只展示 |
| App | wire facts → 自己的投影 | 移动端行样式、手势 | 同上；reconnect 不得猜 Round |

边界：

- Renderer 只展示。`ExecutionRound` grouping、failure attribution、final-answer
  semantics、`model_step` inference **不得**进入 Electron Main，也不得进入 Desktop
  bridge。
- 不得把 TUI widget type 当作共享领域模型。
- 不得把 ANSI / terminal 概念放进 protocol。
- 不得把 CSS / UI label 放进 Rust runtime。
- 不得把 presentation strings 当作 protocol contract。
- collapse 是视觉行为，不能改变 underlying semantic tree。
- 某 surface 功能缺失时可以标 `Deferred` / `Unsupported`，但**不得**用错误语义假装支持。

---

## 7. Conformance matrix（v1 验收）

CI / Gate 每次必须给出如下矩阵；每格只能是 `PASS` / `DEFERRED` / `N/A` / `FAIL`。
`PASS` 必须有 fixture 或真实证据。

| # | invariant | TUI | Web | Desktop | App |
| --- | --- | --- | --- | --- | --- |
| C1 | multi-round | PASS | PASS | DEFERRED | DEFERRED |
| C2 | parallel batch | PASS | PASS | DEFERRED | DEFERRED |
| C3 | run/batch | PASS | PASS | DEFERRED | DEFERRED |
| C4 | public text | PASS | PASS | PASS | PASS |
| C5 | tool-only | PASS | PASS | PASS | PASS |
| C6 | mixed kinds | PASS | PASS | DEFERRED | DEFERRED |
| C7 | status truth | PASS | PASS | PASS | PASS |
| C8 | failure truth | PASS | PASS | PARTIAL | DEFERRED |
| C9 | final bookkeeping | PASS | PASS | PASS | PASS |
| C10 | reconnect/replay | PASS | PASS | PASS | DEFERRED |
| C11 | legacy | PASS | PASS | PASS | PASS |
| C12 | BTW | PASS | PASS | PASS | PASS |
| C13 | runtime injection | PASS | PASS | PASS | PASS |

`PARTIAL` 是显式允许的中间态，只用于「语义已正确但覆盖不完整」；不得用来描述
「看起来没问题」。

---

## 8. 演进

- v1 只统一执行展示。Task Context / Plan / Progress 属于 v2。
- v1 之后各 surface 可以各自做视觉打磨；只要不改语义树，不需要重新冻结 contract。
- 任何改变 §2-§3 的改动都是 contract 变更，必须同时：
  1. 更新本文与英文版；
  2. 更新 fixture corpus；
  3. 更新所有受影响 surface 的 conformance 测试；
  4. 在 dogfood 中更新 `execution_presentation_v1` contract。
