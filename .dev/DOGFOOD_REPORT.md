# Dogfood 验收报告：长任务执行契约

## 结论

**PASS（10/10 用例）**，前提与边界如下：

- 确定性用例全部在真实 `leveler` 二进制 + 真实 PTY + 脚本化 OpenAI-compatible provider 上运行。
- DF-10 真实 Provider 用例使用本机已配置的 `deepseek/deepseek-v4-flash` 完成，**不是 Grok**。本机没有 Grok/xAI 的 provider 配置与凭据，因此“Grok 真实长任务”这一项是 **NOT_VERIFIED**，不能算 PASS。
- 本轮没有 SKIPPED 用例；DF-10 的 Grok 子项单独标注为 NOT_VERIFIED（见文末）。

## 环境与身份

| 项 | 值 |
|---|---|
| 平台 | macOS 26.6（darwin） |
| 二进制 | `CodeLeveler/target/debug/leveler`（committed tree `2c5c997`，`1.0.20 (2c5c99770d46)`） |
| 确定性 Provider | `dogfood/eval/pty/mock_provider.py`，loopback SSE，零网络 |
| 真实 Provider | `deepseek/deepseek-v4-flash`（dogfood `secrets/codeleveler/config.toml`） |
| 证据 | `dogfood/.dev/evidence/goal_contracts_20261021.json` |
| 原始 lab | `dogfood/scratch/goal-contracts-head/`（gitignored，含每个 run 的 PTY 日志与 `sessions.db`） |

运行命令：

```sh
cd dogfood
python3 eval/pty/goal_execution_contracts.py \
  --binary /Users/mengmian/Develop/app/dengmengmian/CodeLeveler/target/debug/leveler \
  --lab scratch/goal-contracts-head \
  --out scratch/goal-contracts-head/goal_contracts.json
```

退出码 0；报告 `counts = {PASS: 10, FAIL: 0, SKIPPED: 0, NOT_VERIFIED: 0}`。

## 测试层级

| 层级 | 本轮内容 | 说明 |
|---|---|---|
| 确定性单元测试 | `leveler-core` 脱敏、`leveler-tools` patch、`leveler-agent` 完成判定纯函数、`leveler-client-protocol` `/goal` 解析 | `cargo test --workspace` 全绿 |
| Runtime 集成测试 | `leveler-agent/tests/direct_test.rs`：引用未运行命令被拒、失败验证不能完成；`leveler-app/tests/collaboration_entry.rs`：`SubmitMessage("/goal …")`、运行中 `SteerCurrentTurn("/goal …")` 升级同一回合 | 真实 Runtime，脚本模型 |
| 真实 CLI / PTY | DF-1 … DF-9 | 真实 `leveler` TUI，真实终端，脚本 Provider |
| 真实 Provider | DF-10 | 真实 DeepSeek API，真实 TUI；非 Grok |

## 用例矩阵

| 用例 | 结果 | 实测证据 | 耗时 |
|---|---|---|---:|
| DF-1 显式 Goal（文本客户端） | **PASS** | `sessions.collaboration='goal'`；首请求含 `update_goal`；`/goal` 前缀不进入模型；重启后 `继续` 仍走 Goal | 10.4s |
| DF-2 阶段完成 ≠ 整体完成 | **PASS** | 5 次「本轮先到这里」后继续；共 11 轮、5 个 stage 文件、1 次 `update_goal`、0 次提前完成 | 4.2s |
| DF-3 虚假完成 | **PASS** | 真实 `verify.sh` 退出 1；`update_goal(complete)` 被 `verification_refused` 拒绝；最终 `blocked`，无 completed 终态 | 4.2s |
| DF-4 Compaction | **PASS** | 真实 Esc 中断（turn=`interrupted`，goal=`running`）→ `/compact` → `继续`；压缩后请求仍含原目标与 `update_goal` | 11.4s |
| DF-5 Resume | **PASS** | 中断后重启 + `继续`：goals 行数仍为 1，objective 保留，原文在 transcript 只出现 1 行（wire 中 2 次 = transcript + host-pinned objective） | 10.6s |
| DF-6 预算/空转 | **PASS** | 只输出阶段总结：3 次主请求后停止（初始 + 2 次有界续跑），`interrupted/incomplete`，从未 completed | 4.2s |
| DF-7 Tool Result 脱敏 | **PASS** | 第二回合 wire 保留 `github.com/dengmengmian/devorder/internal/core/errors`，不出现 `[REDACTED]` 路径；`password=[REDACTED]`、`dbname=devorder` 正常 | 8.7s |
| DF-8 Patch 一致性 | **PASS** | `-same/+same` 空补丁：工具结果含 `changed nothing`，`modified_files=[]`，文件仍为 `same\n` | 4.2s |
| DF-9 Chat 不回归 | **PASS** | 普通会话只发 1 次主请求，`completed`，0 个 goal 行 | 4.1s |
| DF-10 真实 Provider | **PASS（DeepSeek）** | 真实 `deepseek/deepseek-v4-flash`：7 轮、7 次工具调用、61,819 input / 1,355 output / 46,592 cached、27.3s；五项需求完成，`python3 -m unittest` 独立复核通过；模型以 `verification.status="passed"` 引用该命令并被执行记录接受 | 27.3s |

### DF-10 的 Grok 子项

**NOT_VERIFIED。** 本机 `~/.leveler/config.toml` 与 dogfood `secrets/codeleveler/config.toml` 均无 Grok/xAI provider，环境也没有对应 key。没有执行 Grok 真实长任务，因此不能报告 Grok PASS。DF-10 的 PASS 仅指上表中的 DeepSeek 真实 Provider 结果。

## 关键证据摘录

### DF-2：阶段总结没有结束目标

```text
rounds=11  tools=6  goals=1  intercepts=0  outcome=completed
stage files: stage-1.txt … stage-5.txt 全部存在
```

第 1、3、5、7、9 轮都是「第 N 阶段完成，本轮先到这里」；运行时的 closeout 继续，第 11 轮才出现 `update_goal(complete)`。

### DF-3：失败验证不能被 `not_applicable` 覆盖

```text
rounds=3  tools=3  intercepts=1  outcome=blocked
goal_intercepted: verification_refused
```

### DF-5：同一个 Goal 行

```text
DF5-1: turns=1 rounds=3 outcome=interrupted
DF5-2: turns=2 rounds=4 goals=1 outcome=blocked
goals: 1 行，objective = 原始任务
```

### DF-7：持久化重放不再污染路径

第二回合的 wire request：

```text
github.com/dengmengmian/devorder/internal/core/errors   ← 完整保留
password=[REDACTED]                                      ← 凭据值仍脱敏
dbname=devorder                                          ← 非凭据值保留
无 github.com/dengmengmian/[REDACTED]
```

### DF-10：真实模型引用真实验证

工具调用参数（来自 `sessions.db` 的 `tool_call_started`）：

```json
{"status":"complete","summary":"Implemented all five requirements in app.py; unittest passes.","verification":{"detail":"python3 -m unittest","status":"passed"}}
```

终态 `task_finished`：`outcome=completed, stop=completed`，无 completion warning。

## 产品回归测试

本轮同时把曾经失败的基线测试恢复为绿色：

| 问题 | 修复 |
|---|---|
| `envelope_wire_format_is_stable` 仍期待协议 minor 14，实际 15 | 更新 golden fixture 到 15 |
| 大段粘贴的 `/goal …` 被 `[Pasted: N lines]` 占位符遮蔽，退化成 Chat | `submit.rs` 用 canonical 文本识别命令；新增 `slash_goal_with_large_paste_still_runs_a_goal` |
| `active_goal` 头部断言期待完整标题，实际头部会按宽度截断 | 断言按当前头部截断契约（glyph + 稳定前缀）；窄终端不再要求被丢弃的 mode 段 |
| `tab_toggles_workbench_focus...` 期待 Chat 会话有 Goal 焦点站 | 改为 Chat 无 Goal 站；新增 Goal 模式下 Tab 到达 Goal 站的测试 |

`cargo test --workspace`：**全部通过**（含所有新增回归）。

## 未覆盖 / 残余风险

1. **Grok 真实长任务：NOT_VERIFIED**（无 provider/凭据）。
2. **并发 Patch 写入**：由现有 `apply_patch` CAS 单元测试覆盖，不在 DF 的 PTY 路径里重复构造；DF-8 的 PTY 用例只证明空补丁语义。
3. **子 Agent 的验证不计入父 Goal**：见 `GOAL_EXECUTION_CONTRACT.md` 第 8 节。
4. **运行中 `/goal` 的声明窗口**：轴先持久化，声明文本下一轮落库；进程恰在窗口崩溃会丢失声明文本。
5. **真实 Provider 的任务规模**：DF-10 是 5 个需求、5 个模型轮的短任务；长回合路径的证据来自 DF-2（11 轮）与 DF-6（有界空转），不是同一个真实模型会话。

---

# 第二轮：生命周期收口（2026-10-21）

## 结论

**PASS。** 确定性套件 DF-1～DF-10 全部通过（10/10），真实 DeepSeek 的 Blocked→`继续`
重试用例（DF-11）通过。本轮修复的三个生命周期问题都有红→绿测试锁住，并在最终二进制上
复跑。

## 环境与身份

| 项 | 值 |
|---|---|
| 二进制 | `target/debug/leveler`，sha256 `ff898013bd4619d275bc0117ae249926dd85f5a700249cdd3688214bc4f00f84` |
| 版本串 | `1.0.20 (9a4da5477370-dirty) UNTRUSTED: built from a modified working tree` |
| 树状态 | HEAD `9a4da54` + 本轮未提交改动（工作树同时含其他终端的未提交改动） |
| 确定性 lab | `dogfood/scratch/goal-contracts-final3/run-20261021-105501` |
| 确定性结果 | `dogfood/scratch/goal-contracts-final3/goal_contracts.json`（counts = {PASS: 10}） |
| 真实 Provider lab | `dogfood/scratch/df11-real/run-20261021-105626` |
| 真实 Provider 结果 | `dogfood/scratch/df11-real/df11_result_final2.json` |

## 用例

| 用例 | 结果 | 证据 |
|---|---|---|
| DF-1 显式 Goal（文本客户端） | **PASS** | 首个回合 `blocked`；重启后 `继续` **重试同一个 goal**：`goals=1`、objective 原样、`继续` 未成为目标；wire 中原目标出现 2 次（transcript + host-pinned） |
| DF-2 阶段完成 ≠ 整体完成 | PASS | 5 次阶段总结后继续，1 次 goal resolution |
| DF-3 失败验收不能 complete | PASS | `verification_refused`，终态 blocked |
| DF-4 Compaction | PASS | `/compact` 后目标与 Goal 工具面仍在 |
| DF-5 Esc 中断后 Resume | PASS | 同一 `goals` 行，objective 保留 |
| DF-6 空转有界 | PASS | 3 次主请求后 incomplete，不是 completed |
| DF-7 脱敏 | PASS | 首窗 wire + 持久 transcript 保留源码路径；凭据 `[REDACTED]` |
| DF-8 空补丁 | PASS | `changed nothing`，文件未改 |
| DF-9 Chat 不回归 | PASS | 1 次请求结束，0 个 goal |
| DF-10 真实 Provider（DeepSeek） | PASS | `deepseek/deepseek-v4-flash`，独立 `python3 -m unittest` 通过 |
| **DF-11 Blocked 后真实重试（DeepSeek）** | **PASS** | 第一窗模型主动 `update_goal(blocked)`；`继续` 后同一 goal id、原 objective、`completed`；5/5 测试通过 |

### DF-1 / DF-7 断言为什么改了

这两条在第一轮是按旧行为写的：Blocked 不是可恢复终态，所以 `继续` 会新建一个
objective 为「继续」的 goal。本轮把 Blocked 改成可重试后：

- DF-1 的正确断言变成「同一 goal、原 objective、`继续` 不是 objective」；wire 中目标
  出现 2 次是 transcript + host-pinned objective，DF-5 早已按这个上限断言。
- DF-7 的脱敏不变量改在 **持久 transcript + 首窗 wire** 上断言。长 goal 的 retry 可能
  用 goal checkpoint 折叠旧上下文，旧 tool result 不再出现在 wire 里；这不是脱敏失败，
  用 retry 的 wire 断言会把「正常折叠」误判成「路径丢失」。

## 真实 DeepSeek 的生命周期证据（DF-11）

```text
turn 1: user  goal_id=G  completed(blocked)   goals: 1 行  objective=原目标
turn 2: user  continuation_root=G  goal_id=G  completed         goals: 1 行  settled
task_finished #1: goal {goal_id:G, settle:true} outcome=blocked
task_finished #2: goal {goal_id:G, settle:true} outcome=completed
independent: Ran 5 tests ... OK
```

第二个回合的 turn payload 直接携带 `goal_id=G`，证明重试走的是 resume 身份绑定，而不是
按文本新建目标。

## 新增红→绿回归

| 测试 | 锁住的行为 |
|---|---|
| `a_goal_declaration_is_atomic_with_the_axis_and_cleared_by_axis_changes` | 轴与声明同一写；CAS 清空；轴变更清空 |
| `a_busy_goal_declaration_is_durable_with_the_axis` | 运行中 `/goal`：执行器 drain 之前，轴+目标已在库中；回合终态结算该 goal |
| `a_pending_declaration_is_recovered_by_continue` | 崩溃窗口后 `继续` 以声明文本为目标，模型真的收到它 |
| `continue_after_a_blocked_goal_retries_the_same_record` | Blocked→`继续` 重试同一 goal，原 objective 不变 |
| `an_explicit_new_goal_after_blocked_stays_independent` | 显式 `/goal` 仍是独立新目标 |
| `a_mid_turn_upgrade_resumes_as_its_goal` | 运行中升级后中断，`继续` 以 Goal profile 恢复同一 goal |
| `verification_facts_survive_the_next_work_window` | 上一窗口成功的验收命令可被新窗口引用 |
| `a_failed_verification_survives_into_the_retry` | 上一窗口失败的验收命令仍阻止 premature complete |

敏感性检查：临时移除 resume/retry 的验证事实种子后，后两条分别转红（预算耗尽 /
premature complete 被接受），恢复后转绿。

## 回归

- `cargo test --workspace`：**exit 0**，262 个测试二进制全部通过。
- `cargo fmt --all -- --check`：通过。
- `cargo clippy --workspace --all-targets`：本轮新增代码无告警；剩余告警在
  `leveler-execution/background.rs`、`leveler-storage/session_facts.rs`，均为既有代码。

## 已知抖动（不作为 PASS 依据）

全量套件在最终二进制上的一次运行里，DF-10 出现过一次 **提交未发生** 的 PTY 抖动：
4.2s 后 session 仍为 `created`，composer 里保留着输入的 `/goal …` 文本，没有 goal 行或
`task_finished`。单独重跑 DF-10 与紧随其后的全量重跑均 PASS。该现象发生在输入被
Runtime 接受之前，与本轮生命周期改动无关；记录在此，避免把一次 flake 混进结论。

---

# 第三轮：Final Hardening（2026-10-21）

## 结论

**PASS（12/12 dogfood 用例；全量 workspace 测试见文末）。** 本轮收口两个已确认的
Runtime 边界，并新增永久回归：

| 问题 | 结果 | 锁住它的测试 |
|---|---|---|
| Chat 运行中升级 Goal 后，当前窗口的模型消耗未计入 Goal 预算 | 已修复 | `chat_goal_budget_test`（4 个）、DF-12（PTY + 真实 SQLite） |
| Goal 已创建、TurnStart 未落库时崩溃会留下无人能认领的 Running goal | 已修复 | `goal_crash_recovery_test`（5 个）、DF-13（PTY + 真实 SQLite） |
| 在“被 resume 过的 Chat 窗口”中声明的 Goal，谱系根指向旧 Chat 窗口 | 已修复（DF-13/测试暴露） | `a_goal_declared_in_a_resumed_chat_window_keeps_its_own_lineage` |
| Resume 时从 pending 声明收养 Goal，checkpoint 根校验失败导致 Blocked 被写成 Interrupted | 已修复（DF-13 暴露） | `a_pending_goal_adopted_by_resume_roots_its_checkpoint_at_its_own_turn` |

## 预算归属（Phase 1）

| 事实 | 归属 |
|---|---|
| 升级前已写入的 `model_requests`（Chat turn/lineage scope） | `GoalBinder` 在绑定后执行一次 `UPDATE ... SET budget_scope = goal_id`（移动，不复制） |
| 升级后的主回合、子 Agent、harness reviewer | 执行器按 `GoalBinder::bound()` 动态取 scope |
| `ProgressLedger` 的 model steps / duration | 绑定当轮在 `on_round_start` 把 ledger 换到 goal scope 并持久化 |
| 崩溃后恢复 | `resume_inner` 扫描已绑定该 goal 的 Chat turn，按其 WAL payload 推导 scope（fresh=自身 id，resumed=chat root），幂等迁移 |

确定性测试使用真实 SQLite 与真实引擎；cap 设为 100,000，升级窗口已经消耗 980,000
时，第二次 Resume **不再发出任何模型请求**并返回 `BudgetExhausted`，goal 保持 `Running`
（不伪装 Completed）。预算耗尽、Blocked、Completed、Cancelled 的映射沿用既有终态。

## Goal 创建窗口（Phase 2）

顺序改为：

1. 没有 pending 声明时，先以 `set_pending_goal_if_absent` 把本次目标写成声明（CAS，较新的
   `/goal` 不会被覆盖）；
2. 打开/复用 goal；
3. `start_owned_consuming_declaration` 在 **TurnStart 的同一事务**里插入 turn 行并 CAS
   清空声明。

因此每个崩溃窗口都留下“声明 + 至多一个 Running goal”：
- 声明后、goal 前崩溃：下一个 Goal 回合按声明打开 goal；
- goal 后、turn 前崩溃：下一个 Goal 回合按声明复用同一个 goal；
- 绑定与迁移之间崩溃：`resume` 重放预算迁移；turn payload 的 `goal_id` 是身份。

新测试覆盖：Goal 创建后崩溃、绑定后无终态、恢复期间再次崩溃、多次 Resume、并发两个
Session 恢复；每项都验证 goal 数量/id/objective、`pending_goal`、turn lineage、
goal status 与是否重复执行。

## Dogfood

| 用例 | 结果 | 证据 |
|---|---|---|
| DF-1 … DF-10 | **PASS（10/10）** | 真实 TUI + 真实 PTY；DF-10 为真实 DeepSeek |
| DF-12 Chat→Goal 预算归属 | **PASS** | 升级窗口 2 条 main-round 行的 scope 全部 = goal id；首请求无 `update_goal`，升级后请求有 |
| DF-13 创建窗口崩溃恢复 | **PASS** | 收养同一 goal id 与 objective；声明被消费；turn payload goal_id 一致；goal 只有一个 |
| DF-11 Blocked→继续（真实 DeepSeek，单独复跑） | **PASS** | 第一窗口 blocked；`继续` 恢复同一 goal id、objective 不变；完成且独立 `python3 -m unittest` 通过 |

详情：`dogfood/.dev/GOAL_CONTRACT_DOGFOOD.md`，冻结证据
`dogfood/.dev/evidence/goal_contracts_20261021_final_hardening.json`。

## 性能与成本（DF-10 真实 Provider）

| 指标 | 值 |
|---|---:|
| Model rounds | 5（另 1 次 prompt suggestion） |
| Input / Output / Cached | 42,120 / 622 / 28,928 |
| Reasoning tokens | 162 |
| Cost | 6,023 micro-USD（约 $0.006） |
| Wall time | 22.2s |
| Compaction / Resume / 重复执行 | 0 / 0 / 0 |

本轮修复没有增加模型调用：预算迁移是数据库 UPDATE；崩溃恢复是已有 turn 的复用。
DF-13 恢复后只执行 1 个 Goal turn，独立检查 `python3 -m unittest` 通过。

## 残余风险

1. **Grok 真实长任务：NOT_VERIFIED**（本机无 provider/凭据），与上一轮相同。
2. **真实配置 TUI 启动窗口的输入时序**：DF-10 两次因“启动首帧后立即写入长 prompt+Enter”
   而未提交（会话停在 `created`，0 模型请求）。这是输入层时序，不是 Goal 执行失败；
   dogfood 驱动改为 2.5s 启动宽限 + 文本/Enter 分开发送。TUI 输入代码属于并行终端工作区，
   本轮未改。
3. **旧版本无 scope 的历史行不回填**：本次迁移只作用于绑定窗口仍在 `model_requests` 中的
   行；已发布版本留下的无 scope 行沿用既有“拒绝在 cap 下恢复”的保守策略。
4. **checkpoint 根与 turn WAL 根可能不同**：Resume 收养 pending Goal 时，里程碑 checkpoint
   以“命名该 goal 的 turn”为根，而 turn payload 仍记录它续接的 Chat 根。两者都指向同一
   goal；不再导致 checkpoint 失败，但首个收养窗口内切出的 checkpoint 不会被下一次
   resume 命中（回退到完整历史，安全）。

## 2026-10-21 · Chat 显式委托（DF-14 … DF-20）

### 结论

**PASS（19/19 永久套件）**。新增 DF-14～DF-20 全部通过；DF-1～DF-13 全量复跑通过；
DF-11（真实 DeepSeek，Blocked→`继续`）单独复跑通过。

最终证据：`dogfood/.dev/evidence/goal_contracts_20261021_chat_delegation.json`、
`dogfood/.dev/evidence/df11_blocked_retry_20261021_chat_delegation.json`。

**二进制身份**：`1.0.20 (e2c90925c093-dirty) UNTRUSTED: built from a modified working tree`。
工作树同时包含另一会话的内存/桌面 WIP，因此本轮证据不是干净提交构建；提交后应复跑。

### 分层

| 层 | 用例 | 说明 |
|---|---|---|
| 真实 TUI/PTY + 脚本 Provider | DF-1…DF-9、DF-12…DF-19 | 真实终端、真实 SQLite、真实工具执行 |
| 真实 Provider | DF-10、DF-20 | `deepseek/deepseek-v4-flash`；DF-20 不使用 `/goal` |
| 单独真实 Provider | DF-11 | Blocked→`继续` 同一 goal id |

### 新增用例

| 用例 | 结果 | 证据 |
|---|---|---|
| DF-14 普通 Chat 不委托 | **PASS** | 1 次主请求；`goals=0`；`answered`；无续跑 |
| DF-15 Chat 明确委托长任务 | **PASS** | 1 次用户提交、9 次主请求、6 次工具调用、3 个 stage 文件、1 个 goal；`collaboration=chat`；turn kind=`chat` 且 payload `goal_id` 绑定；所有 main-round scope = goal id；首请求有 `commit_task` 无 `update_goal`，后续请求有 `update_goal`；终态 `completed`；任务完成后一条普通 Chat 消息只发 1 次请求、不新建 goal |
| DF-16 阶段性 Final | **PASS** | 阶段总结后出现 `goal_unresolved` closeout 注入并继续；最终 `completed`，goal settle 一次 |
| DF-17 讨论/只读保护 | **PASS** | 1 次请求；写工具调用 0；工作区无 stage 文件；`goals=0` |
| DF-18 Chat 长任务恢复 | **PASS** | Esc 中断后重开 + `继续`：同一 goal id 与 objective；stage-1 保留；budget scope 不变；`completed` |
| DF-19 取消与有界 | **PASS** | `/goal clear` 后 `outcome=cancelled`、goal settled；后续 `继续` 0 次模型请求、不新建 goal；纯阶段总结的任务 ≤5 次主请求后停止，goal 仍 running、无 completed 终态 |
| DF-20 真实 Provider 长任务 | **PASS** | 1 次用户提交；真实模型调用 `commit_task` 1 次；3 个 stage 文件；独立 `python3 -m unittest` 通过；`completed`；会话轴仍为 chat；8 个 main round |

### 性能（新增用例）

| 指标 | DF-14 普通 Chat | DF-15 脚本长任务 | DF-20 真实长任务 |
|---|---:|---:|---:|
| 用户提交 | 1 | 1 | 1 |
| 主模型请求 | 1 | 9 | 8 |
| Tool calls | 0 | 6 | 8 |
| Input / Output / Cached | 2 / 128 / 0（脚本 usage） | 0 / 0 / 0（脚本 usage） | 80,637 / 1,974 / 58,368 |
| 估算输入 tokens | 10,108 | 98,590 | — |
| Cost | 0 | 0 | 11,749 micro-USD（约 $0.012） |
| Wall time | 4.1s | 6.4s | 43.6s |
| 提前 Final / 无效续跑 | 0 / 0 | 0 / 0 | 0 / 0 |
| Compaction / Resume | 0 / 0 | 0 / 0 | 0 / 0 |
| Goal 行 | 0 | 1（settled） | 1（settled） |

固定成本：`commit_task` schema 356 tokens + Chat 委托规则 190 tokens = **546 tokens/请求**，
二者都在会话前缀中缓存；普通 Chat 的模型请求数仍为 1，没有新增分类调用。

### 已知抖动

DF-10 在全量套件的一次运行中因真实配置 TUI 启动窗口（会话停在 `created`、0 模型请求）
失败；单独复跑 PASS，随后全量复跑 PASS。与上一轮记录的输入时序抖动相同，归因于驱动层，
不是 Goal 或 Chat 委托契约失败。

### 残余风险

1. 是否调用 `commit_task` 仍是模型判断；脚本 Provider 证明了契约，真实 Provider 证明了
   一次正向行为，但不能保证所有模型在所有请求上都会主动声明。
2. `acceptance` 是模型声明，不是机械判据；完成仍由 `update_goal` 的证据检查决定。
3. Chat 起步回合中途绑定 goal 时，本轮内折叠不会生成 goal checkpoint（既有缺口）；
   回合结束后的 resume 仍按 `turn.payload.goal_id` 恢复。
4. 工作树包含并行 WIP，证据二进制标记为 UNTRUSTED。
