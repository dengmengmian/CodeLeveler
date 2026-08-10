# C4 Self-Healing / Long Task Reliability Baseline

日期：2026-08-10。分支 `feat/coding-self-healing-c4`。**生产代码零改动。**

**结论前置：D1–D8 全 PASS；benchmark 8/8 VALID（含 recovery baseline shape 证明）；
24/24 functional PASS；recovery rate 100%（21/21）；FalseCompletion 0；UnboundedRecovery 0；
DuplicateSideEffect 0；ObjectiveLost 0。`C4 STATUS: CLOSED`。**

## Baseline HEAD

```
C4_PARENT_MAIN_HEAD      = f95ca2221161905f6748a0ded8e1002158c3ead9  (C3 合入后的 main)
C4_BENCHMARK_FREEZE_HEAD = 2f41a60...  (JSON meta.git_sha 一致)
```

Merge gate（C3→main 后）：fmt/check 干净、tools 244/244、eval 58/58、nav 8/8 VALID、
edit 8/8 VALID、gate regressions 7/7。

## C2/C3 Handoff 与 C4 Scope

C2=找得到，C3=改得对，**C4=失败发生后能否识别、恢复、继续，最终正确完成；不能恢复时诚实有界地停止**。
"自动恢复"不是"什么都重试"：replay-safe read 可自动 reconcile，mutating/unknown/pending-approval
宁停不猜 —— C4 在这条安全边界上测收敛能力，不放宽它换 recovery_rate。

## Recovery Architecture Audit

- **引擎 repair 环**（engine.rs:1382-1434）：verify FAIL + repairable → `RepairStarted` →
  repair turn → **环内 fresh verify** → `DIRECT_REPAIR_ATTEMPTS=1` 封顶；scope 违规与
  non-retryable 不进 repair。
- **Crash window**（crash_recovery_test，20+ 回归）：safe read 自动重放；mutating/unknown/
  corrupt-args/pending-approval 阻塞不猜；owner-epoch fencing（stale/foreign runtime 不能 ack）。
- **Lifecycle**（47 测试）：`client_disconnect_does_not_cancel_and_explicit_cancel_fires_once`、
  session 跨 client 存续、断线不挂 approval/clarification。
- **Eval 侧**：`recovery: true` + `recovery_rate` 为既有权威机制，直接复用，未造平行样本。

## Deterministic Recovery Contract — D1-D8 Matrix

| D | 契约 | 覆盖 |
| --- | --- | --- |
| D1 verify FAIL 进 repair | `failed_verification_repairs_once_then_fails`（turns=[user,repair]） |
| D2 修复后新鲜验证 | **本轮新增** `a_successful_repair_converges_on_fresh_verification`（gate 只能对修复后的树变绿） |
| D3 修复成功收敛 | 同上（Verified + 恰一次 repair turn） |
| D4 repair 有界 | 既有（`DIRECT_REPAIR_ATTEMPTS` 生效，恰一次） |
| D5 不可恢复诚实结束 | 既有（Failed，永不 Verified；`unresolved known failure must never become verified success`） |
| D6 crash-window replay 安全 | 既有 20+ 条 |
| D7 restart/resume 完整性 | 既有（resume_to_completion、fencing、无重复 transcript/side effect） |
| D8 client loss ≠ task loss | 既有 lifecycle 契约 |

**8/8 PASS，全部在未修改的生产上通过**（`1802316` 只加了 D2/D3 的成功路径回归）。

## C4 Benchmark Cases（`5dd13b3`/`77c4ed7`/`9ca41b1`/`2f41a60`）

`evals/recovery/`，全部 navsvc 底材、`recovery: true`（R7 除外，它是纯长任务）：

| slot | 起始状态 | 测什么 |
| --- | --- | --- |
| R1 | build 红（改名遗留） | 编译诊断→修→绿 |
| R2 | build 绿、既有测试红 | 测试证据驱动修产品代码，tests 禁改 |
| R3 | 接口半落地，多文件级联报错 | 清整条 cascade 而非消一个错 |
| R4 | build 绿、filter 按错误字段过滤 | expected/actual 驱动的语义修复 |
| R5 | 残留备份文件重复声明 main | **删除产物而非改健康代码**（源码逐字节钉死） |
| R6 | 编译错误遮蔽行为回归 | 修 A 暴露 B：分层失败链持续推进 |
| R7 | 绿树 + 5 个编号义务 | 长任务义务存续（早期完成的不许丢） |
| R8 | 红树 + 完整 --stats 特性 | 恢复后继续建设，收敛到一棵全绿树 |

### Recovery Failure Ground Truth

checker 新增 `validity.recovery_baseline`：声明的失败形状（build/tests pass|fail）在未修改树上
逐项验证，不符即 `RECOVERY_BASELINE_SHAPE` → INVALID。**造 case 期间它抓了两次真实错误**：
R2 overlay 意外删掉 `flattenStage`（声称 test-fail 实为 compile-fail）；R4 初版反转谓词使
计数不变、broken 树照样绿（G1 拒绝）。两个坏 fixture 都死在模型运行之前。

## Case Validity Matrix

**8/8 VALID，exit 0**（六项闸门 + shape 证明；冻结前 nav/edit 两套复验仍 8/8）。

## Model Config / Experimental Design / Raw Results

与 C2/C3 同构：`deepseek/deepseek-v4-flash`、direct、**R1–R8 × 3 = 24 runs 预先固定**、
单 invocation 跑满、无提前停止、无 replacement（无 infra incident）。

```
evals/baselines/c4-self-healing.json
sha256 = 011a4b3300af047711b080830e1d106640ca935d009f96cbbc44695a49502636
```

Identity **24/24**（`evals/baselines/c4-run-identity.txt`），24 棵终态树保留。

## Outcome Matrix / Recovery Rate

全部 24 run PASS（R1–R8 各 3/3）。

```
FunctionalPass 24/24 · FalseCompletion 0/24 · Recovery Rate 100%（21/21 injected-failure runs）
TargetRecall 24/24 · ImpactTouch 24/24 · ForbiddenEdits 0 · EditFailures 0 · LoopTrips 0
avg steps 10.5（R7/R8 长任务 15–30 步）
```

## Engine Repair Behavior（重要区分）

**EngineRepairAttempts = 0。** 24 个 run 中 agent 全部在 goal turn 内自行运行验证、消化失败
证据并修复，引擎级 repair 环从未需要触发。按 §39 的区分：

- **case-level recovery**（红 workspace → 绿终态）：21/21 —— 由模型运行证明。
- **engine repair mechanism**（gate FAIL → repair turn → fresh verify）：由 **D1–D5 确定性
  契约**证明，不依赖模型运行。

两者不混算。恢复行为的收敛（RecoveryConverged 21/21：失败证据→新行动→新鲜验证→PASS）、
RepeatedFailureThrash 0、UnboundedRecovery 0（全部在 max_rounds 内正常收敛，无 watchdog 击杀）。

## Honest Failure / Crash-Resume / Duplicate Side Effects / Objective Preservation

- HonestFailure：确定性负例已钉（D5 —— 修不好的 gate 以 Failed 结束，非 Verified）。
- Crash/Resume：D6/D7 既有回归全绿；**DuplicateSideEffect = 0**（mutating 不自动重放的契约未放宽）。
- **ObjectiveLost = 0**：R7 三次 5/5 义务全存活，R8 三次修复+特性双义务收敛 —— 长任务
  objective 跨多轮 mutation/verification 未丢失。

## Failure Classification

**空集。** 无 C4 taxonomy 中任何一类真实失败；无 C2/C3 回归。

## C4 Stage Decision

§50 逐项全部成立。

# C4 STATUS: CLOSED — self-healing / long-task objectives satisfied

不再创建 C4-R1 / C4.5 / C4-longer。

## What Is Not Claimed

- 不声称任意规模长任务（本轮最长 30 步）或任意语言/仓库/模型。
- 不声称墙钟级长任务（长任务按 multi-step + 多次 mutation/verification checkpoint 定义）。
- 进程级 SIGKILL/daemon restart/resume 未纳入 model benchmark（现有 eval 无该生命周期 hook），
  由 D6/D7 确定性契约承载 —— 未造平行 runner。
- 引擎 repair 环在真实模型负载下未被自然触发（agent 抢先自愈）；其行为由确定性契约保证。

## NEXT

```
Integrated Coding Capability Gate / post-C4 roadmap decision
```

不在此处开始 C5、云端或 TUI 改造。
