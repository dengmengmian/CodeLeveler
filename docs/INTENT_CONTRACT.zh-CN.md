# 用户意图契约

[English](INTENT_CONTRACT.md) · [文档索引](README.zh-CN.md)

你说「继续，第三阶段做完就停」，当前模型要从这句话里读出两件事：继续当前 Goal，并在第三阶段完成后暂停。模型把这两项操作连同原话交给运行时。运行时核对原话、目标和步骤，再记录结果。同一流程也适用于其他语言。

## 归属

| 谁决定什么 | 负责方 |
| --- | --- |
| 用户想干什么（继续、暂停、补充、取消、提问、新任务） | 模型，在本轮内声明 |
| 声明是否引用了用户自己的话 | Runtime（`quote_is_in_input` / `resolve_source_span`） |
| 它作用于哪个 Goal | Runtime（本轮绑定的 Goal、显式 id、或唯一运行中的 Goal） |
| 转换是否合法、顺序是否正确 | `leveler_lifecycle::project_intent` |
| 暂停边界指向哪个计划步骤 | 模型自己声明的计划，用稳定 step id |
| 持久结果 | `leveler-storage`（`goals` 与有序 `goal_intents` 日志） |
| 客户端显示什么 | TUI / Web / CLI 投射 Runtime 事实 |

运行时只采纳通过校验、能对应到用户原话的声明。这个记录只说明用户授权了什么。

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

`source_span` 指向最新用户消息里的起止行，从第 1 行开始数，两端都包含。按 `\n` 分行，最后一个换行不另算一行。运行时从原消息取出这些行，逐字存为 `source_quote`。

两种引用方式只能选一种。直接填引文时须保留原话。填行号时传 `source_quote: null`，运行时将 `null` 当作未填。Schema 把这个字段放在 `required` 里，是因为一些服务商只遵循这份必填列表。

操作集：`continue_task`、`pause_task`、`set_pause_boundary`、
`replace_pause_boundary`、`amend_current_task`、`start_independent_task`、
`cancel_task`、`answer_only`。一次声明里的操作按数组顺序应用。

## 校验

1. **来源。** `source_quote` 必须非空，且出现在 transcript 中最新的用户输入
   里（精确匹配，或折叠连续空白后匹配）；`source_span` 会先从该输入中切出
   原文。如果之后发了新消息，不能再用旧消息作为这次变更的授权。
2. **身份。** 目标依次是：本轮绑定的 Goal、显式 `goal_id`、任务内唯一运行中
   的 Goal。没有运行中的 Goal、或多个 Goal 且未指明，都拒绝并在错误中列出
   候选 id。Runtime 不猜用户指的是哪个 Goal。
3. **边界。** `after_step` 指向模型自己声明的计划步骤 id（`update_plan`
   带稳定 id）。没有模型计划、或 id 不存在，都拒绝并给出修正方式。重命名
   步骤时，只要沿用原 ID，暂停位置就不变。步骤名只用于显示。
4. **顺序与终结操作。** `cancel_task`、`answer_only`、
   `start_independent_task` 必须单独出现在声明中；其余操作可组合。
5. **状态。** 已结算的 Goal 只接受取消确认；已暂停的 Goal 只接受
   `continue_task`；只有步骤带有显式 `completed` 状态时，边界才算「已到达」。
   删掉或换掉 ID，或把步骤拆分、合并，都不会自动完成原步骤。计划调整后，模型可以为新的步骤 ID 重新声明暂停边界。

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

计划随 Goal 保存。显式恢复或继续一个正在运行的 Goal 时，运行时会先把原计划交给模型，让它看见哪些步骤已完成。新 Goal 从自己的计划开始。

## 持久历史

`goal_intents`（migration 0053、0054）按 Goal 记录单调递增的 `seq`、原始
用户消息所在的 `turn_id`、逐字 `source_quote`、按顺序采纳的操作、生效后的
暂停状态与边界、时间戳。重启后可以查出哪一轮授权了什么、原话是什么。当前状态以 `goals` 表的记录为准。

## 不在本契约内

- 没有关键词表、语言识别或每条消息的额外模型调用。
- 没有客户端续接路由：客户端原样提交用户文本，只投射 Runtime 发布的事实。
- 没有模型自证完成：意图只回答授权。
