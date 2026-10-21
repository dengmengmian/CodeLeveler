# 性能与开销：改造前后

## 先说明可比性

真实 devorder 会话（12 个 Chat 回合、256 次工具调用）**没有在原项目上重跑**：那会再次修改用户仓库并消耗真实模型额度。下表的 Before 是该会话的真实记录；After 来自两类可复现实验：

- 受控 5 阶段任务（DF-2，11 轮，脚本 Provider；用于证明长回合行为）；
- 真实 DeepSeek 5 需求任务（DF-10，7 轮；用于证明真实 Provider 端到端）。

任务规模不同，**wall time 与 token 绝对值的差异不能全部归因于本次修改**。可比的是契约指标：是否进入 Goal、是否提前结束、是否需要用户再次说“继续”、完成是否有验证依据。

## Before：真实事故会话

| 指标 | 实际值 |
|---|---:|
| Turn 数 | 12（全部 Chat） |
| 模型请求 | 226（round 189；aux 37） |
| 输入 tokens | 36,333,683（缓存 34,224,768，94.2%） |
| 输出 tokens | 204,317 |
| Tool calls | 256 |
| Compaction | 0 |
| Goal 行 | 0 |
| 提前 Final | 12 / 12（全部 `answered`） |
| 用户续说次数 | `/goal` 之后仍有 5 次「继续」类输入 |
| Wall time | 37m26s |
| 完成依据 | 模型文字；无运行时验收 |

每回合输入 tokens 从 40K 增长到 7.25M（turn 12），说明上下文本身不是“被截断导致提前结束”；模型每次都在回合边界主动收束。

## After：受控长任务（DF-2）

| 指标 | 实际值 |
|---|---:|
| Turn 数 | 1 |
| 模型轮次 | 11（5 次阶段总结 + 5 次写入 + 1 次 `update_goal`） |
| Tool calls | 6 |
| Goal 行 | 1 |
| 提前 Final 被接受 | 0 |
| 用户续说次数 | 0 |
| 阶段完成 | 5 / 5 |
| 最终状态 | `completed` + 显式 `not_applicable` 警告 |
| Wall time | 4.2s（脚本 Provider） |

阶段总结触发的是有界 closeout 续跑，不是新回合；5 个阶段在同一 Goal 回合内完成。

## After：真实 Provider（DF-10）

| 指标 | 实际值 |
|---|---:|
| 模型 | `deepseek/deepseek-v4-flash` |
| 模型轮次 | 7 |
| Tool calls | 7（读取、验证与收口合共 7 次） |
| 输入 tokens | 61,819（缓存 46,592，75.4%） |
| 输出 tokens | 1,355 |
| 验收 | `python3 -m unittest` 独立复核通过 |
| Goal 行 | 1 |
| 提前 Final | 0 |
| 完成验证 | 模型引用 `python3 -m unittest`，运行时匹配到成功执行 |
| Wall time | 27.3s |

这条路径没有为验证契约多花模型轮次：模型在同一个 `update_goal` 里给出了真实运行过的命令，运行时只做记录匹配。

## 固定开销

| 项 | 开销 | 说明 |
|---|---|---|
| Goal 工具 schema | 初始 Goal 工具面 **6682 → 6897 tokens（+215，+3.2%）** | 仅 Goal 回合；预算由 6800 调到 7000，测试锁定 |
| 每个 Goal 请求 | +215 tokens（首次未缓存，后续随前缀缓存） | 11 轮约 2.4K token-equivalents；真实 DF-10 缓存率 75.4% |
| 验证通过时 | **0 额外模型调用** | 只做本地命令记录匹配 |
| 验证被拒时 | **+1 轮** | 只有模型声明不被事实支持时才发生；DF-3 共 3 轮（失败命令 → 被拒完成 → blocked） |
| 空转 | 最多 2 次续跑后停止 | DF-6 共 3 次主请求（初始 + 2 次续跑），不会无限执行 |
| Patch 写入 | 每个修改文件 +1 次回读 | 本地文件读，不增加模型调用 |
| 脱敏 | O(文本长度 × 会话注册值数) 的字符串扫描 | 无额外模型调用；注册值有 256 个上限 |

## Before / After 契约指标

| 指标 | Before（真实事故） | After（DF-2 / DF-10） |
|---|---|---|
| 是否进入 Goal | 否（`collaboration=chat`） | 是（`goal`，持久化） |
| 阶段结束是否终止总目标 | 是，12/12 次 | 否；5 次阶段总结全部继续 |
| 用户是否需要再次说「继续」 | 需要，5+ 次 | 不需要 |
| 提前 Final 被接受 | 12 次 | 0 次 |
| 完成是否有运行时依据 | 无 | 引用命令 + 执行记录匹配；无验证时带警告 |
| 失败验收能否被忽略 | 可以（Chat 无完成判定） | 不能；`verification_refused` |
| Compaction 后目标保留 | 本会话未发生 | DF-4 通过（真实 `/compact`） |
| Resume 后同一 Goal | 本会话未发生 | DF-5 通过（真实 Esc 中断 + 重启） |
| 路径被脱敏改写 | 是（import 路径 → `[REDACTED]`） | 否（DF-7） |
| 空补丁谎报成功 | 是 | 否（DF-8，`changed nothing`） |

## 结论

本次改造没有引入隐藏模型轮次、重复总结或持续注入：

- 正常完成路径的额外模型调用为 0；
- 只有“声明不被事实支持”时才多一轮，且该轮用于纠正，不是重试刷分；
- 空转有 2 轮硬上界；
- 固定成本是 Goal 工具面的 +215 tokens，且随 prefix cache 摊销。

代价换来的不是“更强制的模型行为”，而是**运行时能准确判断自己是否被要求继续、以及模型的完成声明是否有可核对依据**。

## Final Hardening（2026-10-21）

本轮修复不增加模型调用，只增加持久化写入：

| 路径 | 额外成本 |
|---|---|
| Chat → Goal 升级 | 绑定事务后 1 次 `UPDATE model_requests`（移动本窗口的行）+ 1 条 `progress_updated`（换 scope 时） |
| Resume 预算迁移 | 1 次扫描该会话的 turn payload；每个已绑定该 goal 的 Chat 窗口 1 次 `UPDATE`（幂等，通常 0–1 行） |
| Goal 创建窗口 | 无声明时 1 次 `set_pending_goal_if_absent`；TurnStart 仍是 1 个事务（多 1 条 UPDATE） |
| 崩溃恢复 | 复用既有 turn/goal 行；DF-13 恢复后只执行 1 个 Goal turn |

DF-10（真实 DeepSeek，本轮最终二进制）：

| 指标 | 值 |
|---|---:|
| Model rounds | 5（另 1 次 prompt suggestion） |
| Input / Output / Cached | 42,120 / 622 / 28,928 |
| Reasoning tokens | 162 |
| Cost | 6,023 micro-USD ≈ $0.006 |
| Wall time | 22.2s |
| Compaction / Resume / 重复执行 | 0 / 0 / 0 |

DF-12（PTY + 真实 SQLite）：升级窗口 2 条 main-round 行，估计 token 8,358 / 9,296，
两条 `budget_scope` 都是同一个 goal id；没有额外模型请求。DF-13：1 个 goal、2 个
turn、0 次重复执行。

## Chat 显式委托（2026-10-21）

新增 `commit_task` 的固定成本与真实开销：

| 项 | 值 | 说明 |
|---|---:|---|
| `commit_task` tool schema | 356 tokens | 每个顶层可写 Chat 请求 |
| Chat 委托规则 prompt | 190 tokens | 同一请求的会话前缀 |
| 合计固定成本 | **546 tokens/请求** | 进入 prefix cache；不增加模型请求 |
| 普通 Chat 主请求 | 1（DF-14） | 与 DF-9 相同；无分类调用、无续跑 |
| 普通 Chat 估算输入 | 10,108 tokens | 含 546 新成本；其余是既有 system/tool/history |
| 长任务主请求 | 9（DF-15 脚本）/ 8（DF-20 真实） | 一次用户提交，无需人工 `继续` |
| DF-15 估算输入 | 98,590 tokens | 9 轮，脚本 Provider |
| DF-15 工具调用 | 6 | 3 个 stage 写 + 验收 + commit + update_goal |
| DF-20 工具调用 | 8 | 3 个 stage 写 + 验收 + commit + update_goal + 读取 |
| DF-15 wall | 6.4s | 脚本 Provider |
| DF-20 真实 Input / Output / Cached | 80,637 / 1,974 / 58,368 | DeepSeek v4-flash |
| DF-20 Cost / Wall | 11,749 micro-USD ≈ $0.012 / 43.6s | 8 个 main round |
| DF-20 验收 | 独立 `python3 -m unittest` rc=0 | 3 个 stage 文件 |
| 提前 Final / 无效续跑 | 0 / 0 | DF-15、DF-16、DF-20 |
| 取消 / 预算 / 无进展 | 均有界 | DF-19 + `task_commit_test` |

设计取舍：没有为每个 Chat 增加一次意图分类模型调用。Runtime 只处理结构化工具调用，
因此普通问答仍是一次请求；代价是每个 Chat 请求多 546 tokens 的固定前缀（可缓存），
以及一次 `commit_task` 工具轮（仅长任务）。

预算归属：`GoalBinder::bind` 把当前窗口已产生的 `model_requests` 行从 turn scope 迁到
goal id；DF-15/DF-18/DF-20 的所有 main-round `budget_scope` 都是同一个 goal id。
Resume 从未重置预算；DF-18 中断前后仍是一条 scope。

恢复：DF-18 证明中断的 Chat turn 通过 `turn.payload.goal_id` 以 Goal profile 恢复，
objective、已完成 stage、budget scope 均保留；没有第二条 goal。
