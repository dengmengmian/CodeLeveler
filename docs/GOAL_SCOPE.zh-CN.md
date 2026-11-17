# Goal 范围契约

[English](GOAL_SCOPE.md) · [文档索引](README.zh-CN.md)

例如，你要求「创建 `stage-2.txt`」后又说「这一步先不做了」。模型先登记交付项，再按你的新指示把它标为暂免。两次都要引用用户原话。运行时校验引用、目标和操作是否合法，按顺序保存变更，完成 Goal 时按最新的需求清单检查。不同语言走同一套流程。

## 归属

| 谁决定什么 | 负责方 |
| --- | --- |
| 用户要求了什么（交付物、约束、禁止项） | 模型，阅读用户消息后声明 |
| 引用的 quote 或 span 是否解析出真实用户原文 | Runtime（`quote_is_in_input` / `resolve_source_span`） |
| 需求属于哪个 Goal | Runtime（本轮绑定的 Goal） |
| 需求身份（`R1`、`R2`…)、状态与顺序 | `leveler_lifecycle::apply_scope_declaration` |
| 持久结果 | `leveler-storage`（有序 `goal_requirement_events`） |
| 已记账的需求是否被机械证据满足 | `leveler-agent::executor::scope_gate` |
| 客户端显示什么 | TUI / Web / CLI 投射 Runtime 事实 |

引文能证明用户说过这句话。想核对模型是否理解正确，可以对照保存的声明和会话里的原消息。交付情况由下面的完成检查另行记录。

## 需求模型

每项有效需求包含：

| 字段 | 含义 |
| --- | --- |
| `id` | 需求编号，例如 `R1`、`R2`。替换后旧编号不再分配。 |
| `kind` | 决定该项需要什么证据的种类（见下）。 |
| `text` | 模型陈述的需求本身。 |
| `target` | 路径、文本位置或精确命令；需要目标的种类必填。 |
| `source_quote` | 这项需求引用的用户原话。 |
| `source_span` | 仅声明时可用的 `source_quote` 替代：1-based 闭区间行号。Runtime 从权威消息中切出原文并存为 `source_quote`，不保留坐标。 |
| `check` | 可选的验收计划说明。 |
| `verification` | `named` 交付项可登记的命令断言。一旦登记，必须通过才能结算。 |
| `state` | `active`、`waived` 或 `superseded`。 |
| `superseded_by` | replace 退休旧项后，指向替代它的新 id。 |

种类：`artifact`、`artifact_absent`、`command`、`named`、`content`、`tests`、
`forbidden`、`baseline`、`preparation`、`constraint`。完成时会核对交付项。`forbidden`、`baseline`、`preparation`、`constraint` 记录执行时要遵守的条件，无须申报完成。`artifact`、`artifact_absent`、`content`、`command`
和 `tests` 必须提供 `target`；`tests` 的 `target` 是作为其证据的精确测试命令。
`content` 的 `text` 是 Gate 与 `target` 逐行比对的字面内容本身；若写成对该行的
描述，Gate 会拒绝并要求模型 amend `text`，而不是继续改文件。

## 声明方式

两个结构化入口，都由模型调用：

- `commit_task(requirements=[{kind, text, target, source_quote | source_span, check, verification}])`：
  Chat 轮注册任务时声明初始集合，按数组顺序应用，生成 `R1..Rn`。
- `declare_scope({operations: [...]})`：修改已绑定 Goal 的集合。`/goal` 轮、
  恢复的 Goal、已提交任务的补充需求都使用同一套操作。

每项需求只能用一种方式引用用户原话。`source_quote` 是模型逐字复制的文本；
`source_span` 指向权威用户消息的行范围（1-based、闭区间；`\n` 分行，末尾一个
换行不额外产生一行），由 Runtime 自己切出原始字节。因此模型输出里的标点宽度、
重新换行或 Unicode 归一化选择，不会把真实的用户需求变成被拒声明。两者同时提供
会被拒绝：Runtime 不猜模型想用哪一个。工具 schema 把 `source_quote` 列在
`required` 中（类型可为 null），因为部分 provider 只遵循 required 列表；仅用
span 的声明传 `source_quote: null`，Runtime 将其视为缺失，而不是第二种来源。
两种形式都不接受改写，也不做模糊等价（全角/半角、编辑距离或正则修复）。

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

操作集：`add`、`amend`、`replace`、`waive`、`restore`。没有 delete：`waive` 暂免该项并保留记录，`replace` 把旧需求标为 `superseded`，给替代项分配新编号，`restore` 恢复已暂免的需求。一次声明中的操作按数组顺序写入。

`get_goal_scope` 读取当前集合（id、种类、状态、quote、check），并可按需读取
带授权轮次的有序变更历史。

如果 `named` 交付项有可执行的验收条件，先登记断言，再通过正常的工具与权限路径执行。
例如项目可以提供只读脚本，检查配置指向的数据库。一旦发现演示账号，脚本就以非零状态退出。

```json
{
  "kind": "named",
  "text": "配置指向的数据库没有演示账号",
  "source_quote": "不要演示账号",
  "verification": {
    "tool": "run_command",
    "arguments": {"program": "./scripts/check-no-demo-accounts", "cwd": "backend"}
  }
}
```

凭据由项目配置或环境引用提供。可以通过获得授权的需求修改调整断言，定义修改后需要重新执行。
如果改变已绑定需求的交付结果，需要重新登记检查。其他修改可以保留原定义。

## 校验

1. **Schema 与边界。** 运行时拒绝未知的种类和操作。`text`、`quote`、`target`、`reason` 有长度上限。一次声明最多 `SCOPE_OPERATIONS_MAX` 个操作，单个 Goal 最多
   `SCOPE_REQUIREMENTS_MAX` 项有效需求。
2. **来源。** 每个条目的 `source_quote` 必须出现在权威用户文本里（精确匹配，或
   折叠连续空白后匹配）。`source_span` 会先在同一文本上解析；切出的结果按构造
   就是精确子串，因此严格校验仍然成立。`declare_scope` 的权威文本是 transcript
   中最新的用户输入：旧消息不能授权一个可能已被新消息反转的变更。`commit_task`
   的种子锚定它所记录的 task objective（用户消息；后续 amendment 会追加到
   其上），因为初始范围描述的是被提交的任务，而不是对它的变更。
3. **身份。** 目标只能是本轮绑定的 Goal。操作未知 id 或非 active 的需求会被
   拒绝，并给出当前状态。
4. **转换合法性与顺序。** `amend`、`waive` 要求 active；`restore` 要求 waived；
   `replace` 让 active 需求进入 superseded 并生成替代项。事件带每 Goal 单调
   递增的 `seq`，拒绝不会改变已记录的集合。

## 持久历史

`goal_requirement_events` 按 Goal 保存：`seq` 顺序、声明所在 `turn_id`、操作、
需求身份、kind/text/target、用户原文 `source_quote`、check、reason 与时间戳。
已登记的断言仅保存执行指纹和登记修订 ID。
当前清单由事件历史计算得出。会话恢复或上下文压缩后，仍用同一份历史。`record_scope` 在同一个数据库事务里读取历史、校验并应用声明、追加事件。

## 完成 Gate

`update_goal(status="complete")` 必须按编号报告每个仍有效的交付项
（`requirements=[{id, status, evidence}]`）：

- 有缺项或任意一项为 `not_done` 时，运行时拒绝结案。
- `unverifiable` 是明确的未验证边界，不是对 Runtime 可观察事实的豁免；
- `artifact` / `artifact_absent` 对照当前工作区与记录的变更；
- `content` 按目标文件的一行检查；
- `command` 必须匹配最后一次变更之后的精确命令成功运行记录；不同命令或复合
  命令行不是它的执行记录，同一命令的最新一次执行才是当前状态；
- `tests` 在 `target` 中声明精确的测试命令；该命令在最后一次变更之后、且启动与
  结束时处于同一工作区版本的通过运行记录，即满足该需求。不同命令或复合命令行
  不是它的记录。声明的命令就是证据
  契约：Runtime 记录这次精确执行，不读取 runner 名称、输出或测试数量。因此
  `verified` 表示声明的命令契约成立——这一行在最后一次变更之后执行并以 0 退出
  ——不表示该命令真的执行了任何测试。历史上未带 `target` 的 `tests` 需求，
  按显式未验证边界接受，而不是阻塞一个合法记录的 Goal；
- `named` 交付项属于模型判断。已交付的 `done` 记账记录为 `reported`，正常完成，
  不会因为「没有机械检查」产生警告。登记验收后则成为机械需求：必须在登记后
  执行同一定义并通过，并在 `requirements[].check_call_id` 引用该次执行。
  `unverifiable` 是模型对无法确立的交付给出的显式边界，保留 `unverified`
  记录与警告。

Gate 在每次完成声明时读取持久集合，因此被折叠的 transcript 或重启的进程都
无法缩小范围。模型完全不声明时 Gate 没有可执行集合；所以 Goal mode 在契约里
写明声明义务，`get_goal_scope` 始终展示 Gate 将读取的精确集合。没有应交付项
时，没有任何需求级事实可供验证，因此这种完成永远不会报告为 `Verified`：
引用也许能证明某个命令运行过，但没有检查任何已声明的交付物。

## 验证报告

完成报告对照模型提交的执行引用和运行时实际记录。

| 层 | 来源 | 证明什么 |
| --- | --- | --- |
| `update_goal(verification)` | 模型声明 | 模型用哪些执行来说明交付完成 |
| `VerificationRun` | Runtime 执行记录（`ToolCallStarted` + `CommandExecutionVersions` + `ToolCallFinished`、后台完成） | 命令、call id、退出状态、启动与结束时的工作区版本窗口、所在模型步 |

完成检查用真实执行记录核对每条引用。可以引用 `call_id`、完整命令行，或在命令行后加备注。引用失败的执行、最后一次变更之前的执行，或同一命令较早的一次执行时，检查会说明拒绝原因。运行时以执行记录为准，不根据命令名称或输出判断它是不是测试。

运行时在进程启动前和结束时各记一次工作区变更计数，保存到 `CommandExecutionVersions`。两次计数相同，且这个版本不早于完成判定时的工作区版本，命令结果才算当前证据。比如后台跑测试时又改了代码，测试成功的退出状态仍保留，验收要在当前工作区重跑。后来查看后台结果，也不会改变当时的执行版本。旧记录若没有启动版本，同样需要重跑。

工作区版本只统计运行时通过工具和自己管理的后台任务看到的改动。它不监控外部进程对文件系统的写入。

后台任务确实改了文件时，工作区版本只增加一次，和你通过自动通知、`wait_task` 或 `get_task` 读取结果无关。任务即使失败或被停止，实际写入仍算数。写入范围内的变更路径随任务结果保存，重启后还能查看。

如果写入型后台任务还在运行，完成检查会拿当前工作区和该任务启动时的状态对比，发现的变化只计一次。没有对比基线或对比失败时，变更情况保持未知。对比记录的是该写入范围内看到的变化，并发操作也可能出现在其中，无法据此判断某个文件一定由哪个任务修改。

被接受的完成会在任务终态持久化一份 `VerificationReport`
（`TaskFinished.verification`，`TaskReport` 上也是同一份）：

```json
{
  "status": "reported",
  "records": [
    {"requirement_id": "R1", "kind": "tests", "state": "verified",
     "detail": "verified against the execution record: `make regression` exited 0 at workspace version 4",
     "call_id": "call-12", "command": "make regression", "exit_code": 0,
     "workspace_version": 4, "started_workspace_version": 4,
     "finished_workspace_version": 4, "current_workspace_version": 4,
     "model_step": 7},
    {"requirement_id": "R12", "kind": "deliverable", "state": "reported",
     "detail": "delivered on the model's account; this requirement's contract is model judgement, so no mechanical check applies",
     "workspace_version": 4, "model_step": 0}
  ]
}
```

`records` 按需求顺序、每个应交付项一条。只有真实执行记录支撑时才会出现证据
字段；否则 `call_id`/`command`/`exit_code` 缺省，`detail` 说明 Runtime 观测到
什么、无法检查什么。`workspace_version` 是该执行被计入的变更计数器（包含命令
自己报告的写入）；`started_workspace_version` / `finished_workspace_version`
是执行窗口，`current_workspace_version` 是完成判定时的当前计数器。因此无需
重放 transcript 就能看出结论是否过期或版本是否一致。`Verified` 要求至少有一条
应交付项记录，且每条记录都是 `verified`。对 `tests` 需求，该结论来自所声明精确命令的当前运行；
记录陈述该次执行，不会声称 Runtime 未观测到的测试数量或覆盖率。`Reported`
表示所有机械检查均通过，且至少一条记录来自已交付的模型判断。`Unverified` 表示至少一项必要事实缺失或无法确立；只有这种聚合才会
产生证据警告。

重放是增量的。2026-09-23 之前的终态把已移除的宿主验证结论写成同一个字段里的
裸字符串（`"verification": "not_run"`）；它不是报告，读作「没有报告」，旧会话
仍可重放。结构化对象按严格模式解析：损坏的报告仍会让整次重放失败，而不是静默
变成「没有报告」。

界面根据任务是否完成或受阻，以及报告的验证状态，显示以下五种结果。

| 结果 | 表示 |
| --- | --- |
| Verified | `TaskOutcome::Completed` + `VerificationStatus::Verified` |
| Reported | `TaskOutcome::Completed` + `VerificationStatus::Reported`（正常完成：模型判断并交付自身需求，且所有机械检查均通过） |
| CompletedUnverified | `TaskOutcome::Completed` + `VerificationStatus::Unverified` |
| Incomplete | 被接受轮次上的非交付 `StopReason`（预算、中断） |
| Blocked | `TaskOutcome::Blocked` |

任务可以带着未验证项完成，界面会给出警告，报告会列明缺哪些证据。`Reported` 不显示这类警告，仍会记录哪些需求经过程序检查，哪些由模型判断。只读 Goal 显式声明 `not_applicable` 时，报告为 `Reported`，交付记录为空。

## 不属于本契约

- 没有关键词、否定词、章节标题、测试 runner 或状态词表。
- 不从自然语言抽取需求——提交时、恢复时、折叠后都不抽取。
- 不为分类用户消息增加第二次模型调用；正在执行的轮次自己阅读并声明。
- 记录需求不需要人工审批；用户的 quote 是来源，Runtime 只负责身份与持久性。
