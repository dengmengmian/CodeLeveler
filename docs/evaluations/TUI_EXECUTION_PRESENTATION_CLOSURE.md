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

- **§5 真实并行批已用真流量关闭。** 见 §11。此前写"本机无法触发"是配置读成了能力：
  DeepSeek 的 `parallel_tool_calls = false` 只是往线上发 `parallel_tool_calls:
  false`，把并发关掉了，不是 provider 不支持。
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

## 11. 真实并行工具批次（REAL PARALLEL TOOL BATCH DOGFOOD）

上一轮把这一项记成"本机无法用真流量触发"。那是把配置读成了能力：DeepSeek 的
`parallel_tool_calls = false` 只是让 `openai_chat` 适配器往线上发
`parallel_tool_calls: false`（`openai_chat/mod.rs`：只有模型不能并行时才发这个字段），
主动把并发关掉了。把它在**一份隔离的 `LEVELER_HOME`** 里改成 `true` 之后，
provider 一次响应就返回了三个 tool call。仓库产品代码一行没动。

另一个关键事实：执行器的批宽**不由**模型 profile 决定。`coding/policy.rs` 的
`DEFAULT_PARALLEL_TOOLS = 4` 是唯一来源，注释写明 profile 的
`max_parallel_tool_calls` 是"conservative placeholder"，折进来会把 4 悄悄降成 1。所以
只要 provider 肯一次给多个调用，运行时就会真并发跑。

### 11.1 正向：一次响应 → 一个并发批

| 项 | 值 |
| --- | --- |
| Provider / 模型 | `deepseek` / `deepseek-v4-flash`（`reasoning_effort=max`） |
| 网关 | `https://taotoken.net/api/v1` |
| 模型请求数 | 2（一次给出三个 grep，一次 `update_goal`） |
| Session | `30fabaa1-4749-4e8e-8e24-9962d5b3dc4c` |

一次响应里的三个 provider call ID（`events` 表原文）：

```
call_bfl3we8eusd3q3fzxjmnf9pc  grep  parallel=true  {"path":"mod_a","pattern":"ZZQQ_ABSENT_TOKEN_9137"}
call_azczs6qyxrvt7m060guyl24j  grep  parallel=true  {"path":"mod_b","pattern":"ZZQQ_ABSENT_TOKEN_9137"}
call_on19wvaat7qlri3910w8v8u7  grep  parallel=true  {"path":"mod_c","pattern":"ZZQQ_ABSENT_TOKEN_9137"}
```

Runtime 批次身份来自持久化事件的顺序本身——三个 `tool_call_started` 全部先于任何
`tool_call_finished`：

```
seq 3  tool_call_started   2026-09-12T08:28:41.217802Z
seq 4  tool_call_started   2026-09-12T08:28:41.218755Z
seq 5  tool_call_started   2026-09-12T08:28:41.219230Z
seq 6  tool_call_finished  2026-09-12T08:28:41.780373Z
seq 7  tool_call_finished  2026-09-12T08:28:41.780373Z
seq 8  tool_call_finished  2026-09-12T08:28:41.780373Z
```

对照顺序路径（§11.3 seq 3→4）是"宣告一个、跑完一个、再宣告下一个"。三个 started 先
于全部 finished，只有 `parallel_jobs` 那条路径会产生。

### 11.2 时间区间重叠的量化证明

只有相同 batch ID 不算证据，所以用同一 corpus、同一模型做了 A/B 计时。语料是三份各
50MB 的 Go 文件；pattern 取一个**不存在的 token**，让 grep 无法提前退出，必须整份扫完。

**Arm B（线上能力关掉 → 一轮一个调用）**，每个调用的 start/finish 落在不同的持久化批
次里，所以单次全量扫描的真实成本可读：

| 调用 | start → finish | 耗时 |
| --- | --- | --- |
| grep mod_a | 08:26:52.915968 → 08:26:53.006406 | 90.4 ms |
| grep mod_b | 08:26:55.881433 → 08:26:55.965954 | 84.5 ms |
| grep mod_c | 08:26:58.337549 → 08:26:58.416832 | 79.3 ms |
| **合计扫描工作量** | | **254.2 ms** |

**Arm A（同一任务，能力打开 → 一轮三个调用）**：

```
第一个 start → finish   114.3 ms
最后一个 start → finish 113.7 ms
```

254.2 ms 的扫描工作在 114.3 ms 的窗口里做完了。**至少 139.9 ms 的工作必须是并发的**；
三个各约 85 ms 的区间挤进 113.7 ms 的窗口，两两重叠至少 55.8 ms。顺序执行不可能得到
这个数。

放大到每份 200MB 之后（PTY 那一程），批次跨度 562.6 ms，而 TUI 自己测得的
`duration_ms` 三条都是 **0.6s**：1.8 s 的工具时间落在 0.56 s 的跨度里。TUI 的独立测量
和事件日志的时间戳互相印证。

### 11.3 执行中与冻结后的画面

真实 PTY，真实 TUI，binary `32f8d5e700a9`。截图 `tui_closure_parallel_live.txt` /
`tui_closure_parallel_frozen.txt`。执行中的形态被抓到 10 帧。

执行中：

```
    ◌ 并行处理 3 项
    ├ ◌ 搜索代码  "ZZQQ_ABSENT_TOKEN_9137" in mod_a …
    ├ ◌ 搜索代码  "ZZQQ_ABSENT_TOKEN_9137" in mod_b …
    └ ◌ 搜索代码  "ZZQQ_ABSENT_TOKEN_9137" in mod_c …
```

完成后冻结：

```
    ▸ 搜索代码库
    · 并行处理 3 项
    ├ · 搜索代码  "ZZQQ_ABSENT_TOKEN_9137" in mod_a · 0.6s · 1 行
    ├ · 搜索代码  "ZZQQ_ABSENT_TOKEN_9137" in mod_b · 0.6s · 1 行
    └ · 搜索代码  "ZZQQ_ABSENT_TOKEN_9137" in mod_c · 0.6s · 1 行

  ● 三处均无命中

  ── ✓ 任务已完成 · 3 次工具 · 9s ──────────────────────────────
```

每个子工具都写明自己作用的对象（`in mod_a` / `in mod_b` / `in mod_c`）和自己的结果
（`0.6s · 1 行`），表头计数等于子行数。Completion Footer 在最终回答 `● 三处均无命中`
之后才出现。无 panic，无 DIFF 或 Transcript 丢失。

### 11.4 反向：两个顺序轮次不得合并

任务要求分两步：第一步同时读两个文件，第二步等结果回来后再同时发两个 grep。两轮都是
可并发的，所以只按 `parallel` 标志分组的 UI 会显示一个"并行处理 4 项"。

事件日志（session `cf00331d-7215-417d-bfa5-ccc45b6ffb45`，3 次模型请求）：

```
seq  3  tool_call_started   08:30:00.452350   ┐ 批次一
seq  4  tool_call_started   08:30:00.452780   │
seq  5  tool_call_finished  08:30:00.457020   │
seq  6  tool_call_finished  08:30:00.457020   ┘
                     ── 间隔 2.54 s ──
seq  9  tool_call_started   08:30:02.995596   ┐ 批次二
seq 10  tool_call_started   08:30:02.996209   │
seq 11  tool_call_finished  08:30:04.029031   │
seq 12  tool_call_finished  08:30:04.029031   ┘
```

TUI 的画面（`sequential_out/01_final.txt`）：

```
    ▸ 检查代码库
    · 并行处理 2 项
    ├ · 读取文件  mod_a/round1_a.txt · 1 行
    └ · 读取文件  mod_b/round1_b.txt · 1 行
    · 并行处理 2 项
    ├ · 搜索代码  "ZZQQ_ABSENT_TOKEN_9137" in mod_a · 1.0s · 1 行
    └ · 搜索代码  "ZZQQ_ABSENT_TOKEN_9137" in mod_b · 1.0s · 1 行
```

同一个工具组里两个独立批次，各带自己的两个子项。全程没有出现"并行处理 4 项"。

### 11.5 Kimi / Moonshot：根因已定位，按独立缺陷记录

因为 DeepSeek 已经完成真实并行验收，本轮**不修 Kimi**。但根因是量测出来的，不是照
错误信息猜的——直接对 Moonshot 端点提交四种 schema 变体：

| schema 形态 | 结果 |
| --- | --- |
| 出厂原样（`$defs` + `allOf[$ref]` + 兄弟 `description` + nullable） | **REJECTED** |
| `allOf: [{$ref}]`，无兄弟键 | **REJECTED** |
| 裸 `$ref` + 兄弟 `description` | ACCEPTED |
| 裸 `$ref`，无兄弟键 | ACCEPTED |
| 完全内联（保留 / 去掉 nullable） | ACCEPTED |

五个问题的答案：

1. **拒的是哪个字段组合？** 是 `$ref` 被包在 `allOf` 里。`$defs` / `$ref` / nullable
   `type: [string, null]` / `$ref` 旁边挂 `description` 全都能过。唯一触发条件是
   `allOf: [{"$ref": …}]`，出现在 `PlanItem.status`。报错里的路径
   （`properties.plan.items`）指的是外层那个裸 `$ref`，比真正的位置浅一层，所以照报错
   文字猜会猜错地方。
2. **是通用 JSON Schema 违规还是只违 Moonshot 子集？** 只违 Moonshot 子集。该 schema
   是合法的 draft 2020-12，而且**无环**：`Args` → `PlanItem` → `StepStatus`（纯枚举）。
   "infinite recursion" 这个判断本身是错的；Moonshot 的校验器只在节点层解析 `$ref`，
   遇到组合关键字里的 `$ref` 就当成解析不出来。`allOf` 包 `$ref` 是 schemars 的
   draft-07 惯用写法：字段同时有 `$ref` 和文档注释时，它用 `allOf` 腾出位置放
   `description`。
3. **已有 provider capability / schema 规范化层吗？** 没有按 provider 的。
   `leveler-tools` 的 `normalize_to_draft_2020_12` 是对所有 provider 一视同仁的
   draft-07 → 2020-12 改写；`openai_chat` 适配器把 `input_schema` **原样**送上线，还有
   `tool_schema_required_reaches_the_outbound_request_verbatim` 这条契约测试守着。
4. **修复该落在哪一层？** provider / protocol 适配层，不是通用工具定义。通用 schema
   本身是对的，别的 provider 都收。最小边界是一个按 profile 开关的 compatibility 变换：
   把单元素 `allOf` 折叠回裸 `$ref`（语义等价），只对声明了该 compatibility 的 profile
   生效。
5. **会削弱其他 provider 的工具契约吗？** 折叠单元素 `allOf` 语义等价，不会。但必须按
   profile 收口：无条件全局改写会动到每个 provider 的线上字节，也会和上面那条"原样送
   达"的契约测试冲突。

记录为独立缺陷：**Moonshot 端点拒绝 `allOf` 包裹的 `$ref`，使 Kimi 模型无法加载
`update_plan`**。本轮未改动任何 provider 代码。
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
TUI_PARALLEL_DETERMINISTIC_ACCEPTANCE=PASS
TUI_PARALLEL_REAL_TRAFFIC=PASS
TUI_REAL_DOGFOOD=PASS
TUI_EXECUTION_PRESENTATION_CLOSURE=PASS
```

`TUI_PARALLEL_REAL_TRAFFIC=PASS` 立在三类证据同时具备之上，缺一不给：

1. **Runtime batch identity** — 三个 provider call ID，`parallel=true`，三个
   `tool_call_started` 全部先于任何 `tool_call_finished`（§11.1）。
2. **时间区间重叠** — 同 corpus 的 A/B 计时：254.2 ms 的扫描工作在 114.3 ms 的窗口内
   完成，至少 139.9 ms 必须并发；放大后 1.8 s 工具时间落在 0.56 s 跨度内（§11.2）。
3. **TUI 画面** — 执行中 `◌ 并行处理 3 项` 带三个 `├`/`└` 子项，完成后冻结为
   `· 并行处理 3 项`，三行各带自己的对象与结果（§11.3）。

外加反向样例：两个都可并发的顺序轮次保持成两个批次，从未出现"并行处理 4 项"（§11.4）。

仍写在限定条件里的只剩 §6.6：Partial Apply 在本 runtime 不存在，没有为它造 UI 状态。

Kimi / Moonshot 的 schema 不兼容按独立缺陷记录（§11.5），不构成本轮验收阻塞——真实并行
验收已由 DeepSeek 完成。因此不写 `ACCEPTANCE_BLOCKER=PROVIDER_SCHEMA_COMPATIBILITY`。

## 10. 产品代码之外的额外改动

- 本文档与同目录十张 dogfood 截图。
- `testdata/session_transcript.golden.json` 按新增的可选字段重新生成（上一轮）。

并行验收这一轮**没有改动任何产品代码**：`git diff --stat` 为空，HEAD 停在
`ac40def`。`parallel_tool_calls = true` 只写在一份隔离的 `LEVELER_HOME`（scratch
目录）里，用户的 `~/.leveler/config.toml` 未被触碰，仓库里的 provider capability
声明也未改动。诊断 Moonshot 时临时加过一个打印 schema 的测试，已 `git checkout` 还原。

没有新建分支。`crates/leveler-execution/src/command.rs` 的既有未提交改动未被触碰
（它在上一轮进行中由别处提交为 `0f901f0`）。
