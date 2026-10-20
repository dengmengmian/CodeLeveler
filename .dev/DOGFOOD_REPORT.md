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
| 二进制 | `CodeLeveler/target/debug/leveler`（committed tree `fd56d76`，`1.0.20 (fd56d7694dd9)`） |
| 确定性 Provider | `dogfood/eval/pty/mock_provider.py`，loopback SSE，零网络 |
| 真实 Provider | `deepseek/deepseek-v4-flash`（dogfood `secrets/codeleveler/config.toml`） |
| 证据 | `dogfood/.dev/evidence/goal_contracts_20261021.json` |
| 原始 lab | `dogfood/scratch/goal-contracts-postcommit/`（gitignored，含每个 run 的 PTY 日志与 `sessions.db`） |

运行命令：

```sh
cd dogfood
python3 eval/pty/goal_execution_contracts.py \
  --binary /Users/mengmian/Develop/app/dengmengmian/CodeLeveler/target/debug/leveler \
  --lab scratch/goal-contracts-postcommit \
  --out scratch/goal-contracts-postcommit/goal_contracts.json
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
| DF-1 显式 Goal（文本客户端） | **PASS** | `sessions.collaboration='goal'`；首请求含 `update_goal`；`/goal` 前缀不进入模型；重启后 `继续` 仍走 Goal | 8.7s |
| DF-2 阶段完成 ≠ 整体完成 | **PASS** | 5 次「本轮先到这里」后继续；共 11 轮、5 个 stage 文件、1 次 `update_goal`、0 次提前完成 | 4.2s |
| DF-3 虚假完成 | **PASS** | 真实 `verify.sh` 退出 1；`update_goal(complete)` 被 `verification_refused` 拒绝；最终 `blocked`，无 completed 终态 | 4.2s |
| DF-4 Compaction | **PASS** | 真实 Esc 中断（turn=`interrupted`，goal=`running`）→ `/compact` → `继续`；压缩后请求仍含原目标与 `update_goal` | 11.5s |
| DF-5 Resume | **PASS** | 中断后重启 + `继续`：goals 行数仍为 1，objective 保留，原文在 transcript 只出现 1 行（wire 中 2 次 = transcript + host-pinned objective） | 10.6s |
| DF-6 预算/空转 | **PASS** | 只输出阶段总结：3 次主请求后停止（初始 + 2 次有界续跑），`interrupted/incomplete`，从未 completed | 4.2s |
| DF-7 Tool Result 脱敏 | **PASS** | 第二回合 wire 保留 `github.com/dengmengmian/devorder/internal/core/errors`，不出现 `[REDACTED]` 路径；`password=[REDACTED]`、`dbname=devorder` 正常 | 8.7s |
| DF-8 Patch 一致性 | **PASS** | `-same/+same` 空补丁：工具结果含 `changed nothing`，`modified_files=[]`，文件仍为 `same\n` | 4.4s |
| DF-9 Chat 不回归 | **PASS** | 普通会话只发 1 次主请求，`completed`，0 个 goal 行 | 4.1s |
| DF-10 真实 Provider | **PASS（DeepSeek）** | 真实 `deepseek/deepseek-v4-flash`：5 轮、5 次工具调用、42,245 input / 816 output / 28,928 cached、25.2s；五项需求完成，`python3 -m unittest` 独立复核通过；模型以 `verification.status="passed"` 引用该命令并被执行记录接受 | 25.2s |

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
