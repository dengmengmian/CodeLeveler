# 新版 Memory 可靠性（R6-A4）

日期：2026-11-28
结论：**核心 PASS（22/22 集成测试 + R6-0 真机 16 项）**；A4 全矩阵 **PARTIAL**。

## 1. 系统边界

现行记忆是 Runtime 自有：capture → observation → Dream → topic + 有界 prompt index；
Markdown + SQLite ledger；Desktop/TUI/CLI 读同一 store，不另建记忆中台。
R6-0 已完成四个 Desktop 命令的接入，本轮没有改记忆架构。

## 2. 本轮验证

`cargo test -p leveler-app --test memory_integration` → **22 passed / 0 failed**。

覆盖到的真实行为（模型为录制 mock，store/文件/游标为真实实现）：

| 场景 | 测试 |
| --- | --- |
| 每轮稳定索引、topic 可读 | `active_memory_injects_a_stable_index_once_per_turn`、`a_seeded_topic_is_listed_and_readable_by_the_model` |
| 显式 `/remember` 跨 Session | `explicit_remember_survives_into_a_new_session` |
| 非空 capture → durable observation | `a_settled_turn_is_captured_into_a_durable_observation` |
| **非空 workspace → 手动 Dream 生成 topic** | `a_manual_dream_consolidates_captured_observations_into_a_topic` |
| 批量 Dream 的逐 note 结算 | `batch_dream_settles_each_note_with_typed_edits` |
| 自动 Dream 触发 | `an_automatic_dream_runs_when_the_oldest_pending_observation_is_old` |
| capture/Dream provider 失败与重试 | `a_capture_provider_failure_is_retried_and_never_breaks_the_turn`、`a_dream_provider_failure_keeps_the_observations_pending` |
| 损坏 DB 降级 | `a_corrupt_memory_database_degrades_the_agent_without_breaking_it` |
| 冲突事实只通过 claimed evidence 解决 | `a_conflicting_fact_is_resolved_only_through_claimed_evidence` |
| 重启后从 durable cursor 续跑、不重复捕获 | `a_restarted_runtime_resumes_the_capture_cursor_without_recapturing` |
| 崩溃/中断的 capture job 恢复 | `a_capture_interrupted_by_restart_is_recovered_from_its_durable_job` |
| fork/parallel Session 不互相覆盖 | `a_forked_session_keeps_memory_but_captures_only_its_own_turns`、`parallel_sessions_capture_into_one_store_without_losing_each_other` |
| `/compact` 不重置注入与捕获进度 | `compaction_does_not_reset_memory_injection_or_capture_progress` |
| 中断 turn 不伪造记忆 | `an_interrupted_turn_settles_its_capture_range_without_inventing_memory` |
| 临时 workspace 关闭 pipeline | `a_temporary_workspace_keeps_the_automatic_pipeline_off` |
| 会话级 pause 只影响本会话 | `the_session_toggle_pauses_memory_for_this_session_only` |
| 退出释放 lease、重启续跑 | `quitting_releases_the_capture_lease_this_process_owns`、`a_restarted_process_resumes_the_cancelled_capture_without_a_new_turn` |
| 正常退出不耗尽重试预算 | `normal_quits_do_not_exhaust_the_capture_retry_budget` |
| 初始化幂等/并发 | `leveler-memory` 的 `scope_tests`、`capture_store_tests`、`consolidation_tests`（lease、崩溃、tombstone） |

## 3. R6-0 真机结果（沿用）

真实 HOME/config/provider/Runtime/Desktop：

- `/remember` 写入唯一全局 note，文件字节=输入文本；
- Desktop 页面与 CLI 读取同一 note；
- `/dream`、`/flush` 走真实 Runtime，消息为 runtime 原文；
- 重启后 note 仍可列出/预览；
- 通过页面遗忘后文件消失，CLI 不再列出，其他全局记忆文件逐字节不变；
- 16 项检查全部 PASS。

## 4. 未覆盖 / 风险

| 项 | 状态 |
| --- | --- |
| 非空 capture 队列的 `/flush` | 未能确定性构造：自动 capture worker 会先消费；集成测试覆盖了 capture 与 flush 的下层，但没有一个“flush 独占非空队列”的产品级测试 |
| Desktop + TUI 同时写同一 scope 的压测 | 只验证了 Desktop 打开时 CLI 读取一致；未做双写压力 |
| 并发 `/remember` | 下层 SQLite/lease 有并发测试；产品级并发命令未压测 |
| 并发 Dream | `dream_lock`/lease 有测试；两个客户端同时发 `/dream` 的 UI 级结果未压测 |
| 遗忘与读取竞争 | tombstone + read ledger 有单测；真实 UI 竞争未跑 |
| 35 分钟“结果未知”边界 | 显示层设计存在，未实测超时 |
| rollout=off | 集成测试验证 memory off 不注入/不捕获/不建目录；`/remember` 仍可写（产品契约） |
| pause 不得提升 off | 配置/会话 pause 有测试；off→pause→resume 的完整矩阵未单独跑 |
| 跨 workspace 召回 | scope 隔离有单测；真实两 workspace 互查未跑 |
| Windows | NOT VERIFIED |

## 5. 结论

Memory 的核心持久化、Dream、恢复、隔离与失败语义有真实测试支撑；R6-0 的真机命令接入
通过。A4 要求的并发/竞争/35 分钟边界与“非空 flush”没有全部实测，不能宣称 A4 全量 PASS。
