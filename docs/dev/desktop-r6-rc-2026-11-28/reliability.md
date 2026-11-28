# 可靠性与 Steer 长任务（R6-A1）

日期：2026-11-28
结论：**A1 PASS（当前构建，真实 Provider）**；R5 的原始会话已删除，其 240s 超时
**无法复现，也无法确证原根因**。本轮没有发现需要修改的产品缺陷。

## 1. 方法

不是把 240s 调大，而是同时读取：

- Desktop 快照的 `task_status`；
- SQLite `turns.status`；
- `session_inputs`（steer 的 durable journal：`pending`/`consumed` + ordinal）；
- `model_requests` 数量与时间；
- `session_messages` / `events` 的类型与时间线；
- workspace 实际文件与独立 `node --test` 结果。

真实环境：真实 HOME/config/provider（`deepseek/deepseek-v4.1-flash-1017`）、真实 Runtime、
真实 Desktop、临时 workspace 与 Electron profile；测试 Session 按 id 删除；配置 SHA 前后一致。

## 2. 结果

| 项 | 结果 |
| --- | --- |
| 任务 | 读 README → `read_file arithmetic.mjs` → 写实现 → 加测试 → 跑测试 → 回答 5050 |
| Steer | 运行中追加“最终回答必须包含 `STEER_DOGFOOD_MARK`” |
| 完成时间 | **59s**（R5 观察窗 240s） |
| Steer 日志 | 创建于 `02:26:34.068Z`，`02:26:34.513Z` settled，state=`consumed`，ordinal=3 |
| Steer 投递次数 | 1（一条 journal 行，一次 settle） |
| Steer 后模型调用 | 11 次 `model_requests` |
| 工具事件 | 22 次 started/finished |
| Turn 数 | **1**（不是 post-turn 队列新回合） |
| 最终状态 | `completed`（runtime 与 DB 一致） |
| 最终答案 | 同一 assistant 回答含 5050 与 `STEER_DOGFOOD_MARK` |
| 独立验证 | `node --test arithmetic.test.mjs` → 1 passed / 0 failed，断言 `sum(1..100) === 5050` |
| 进程持续进展 | 每 5–6s 都有新 model request / progress / tool event，无静默窗口 |

完整时间线在 `evidence/r6-a1-steer-evidence.json`，截图 `evidence/r6-a1-final.png`。

## 3. 一个被自己抓到的假 PASS

第一版脚本没有把 Desktop 的“繁忙时发送行为”设为 `steer`（默认是排队）。按 Enter 后
文字被排成**第二个回合**，最终答案仍包含 marker，表面 PASS；但：

- `session_inputs` 中没有 steer 行；
- `turns` 从 1 变 2。

这正是“状态不是真相”的例子。脚本改为先写 `busySend='steer'`，并要求
`steer_state=consumed` 且 `turn_count=1` 才算 PASS。修正后的结果才是上表。

## 4. R5 原始失败

R5 报告记录：R4 的 chat+steer 在 240s 内未产生 `CHAT_DOGFOOD_DONE`，Session 随后被删除。
本轮尝试读取该 Session 的 `events`/`turns`，数据库已无记录，因此：

- 不能证明当时是产品卡死；
- 不能证明当时是脚本/队列/Provider 延迟；
- 只能确认：**当前构建、同一任务、真实 Provider 下 59s 正常完成，steer 一次投递**。

结论按证据分级：当前行为 PASS；历史事件 UNKNOWN（证据已删除）。发布说明不得把 R5
的 INCOMPLETE 改写成“已修复”，也不得把它断言为产品 BUG。

## 5. 未验证

- R5 当时的模型/provider 版本与上下文不同；模型延迟不是可复现变量。
- Windows 上的 steer 未验证。
- 超长（>15 分钟）任务的 steer 未压测；本轮任务 59s。
