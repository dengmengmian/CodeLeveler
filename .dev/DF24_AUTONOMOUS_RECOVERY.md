# DF-24 产品侧：完成被拒后的有界恢复

本文只记录 CodeLeveler 侧的缺口、根因、修复与确定性验证。端到端验收报告在
Dogfood 仓库 `.dev/DF24_AUTONOMOUS_RECOVERY.md`。

## 1. 结论

| | |
|---|---|
| 缺口 | 重复被拒的 `update_goal(complete)` 不计入空转，`UntilTerminal` 下可无限重试 |
| 修复 | 完成门拒绝视为「未产生持久效果的调用」，复用既有的有界空转规则 |
| 已有机制 | 拒绝反馈、逐项 accounting、范围权威、恢复/压缩读取持久 Goal 行**均已正确**，未改动 |
| 验证 | 6 个新增集成测试 + 1 个提取单元测试；`leveler-agent` 全量测试通过 |

DF-22 已经证明「模型不能缩窄范围」，但没有证明「拒绝之后能恢复」。本轮证明：
恢复链路本身在现有 Runtime 中已经成立；真实缺口只有**无进展拒绝不受既有空转上限约束**。

## 2. Red：先复现

测试（`crates/leveler-agent/tests/direct_test.rs`）：

```text
repeated_refused_completions_are_bounded
```

脚本 Provider 连续三次提交同一个只记 stage-1 的 `update_goal(complete)`，
Goal objective 明确枚举 `stage-1.txt、stage-2.txt、stage-3.txt`。

修复前的结果（原文）：

```text
thread 'repeated_refused_completions_are_bounded' panicked at
crates/leveler-agent/tests/direct_test.rs:4412:10:
called `Result::unwrap()` on an `Err` value: Execution { detail: "model error [Other]: no more responses", ... }
test result: FAILED
```

含义：每次拒绝都被当成「有进展」（`note_progress` 重置空转计数），运行一直
走到脚本响应耗尽。真实 Provider 下 `UntilTerminal` 没有回合上限，只会由
token / cost / 用户取消终止，而不是由既有的 `no_progress_rounds = 2`。

## 3. 根因

`crates/leveler-agent/src/executor/drive.rs::execute_calls` 只在**执行前**被
guard 拒绝的调用上累加 `denied_calls_this_round`（budget、allowlist、权限）。
`update_goal` 的完成门拒绝发生在工具分发之后：

| 拒绝 | 事件 kind |
|---|---|
| 缺 status / status 非 complete\|blocked | `invalid_status` |
| `verification` 结构非法 | `invalid_verification` |
| 验证事实不成立（失败、未跑、改动后过期） | `verification_refused` |
| `requirements` 结构非法 | `invalid_requirements` |
| 用户范围未逐项记账 / 产物不存在 / 命令未跑 | `requirements_refused` |

这五种都不产生任何持久副作用，但都没有计入 `denied_calls_this_round`，
因此这一轮被 `note_progress` 记为有进展。

## 4. 修复（最小）

1. 五个拒绝分支各增加一次 `denied_calls_this_round += 1`。
2. 更新该计数的注释：它表示「没有产生持久效果的调用」，包含执行前拒绝与
   完成门拒绝。
3. 空转硬停的 `stop_detail` 改为
   `no-progress streak; model steps that only repeated refused calls short-circuited`
   （TUI 只依赖 `no-progress streak` 前缀，未受影响）。
4. `leveler-lifecycle/src/progress.rs` 的 `ProgressCaps` 注释同步该输入。

没有新增状态机、没有新的阈值、没有 LLM judge、没有关键词判断：

- 两条连续被拒 → 由既有 `no_progress_rounds = 2` 停机，Goal 保持 owed。
- 中间有真实工作（文件写入、命令执行）→ `note_progress` 重置，恢复不受影响。
- 被接受的 `complete` / `blocked` → 直接结算，不进入该计数。
- 因后台子代理未结算而拒绝的 `outstanding_children` **不**计入：子代理结算
  是真实工作，下一轮有新的持久事实。

## 5. 确定性测试

文件：`crates/leveler-agent/tests/direct_test.rs`（复用真实 `CodingRuntime`、
真实 SQLite、真实工具与权限管线）。

| 测试 | 证明 |
|---|---|
| `a_refused_premature_completion_recovers_within_the_same_run` | 只交付 stage-1 → 被拒（点名 stage-2/3）→ 同一回合补齐 → 真实验收脚本 → 只完成一次 |
| `a_false_delivery_claim_is_refused_until_the_work_and_check_exist` | 谎称三个文件都存在 → 拒绝（不存在）；文件补齐但脚本没跑 → 再拒；跑过脚本后才接受 |
| `a_failed_acceptance_script_blocks_completion_until_it_really_passes` | 内容错误 → 脚本真实失败 → 拒绝完成；修正后重跑 → 完成 |
| `repeated_refused_completions_are_bounded` | 两次相同拒绝后由空转上限停机，第三次脚本响应从未被请求 |
| `a_compacted_context_still_owes_the_users_full_scope` | 真实 fold（summary 请求 + fold 提交）后，第二次缩窄记账仍被拒；fold 发生在第二次尝试之前 |
| `a_refused_completion_survives_a_process_restart_and_completes_on_resume` | 拒绝后进程取消；新引擎 resume 同一 goal id；transcript 仍带拒绝与原始 objective；补齐后完成 |
| `the_df24_recovery_prompt_yields_three_paths_and_one_command`（unit） | DF-24 原文只提取 3 个路径 + 1 个命令，不凭空制造要求 |

压缩变体用 `context_soft_percent = 1` 触发真实折叠（`tool_choice = None` 的
summary 请求由 mock 应答），不是伪造的「已压缩」状态。

## 6. 验证

| 命令 | 结果 |
|---|---|
| `./dev fmt`（本次修改的 Rust 文件） | PASS |
| `./dev verify`（本次修改的 Rust 文件） | PASS（fmt / check / test / version） |
| `env -u NODE_OPTIONS cargo test -p leveler-agent` | 全部 target 通过（含 direct_test 61、loop_test 132、task_commit 12、goal_crash_recovery 5、multi_turn 18、multi_agent 90） |
| clean candidate `cc91c2d`：`cargo fmt --all -- --check` | PASS |
| clean candidate `cc91c2d`：`cargo test --workspace --all-features --locked --no-fail-fast` | **6,961 passed / 0 failed / 59 ignored**（269 个 target），exit 0 |
| clean candidate `cc91c2d`：`cargo clippy --workspace --all-targets --all-features -- -D warnings` | FAIL，但只在 `leveler-agent/src/experience.rs`（并行 memory 会话新增文件：`if_same_then_else`、`items_after_test_module`），不是 DF-24 |
| 当前 HEAD `ca1145b`：同一 clippy 命令 | **PASS**（并行会话已在 `a6ffd15` 修复 experience 的两个 lint；`ca1145b` 相对本候选只多 memory 与 desktop 改动） |

## 7. 未改动与边界

- `goal_scope` 提取规则、`GoalStore`、`consume/amendments`、`update_goal` 的
  拒绝文案均未改：DF-24 的恢复链路复用它们。
- 拒绝仍是反馈，不是终止条件：只限制「连续没有持久效果的重复」。
- 语义性交付仍只能由模型的 `unverifiable` 边界承认，不会被包装成机械验证。
- 空转上限仍是 2，没有为本轮调参。
