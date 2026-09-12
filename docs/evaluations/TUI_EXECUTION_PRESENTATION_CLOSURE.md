# TUI Execution Presentation Closure

TUI 执行过程、计划、交互选择与最终状态展示闭环。

- 初始 HEAD：`2d1cf19eaa443b97abe9e40c5a547580e0fe6888`
- 工作区起点：`crates/leveler-execution/src/command.rs` 有一处未提交改动（Windows
  进程树 canary），本轮**未触碰**；它在本轮进行中被单独提交为 `0f901f0`。
- 本轮改动分支：`main`（未新建分支，未 push，未 commit 产品代码）

## 1. 调查结论

现有 TUI 已经具备本任务多数要求，不是从零搭建：

| 规范 | 现状 | 结论 |
| --- | --- | --- |
| §3 线性 Transcript | `TranscriptItem` 按事件顺序追加，无虚构进度树 | 已满足 |
| §7 权威 Plan | `state.plan` 来自 `RuntimeEvent::PlanUpdated`；单一 sticky dock，原地更新；`plan_viewport.rs` 保证 9 项不丢 | 已满足 |
| §8 实时活动行 | `status_line::busy_status_lines` 单行、原地刷新、不进 Transcript | 缺步骤号与工具数 |
| §9 滚动 | `conversation/viewport.rs` 自动跟随 + `▼N` 未读徽标 + End 回底 | 已满足 |
| §10 Choice | `Overlay::Clarification`，真实 `ClarificationRequested` | 答案不留痕 |
| §11 Approval | `Overlay::Approval`，四档决策，默认落在拒绝 | 待批准的调用仍显示为运行中 |
| §12 完成状态 | `TurnEnd*` 全部来自 runtime 事件，终态区分齐全 | 缺"无最终回答"态 |
| §13 工具语义 | `tool_taxonomy.rs` 把 `update_plan` 渲染成"更新计划" | 已满足 |
| §14 符号 | `◌ · ✓ ✗ ⚠ ▸/▾`，`▸` 不表示运行中 | 已满足，沿用 |

真正的缺口集中在五处，全部与"证据"有关：

1. **§6 applied diff 被截断。** `push_edit_diff_body` 折叠时 16 行、展开时 48
   行，末尾一句"点击展开完整 Diff"。`write_file`（新建文件）根本进不了 edit 节点，
   整个 diff 不显示；单个 `apply_patch` 折叠时也只有一行。
2. **§4 工具证据被聚合替换。** 完成后的工具组折叠成一行 `▸ 读取 4 个文件`；运行中
   已结算的调用让位给 `· 已读取 4 个文件`。四个问题（什么工具/什么对象/什么结果/
   什么状态）一个都答不上。
3. **§5 并行关系靠推断。** `parallel: bool` 是"这个工具可以进并发批"，不是"它和别
   人一起跑过"。连续两轮各一次可并行的读会被算成"2 个并行"。
4. **§10.7 用户的选择不留痕。** `is_user_input_tool` 分支既不发 `ToolCall` 也不发
   `ToolResult`，问题和答案只活在 overlay 里，回车之后同时消失——屏幕、会话日志、
   replay 全都没有。
5. **§11 待批准的调用显示成运行中。** 批准框打开时，那条调用的 Transcript 行仍是
   `◌ 执行命令 $ rm -rf stale …`——和真正在执行的命令同一个记号，还带着一个在走的
   秒表。`UiApprovalRequest` 当时不带 `call_id`，UI 无法知道是哪一行。

过程中另外发现两个既有缺陷（详见 §5）。

## 2. 实现架构

沿用现有分层，没有新造命名：

```
RuntimeEvent
  → reducer/runtime_apply.rs        （生命周期，唯一权威）
  → TranscriptState                （历史 + 并发批身份）
  → activity_stream / tool_cell     （工具语义渲染）
  → presentation/disclosure         （复用的折叠行组件）
  → conversation/build              （行 + 命中行，同一缓存）
```

## 3. 改动清单

| 文件 | 改动 |
| --- | --- |
| `leveler-tui/src/tool_cell.rs` | `push_edit_diff_body` 去掉行数上限；`is_guard_denied_name` 收窄；`request_user_input` 摘要取 question |
| `leveler-tui/src/activity_stream.rs` | `render_group` 重写为"每调用一行证据"；新增 `StreamUnit::Batch` 树渲染；edit 节点改读 applied diff |
| `leveler-tui/src/transcript.rs` | `ToolCallBlock.batch`；`assign_batch`；`TurnEndStatus::NoFinalAnswer`；`settle_final_answer` |
| `leveler-tui/src/status_line.rs` | 实时活动行加步骤号与工具数 |
| `leveler-tui/src/reducer/runtime_apply.rs` | 无最终回答时替换终态 |
| `leveler-tui/src/render/transcript_lines.rs` | 渲染 `NoFinalAnswer` |
| `leveler-tui/src/terminal_title.rs` | 终端标题不给无回答的轮次打 ✓ |
| `leveler-tui/src/i18n.rs` | 删 `fold_full_diff` / `parallel_more_running` / `observe_denied`；加 `goal_update_rejected` / `turn_no_final_answer` |
| `leveler-agent/src/executor/drive.rs` | clarification 前后各发一条生命周期事件 |
| `leveler-client-protocol/src/approval.rs` | `UiApprovalRequest.call_id`（可选、`serde(default)`） |
| `leveler-app/src/prompt_bridge.rs` | 填入真实 `call_id` |
| `leveler-tui/src/overlay/approval.rs` | overlay 解析一次 `gated_call` |
| `leveler-tui/src/state.rs` | `approval_gated_call()` |
| `leveler-tui/src/conversation/view.rs` + `build.rs` | `ConvKey` 纳入待批准调用，避免缓存留旧行 |
| `testdata/session_transcript.golden.json` | 按新增字段重新生成（移动端 golden 测试 4/4 通过） |

## 4. 各类状态的数据来源

| UI 状态 | 来源 | 不来自 |
| --- | --- | --- |
| Plan 步骤与状态 | `RuntimeEvent::PlanUpdated` | 助手自然语言 |
| 工具 running/ok/failed | `ToolCallStarted` / `ToolCallCompleted.ok` | 文字或颜色推断 |
| 并行批身份 | `ToolCallBlock.batch`，由"B 启动时 A 仍在 Running"这一事件顺序事实赋值 | 时间接近、`parallel` 标志单独使用 |
| Edit 实际改了什么 | `ToolCallCompleted.applied_diff` | 调用参数里的 patch |
| Edit 统计 `+A −B` | 与上面同一份 patch 文本 | 调用参数 |
| Choice / Approval | `ClarificationRequested` / `ApprovalRequested` | 本地推断 |
| 用户答案 | 新增的 `ToolResult.preview` | overlay 临时状态 |
| 哪一行在等批准 | `UiApprovalRequest.call_id` | "最近一个运行中的调用" |
| 完成 Footer | runtime 终态 **且** transcript 里有已提交的 Final 回答 | 工具循环结束 |

## 5. 顺带修掉的两个既有缺陷

1. **失败的 grep 被塞进一句编造的解释。** `is_guard_denied_name` 按工具名把
   `grep` / `find_files` / `list_files` / `git_status` 的任何失败改写成
   "已跳过：重复的检查无需再次执行"。那个 observe-dedup guard 早已从 runtime 删除
   （该英文原文现在只存在于一个 TUI 测试里），所以这句话背后什么都没有：一个正则
   语法错会被显示成"跳过了重复检查"。现在只有 `update_plan` / `update_goal` 会被
   替换文案，其余失败照实报错。
2. **失败的 edit 会显示请求里的 patch 当作结果。** `edit_unit_lines` 的统计行从调用
   参数算 hunk 数和 `+A −B`，与下面渲染的 applied diff 不是同一份来源。现在两者同源。

## 6. 尚未解决 / 已知限制

- **§5 真实并行批在本机无法用真流量触发。** 唯一可用的 provider（DeepSeek）配置为
  `parallel_tool_calls = false` / `max_parallel_tool_calls = 1`；Kimi k3 支持 16 路
  并发，但 Moonshot 的 schema 校验拒绝 `update_plan` 的工具定义
  （`At path 'properties.plan.items': detected infinite recursion without
  termination condition`），整轮开局即失败。该 schema 问题是既有缺陷，不在本任务
  范围内。并行批逻辑由确定性 reducer + renderer 测试覆盖。
- **§6.6 Partial Apply 在本 runtime 不存在。** `tools/patch/apply.rs` 明确
  "returns an error and makes no partial change"，apply_patch 是全有全无。没有为它
  造 UI 状态。
- **applied diff 上游有 64KB 上限。** `applied_diff.rs` 的 `MAX_APPLIED_DIFF_BYTES`
  超限时不发布，UI 退回到无行号的请求 patch。这是 runtime 既有边界，UI 侧不再截断。
- **`observe_denied` 文案删除后，历史会话 replay** 若含已删除 guard 的英文原文，会
  按原文显示（那确实是工具当时的返回），不再被替换。

## 7. 测试

```
cargo fmt --all                      # 无改动
cargo clippy --workspace --all-targets   # 0 warning 0 error
cargo test -p leveler-tui            # 626 + 19 + 2 + 243 + 36 + 7 + 1 = 934 passed
cargo test -p leveler-tui --test product_scenarios -- --ignored   # 11 passed
cargo test -p leveler-tui --test visual_dump -- --ignored         # 2 passed
cargo test --workspace               # 3749 passed, 0 failed
flutter test test/transcript_golden_test.dart                     # 4 passed
```

`cargo test --workspace` 必须在 `env -u NODE_OPTIONS` 下跑：本机 shell 的
`NODE_OPTIONS` 指向一个沙箱内不存在的 `--require` 脚本，会让
`leveler-execution` 的 confined npm 用例失败。与本轮改动无关。

新增 / 重写的测试，按规范条目：

| 条目 | 测试 |
| --- | --- |
| §6 完整 diff | `tool_cell`: 500 行 patch 逐行在场、无隐藏计数、展开与折叠一致、gutter 列不漂移；`render`: 40 行 inline diff 折叠态即完整 |
| §6 只展示真正应用的改动 | `activity_stream`: 失败的 edit 既无 diff 也无统计；应用后的 edit 渲染 runtime 报告的 diff；新建文件内容完整；统计与 diff 同源 |
| §4 每调用一行证据 | `activity_stream`: 开放/关闭组各自保留每调用行；单调用无冗余表头；一行答齐工具/对象/结果；静默探针不贡献行也不进计数 |
| §5 真实并行批 | `transcript`: 观察到重叠才同批、顺序调用永不成批、跨批不合并、runtime 未并发派发的调用不入批；`activity_stream`: 树渲染、表头计数等于子行数、无批不声称并行、单成员不成树、子项各自状态、两批两树、17 路全部在场 |
| §10.7 选择留痕 | `leveler-agent`: clarification 的问题与答案进入事件流；`activity_stream`: 问题与答案都在行上 |
| §11 待批准 ≠ 运行中 | `activity_stream`: 等待记号、只标被批准框持有的那一条、无批准框时照旧；`reducer`: 真实 `ApprovalRequested` → `等待批准` → `ApprovalResolved` 后恢复 |
| §8 实时活动行 | `status_line`: 有 plan 报步骤号与工具数、无 plan 不编造分数、零工具不报零 |
| §12 完成真相 | `reducer`: 无回答不显示完成（`TurnCompleted` / `TurnAnswered` 两路）、有回答保留 runtime 终态、被工具作用过的旁白不算回答、失败终态不被改写 |
| 顺带修的两处 | `activity_stream`: 真实失败的搜索报真实错误；`tool_cell`: 只有 plan/goal 失败会被替换文案 |

## 8. 真实 TUI Dogfood

不是 mock，不是 snapshot：从一份干净副本 release 构建，在独立 `LEVELER_HOME` 和独立
lab 仓库里，用真实 PTY 驱动真实 TUI，打真实 provider
（`deepseek/deepseek-v4-flash`，`reasoning_effort=max`）。

两次构建：第一程 `leveler 0.2.0-beta.1 (605707dcf06c)`，第二程
`(32f8d5e700a9)`（含 §11 修复）。干净副本是必要的——`BuildIdentity::matches` 要求两
侧都不 dirty。

Lab 仓库是一个单组消费信号量的 Go worker，任务要求读两个文件、搜两处用法、改两个
文件、跑 `go test`。

### 第一程：完整流程（`assisted`，产品默认）

17 次工具调用，1m58s，无 panic。截图存于本目录
`tui_closure_dogfood_*.txt`。

- **计划**（`_plan.txt`）：`▸ 检查代码库` 表头下**六条**逐调用证据行，各自写明工具、
  对象、结果行数；单一 plan dock 三项全在，`● 1.` 标当前项；`✓ 更新计划 · 3 行` 用
  产品语义而不是工具名；实时行 `⠦ 等待模型 · 当前 1/3 · 58s · 7 次工具 · ↑12,130 ↓3,186`。
- **applied diff**（`_diff.txt`）：`✓ 编辑文件 internal/config/config.go` /
  `└ 1 处修改 · +4 −3` / 7 行 diff 全在，行号两侧各自正确。
- **长 diff 滚动**（`_scrolled.txt`）：worker.go 的多 hunk 改动向上滚动后逐行可查，
  旧文件侧 39–52 为删除、新文件侧 58–71 为上下文与新增，右下角 `▼120` 报还有 120 行；
  没有任何"点击展开完整 Diff"。
- **执行中的命令**：`◌ 执行命令 $ go test ./internal/worker/... …`，实时行同步为
  `⠴ 执行命令 go test ... · 当前 1/3 · 1m 37s · 13 次工具`。
- **完成**（`_final.txt`）：`▸ 执行了 2 个命令` 下两条命令各一行带结果；最终回答流式
  输出完毕后才出现
  `── ✓ 任务已完成 · 17 次工具 · 1m 58s · 验证 ✓ ──`；实时活动行消失。
- **Changes**（`_changes.txt`）：三个文件 `+4 -3` / `+48 -16` / `+39 -0`，与
  `git diff --stat` 一致；`go test ./internal/worker/...` 真的通过。

### 第二程：真实 Approval（`request-approval` + 破坏性命令）

`go test` 在任何 profile 下都是 `Requirement::Auto`（策略明确放行普通构建/测试），
所以第一程没有批准框。第二程让 agent `rm -rf stale`，命中
`command_is_destructive` → `NeedApproval`。截图为 `_approval.txt` /
`_after_allow.txt`，binary `32f8d5e700a9`（含 §11 修复）。

- 批准框真实出现：`等待审批` / `允许 执行命令（rm -rf stale && ls）` /
  `⚠ 可能造成破坏性变更` / 四档选项，光标默认落在 `4. 拒绝`。
- **待批准的那一行不再显示为运行中**：
  `⚠ 执行命令  $ rm -rf stale && ls · 等待批准`——警告记号、没有秒表、写明在等什么。
  修复前同一场景是 `◌ 执行命令 $ rm -rf stale …`。
- **等待期间没有 spinner**：`tui.busy()` 实测 `False`（`result.json`）。
- 等待期间向上滚动不被拽回底部。
- 按 `y`（仅允许本次）后该行转为 `✓ 执行命令 $ rm -rf stale && ls · 2.9s · 2 行`，
  实时活动行恢复——缓存确实随批准状态失效了。
- **决策真的流到了 runtime**：`stale/` 目录在文件系统上确实被删除，不是只改了颜色。

### 未能用真流量覆盖的一项

§5 的真实并行批。见 §6 已知限制：唯一可用 provider 不做并发工具调用，支持 16 路并发
的 Kimi k3 在 `update_plan` 的 schema 校验上开局即失败。并行批的身份判定与树渲染由
上表 13 个确定性测试覆盖。

## 9. 判定

```
TUI_TRANSCRIPT_STRUCTURE=PASS
TUI_PLAN_AUTHORITY=PASS
TUI_TOOL_EVIDENCE=PASS
TUI_FULL_DIFF=PASS
TUI_INTERACTION_STATE=PASS
TUI_LIVE_ACTIVITY=PASS
TUI_COMPLETION_TRUTH=PASS
TUI_SCROLL_ACCEPTANCE=PASS
TUI_REAL_DOGFOOD=PASS
TUI_EXECUTION_PRESENTATION_CLOSURE=PASS
```

判定的限定条件写在 §6 与 §8 最后一节，不在这十行里省略：真实并行批未经真流量验证，
Partial Apply 在本 runtime 不存在。除此之外每一项都有真实 TUI 截图或确定性测试。

## 10. 产品代码之外的额外改动

- 本文档与同目录七张 dogfood 截图。
- `testdata/session_transcript.golden.json` 按新增的可选字段重新生成。

没有新建分支，没有 push，没有提交产品代码。`crates/leveler-execution/src/command.rs`
的既有未提交改动未被触碰（它在本轮进行中由别处提交为 `0f901f0`）。
