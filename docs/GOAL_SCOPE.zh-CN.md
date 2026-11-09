# Goal 范围契约

一个 Goal 的需求如何被记录、修改和执行。一项需求是**锚定在用户消息原文片段上的
模型声明**：Runtime 校验身份、来源、转换合法性和顺序，绝不从自然语言里重新推导
含义。中文、英文、日文或混合表达走同一条路径，因为这里没有词表。

## 归属

| 决策 | Owner |
| --- | --- |
| 用户要求了什么（交付物、约束、禁止项） | 模型，阅读用户消息后声明 |
| 引用的片段是否真的出现在最新用户输入里 | Runtime（`quote_is_in_input`） |
| 需求属于哪个 Goal | Runtime（本轮绑定的 Goal） |
| 需求身份（`R1`、`R2`…)、状态与顺序 | `leveler_lifecycle::apply_scope_declaration` |
| 持久结果 | `leveler-storage`（有序 `goal_requirement_events`，迁移 0055） |
| 已记账的需求是否被机械证据满足 | `leveler-agent::executor::scope_gate` |
| 客户端显示什么 | TUI / Web / CLI 投射 Runtime 事实 |

声明是模型输出：它不能创造 quote 之外的任何用户授权，也不构成工作已完成的证据。
quote 只证明来源存在，不证明模型的解释正确；原始用户消息保留在 transcript 中，
后来的读者可以同时审计两者。

## 需求模型

每项有效需求包含：

| 字段 | 含义 |
| --- | --- |
| `id` | 稳定的 Runtime 身份（`R1`、`R2`…）。replace 之后旧 id 不再复用。 |
| `kind` | 决定该项需要什么证据的种类（见下）。 |
| `text` | 模型陈述的需求本身。 |
| `target` | 路径、文本位置或精确命令；需要目标的种类必填。 |
| `source_quote` | 声明所锚定的用户消息原文片段。 |
| `check` | 模型为该项关联的可选验收检查。 |
| `state` | `active`、`waived` 或 `superseded`。 |
| `superseded_by` | replace 退休旧项后，指向替代它的新 id。 |

种类：`artifact`、`artifact_absent`、`command`、`named`、`content`、`tests`、
`forbidden`、`baseline`、`preparation`、`constraint`。交付类是 Gate 会追究的
种类；`forbidden`、`baseline`、`preparation`、`constraint` 是模型必须遵守、
但不能被「完成」的上下文。`artifact`、`artifact_absent`、`content`、`command`
和 `tests` 必须提供 `target`；`tests` 的 `target` 是作为其证据的精确测试命令。
`content` 的 `text` 是 Gate 与 `target` 逐行比对的字面内容本身；若写成对该行的
描述，Gate 会拒绝并要求模型 amend `text`，而不是继续改文件。

## 声明方式

两个结构化入口，都由模型调用：

- `commit_task(requirements=[{kind, text, target, source_quote, check}])`：Chat
  轮注册任务时声明初始集合，按数组顺序应用，生成 `R1..Rn`。
- `declare_scope({operations: [...]})`：修改已绑定 Goal 的集合。`/goal` 轮、
  恢复的 Goal、已提交任务的补充需求都使用同一套操作。

```json
{
  "operations": [
    {"op": "add", "kind": "artifact", "text": "stage-2.txt 存在",
     "target": "stage-2.txt", "source_quote": "创建 stage-2.txt"},
    {"op": "waive", "requirement_id": "R3",
     "source_quote": "这一步先不做了", "reason": "用户暂缓"}
  ]
}
```

操作集：`add`、`amend`、`replace`、`waive`、`restore`。没有 delete：`waive`
停止追究但保留历史，`replace` 让旧 id 进入 superseded 并生成承载替代内容的新
id，`restore` 恢复已 waived 的需求。声明按数组顺序应用，复合变更是一次有序
事件序列，而不是几次独立写入。

`get_goal_scope` 读取当前集合（id、种类、状态、quote、check），并可按需读取
带授权轮次的有序变更历史。

## 校验

1. **Schema 与边界。** 未知种类 / 操作拒绝；text、quote、target、reason 都有
   明确长度上限；单次声明最多 `SCOPE_OPERATIONS_MAX` 个操作，单个 Goal 最多
   `SCOPE_REQUIREMENTS_MAX` 项有效需求。
2. **来源。** 每个操作的 `source_quote` 必须出现在 transcript 中最新的用户输入
   里（精确匹配，或折叠连续空白后匹配）。旧消息不能授权一个可能已被新消息反转
   的变更。
3. **身份。** 目标只能是本轮绑定的 Goal。操作未知 id 或非 active 的需求会被
   拒绝，并给出当前状态。
4. **转换合法性与顺序。** `amend`、`waive` 要求 active；`restore` 要求 waived；
   `replace` 让 active 需求进入 superseded 并生成替代项。事件带每 Goal 单调
   递增的 `seq`，拒绝不会改变已记录的集合。

## 持久历史

`goal_requirement_events` 按 Goal 保存：`seq` 顺序、声明所在 `turn_id`、操作、
需求身份、kind/text/target、用户原文 `source_quote`、check、reason 与时间戳。
当前集合是这段历史折叠出来的结果——不存在第二份可变副本，因此恢复、上下文折叠
或崩溃都无法缩小范围。`record_scope` 在同一事务里折叠历史、应用并校验声明、
追加新事件。

## 完成 Gate

`update_goal(status="complete")` 必须按稳定 id 为每个 active 交付项记账
（`requirements=[{id, status, evidence}]`）：

- `not_done` 拒绝完成；缺项拒绝完成；
- `unverifiable` 是明确的未验证边界，不是对 Runtime 可观察事实的豁免；
- `artifact` / `artifact_absent` 对照当前工作区与记录的变更；
- `content` 按目标文件的一行检查；
- `command` 必须匹配最后一次变更之后的成功运行记录，evidence 可写
  `call_id:<id>` 或命令行；
- `tests` 在 `target` 中声明精确的测试命令；该命令在最后一次变更之后的通过
  运行记录可以记账。Runtime 没有结构化测试结果，因此只有命令成功时，报告为
  **未验证的测试覆盖**，并在记录中说明这一局限：一个没有运行任何测试却退出 0
  的命令不能声称测试覆盖。Runtime 不认识任何 runner 名称或输出格式。历史上
  未带 `target` 的 `tests` 需求，按同一显式未验证边界接受，而不是阻塞一个
  合法记录的 Goal；
- `named` 接受，但附带「无机械检查」的显式警告。

Gate 在每次完成声明时读取持久集合，因此被折叠的 transcript 或重启的进程都
无法缩小范围。模型完全不声明时 Gate 没有可执行集合；所以 Goal mode 在契约里
写明声明义务，`get_goal_scope` 始终展示 Gate 将读取的精确集合。没有应交付项
时，没有任何需求级事实可供验证，因此这种完成永远不会报告为 `Verified`：
引用也许能证明某个命令运行过，但没有检查任何已声明的交付物。

## 验证报告

完成状态由两层组成，它们是不同性质的事实：

| 层 | 来源 | 证明什么 |
| --- | --- | --- |
| `update_goal(verification)` | 模型声明 | 模型声称自己运行了哪些执行 |
| `VerificationRun` | Runtime 执行记录（`ToolCallStarted` + `ToolCallFinished`、后台完成） | 命令、call id、退出状态、工作区版本与所在模型步 |

Gate 用执行记录解析每个引用。引用可以写 `call_id`、精确命令行，或命令行加
尾部备注；失败的运行、早于最后一次变更的运行、以及被同一命令更晚一次运行推翻
的引用，都会按真实原因拒绝。命令「是不是测试 runner」永远不会由名称或输出
决定。

被接受的完成会在任务终态持久化一份 `VerificationReport`
（`TaskFinished.verification`，`TaskReport` 上也是同一份）：

```json
{
  "status": "unverified",
  "records": [
    {"requirement_id": "R2", "kind": "tests", "state": "unverified",
     "detail": "`make regression` exited 0 at workspace version 4 (step 7), but the runtime observed no structured test result: command success does not establish that any test executed, so the test coverage remains unverified",
     "call_id": "call-12", "command": "make regression", "exit_code": 0,
     "workspace_version": 4, "model_step": 7}
  ]
}
```

`records` 按需求顺序、每个应交付项一条。只有真实执行记录支撑时才会出现证据
字段；否则 `call_id`/`command`/`exit_code` 缺省，`detail` 说明 Runtime 无法
检查什么。`workspace_version` 是判定针对的变更计数器，因此无需重放 transcript
就能看出结论是否过期。`Verified` 要求至少有一条应交付项记录，且每条记录都是
`verified`；部分或空记录集一律是 `unverified`。

重放是增量的。2026-09-23 之前的终态把已移除的宿主验证结论写成同一个字段里的
裸字符串（`"verification": "not_run"`）；它不是报告，读作「没有报告」，旧会话
仍可重放。结构化对象按严格模式解析：损坏的报告仍会让整次重放失败，而不是静默
变成「没有报告」。

四种用户可见结果是对这两个事实的投射，而不是另算一套状态：

| 结果 | 表示 |
| --- | --- |
| Verified | `TaskOutcome::Completed` + `VerificationStatus::Verified` |
| CompletedUnverified | `TaskOutcome::Completed` + `VerificationStatus::Unverified` |
| Incomplete | 被接受轮次上的非交付 `StopReason`（预算、中断） |
| Blocked | `TaskOutcome::Blocked` |

未验证的完成照常接受并交付，不会停下来等待用户确认：警告与报告告诉用户
Runtime 证明了什么、没能证明什么，由用户决定后续处理。

## 不属于本契约

- 没有关键词、否定词、章节标题、测试 runner 或状态词表。
- 不从自然语言抽取需求——提交时、恢复时、折叠后都不抽取。
- 不为分类用户消息增加第二次模型调用；正在执行的轮次自己阅读并声明。
- 记录需求不需要人工审批；用户的 quote 是来源，Runtime 只负责身份与持久性。
