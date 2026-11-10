# 用户意图契约

一条用户消息如何改变 Goal 的执行授权。契约的目的：让「继续」「go ahead」
「続けて」「等一下，先别做阶段 2」都由**正在执行本轮的模型**读懂，而不是由
关键词表、额外的分类模型调用或客户端路由来决定。

## 归属

| 决策 | Owner |
| --- | --- |
| 用户想干什么（继续、暂停、补充、取消、提问、新任务） | 模型，在本轮内声明 |
| 声明是否引用了用户自己的话 | Runtime（`quote_is_in_input` / `resolve_source_span`） |
| 它作用于哪个 Goal | Runtime（本轮绑定的 Goal、显式 id、或唯一运行中的 Goal） |
| 转换是否合法、顺序是否正确 | `leveler_lifecycle::project_intent` |
| 暂停边界指向哪个计划步骤 | 模型自己声明的计划，用稳定 step id |
| 持久结果 | `leveler-storage`（`goals` 与有序 `goal_intents` 日志） |
| 客户端显示什么 | TUI / Web / CLI 投射 Runtime 事实 |

声明是模型输出：它不能创造用户没有说过的授权，也不构成工作已完成的证据。
只有 quote 把它与用户绑定；只有 Runtime 校验通过才被采纳。

## 声明方式

模型调用 `declare_intent` 工具（有 Goal 生命周期的第 0 层可写轮次都会提供，
包括 Chat 轮）：

```json
{
  "source_quote": "继续执行，但第三阶段完成后暂停。",
  "goal_id": "…目标唯一时可省略…",
  "operations": [
    {"kind": "continue_task"},
    {"kind": "set_pause_boundary",
     "boundary": {"kind": "after_step", "step": {"step_id": "stage-3"}}}
  ]
}
```

`source_quote` 可以换成 `source_span`：

```json
{
  "source_span": {"start_line": 2, "end_line": 2},
  "operations": [{"kind": "continue_task"}]
}
```

span 是最新用户消息的 1-based 闭区间行号（`\n` 分行，末尾一个换行不额外产生
一行）。Runtime 从消息里切出原始字节，并存为持久 `source_quote`。模型因此只
指位置、不必复制标点；因为切的是原文，严格性不变。两种形式必须且只能给一个，
改写仍然不被接受；同时给出会被拒绝，而不是按偏好取一个。工具 schema 把
`source_quote` 列在 `required`（可为 null），因为部分 provider 只遵循 required
列表；仅用 span 的声明传 `source_quote: null`，Runtime 将其视为缺失，而不是第二
种来源。

操作集：`continue_task`、`pause_task`、`set_pause_boundary`、
`replace_pause_boundary`、`amend_current_task`、`start_independent_task`、
`cancel_task`、`answer_only`。按数组顺序应用，因此复合指令是一次声明，
不是两个彼此独立的决定。

## 校验

1. **来源。** `source_quote` 必须非空，且出现在 transcript 中最新的用户输入
   里（精确匹配，或折叠连续空白后匹配）；`source_span` 会先从该输入中切出
   原文。引用更早的消息会被拒绝：一条可能已被更新消息反转的授权不能复用。
2. **身份。** 目标依次是：本轮绑定的 Goal、显式 `goal_id`、任务内唯一运行中
   的 Goal。没有运行中的 Goal、或多个 Goal 且未指明，都拒绝并在错误中列出
   候选 id。Runtime 不猜用户指的是哪个 Goal。
3. **边界。** `after_step` 指向模型自己声明的计划步骤 id（`update_plan`
   带稳定 id）。没有模型计划、或 id 不存在，都拒绝并给出修正方式。重命名
   步骤不会移动边界；label 只用于显示。
4. **顺序与终结操作。** `cancel_task`、`answer_only`、
   `start_independent_task` 必须单独出现在声明中；其余操作可组合。
5. **状态。** 已结算的 Goal 只接受取消确认；已暂停的 Goal 只接受
   `continue_task`；只有步骤带有显式 `completed` 状态时，边界才算「已到达」。
   被删除、改名但未保留 id、被替换、拆分或合并的步骤仍然未完成——修改计划
   不等于完成工作。模型可以自由调整计划，并在阶段真的移动时重新声明边界。

## 状态写入方

`user_paused_at` / `pause_boundary` 每个方向只有一个写入路径：

- 校验通过的声明（`continue_task` 清除；边界已到达时设置）与有序日志在
  同一次写入中记录；
- 因已到达的声明边界而收束的窗口设置暂停（永不主动清除暂停）；
- 结算 Goal 时在同一次写入中清除两者。

读自然语言的代码不能设置或清除暂停。状态提问用 `answer_only` 回答而不
驱动 Goal；补充用 `amend_current_task` 记录而不释放暂停；无关任务必须用
`task_relation: "new"` 显式 commit。

## 计划连续性

计划属于声明它的 Goal。继续同一 Goal 的轮次——显式 resume，或续接运行中
Goal 的 Goal 轮——在模型行动前播种原计划，使新窗口看到已经完成的阶段。
全新 Goal 不播种计划。

## 持久历史

`goal_intents`（migration 0053、0054）按 Goal 记录单调递增的 `seq`、原始
用户消息所在的 `turn_id`、逐字 `source_quote`、按顺序采纳的操作、生效后的
暂停状态与边界、时间戳。重启后日志能回答「谁、用什么话、以什么顺序授权
了什么」；Goal 行仍是当前事实的权威源。

## 不在本契约内

- 没有关键词表、语言识别或每条消息的额外模型调用。
- 没有客户端续接路由：客户端原样提交用户文本，只投射 Runtime 发布的事实。
- 没有模型自证完成：意图只回答授权。
