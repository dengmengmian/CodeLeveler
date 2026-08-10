# C3 Implementation / Edit Reliability Baseline

日期：2026-08-10。分支 `feat/coding-implementation-reliability-c3`。**生产代码零改动。**

**结论前置：Deterministic Edit Contract 8/8 PASS；benchmark 8/8 VALID；24/24 functional
PASS；FalseCompletion 0/24；SilentPartialMutation 0；StaleOverwrite 0；唯一一次 edit failure
是 CAS 守卫的正确拒绝且同 run 自愈。`C3 STATUS: CLOSED`。**

## Baseline HEAD

```
C3_BASELINE_HEAD         = 46db88dff7ddf93b2a8b8a2308d35c9a0467a0c7  (main, C2 合并后)
C3_BENCHMARK_FREEZE_HEAD = 294088ecb6cdc93ee5bd5fe255c1c4acb394d9a6
```

## C2 Handoff

C2 已 CLOSED（24/24、FalseCompletion 0）。C3 的问题不是"能不能找到代码"，而是：
**找到正确代码以后，能不能稳定、完整、无破坏地把最终代码改对。**

## C2/C3/C4 Boundary

| 阶段 | 回答 |
| --- | --- |
| C2 | 找到正确代码、读够上下文、发现影响面 |
| **C3** | 找到以后，能不能稳定地改对（完整性、传播、不被后续 edit 破坏、无 silent partial mutation） |
| C4 | 发现失败以后，能不能自动恢复并最终完成 |

一次 edit 报错后立即换正确方式完成 = C3 的 `SELF_RECOVERED`；持续失败不收敛才是 C4。

## Current Edit Architecture Audit

审计对象：`apply_patch.rs`(1000 行)、`replace.rs`(1423 行)、`patch/`(1400 行)、`format.rs`，
共 **85 条既有测试**。生产栈已具备：全量预规划（任何 hunk 失败在首次写盘前中止）、CAS 提交、
跨文件逆序回滚、回滚拒绝覆盖并发写（响亮）、stale 预检+提交双层守卫、replace 歧义拒绝、
保守 fuzzy fallback、formatter 后重指纹、modified_files 上报。**未预设缺陷，未重写任何工具。**

## Deterministic Edit Contract Matrix

| 契约 | 既有测试 | 本轮补充（`072acb9`） |
| --- | --- | --- |
| A1 失败 patch = 零变更 | `failed_hunk_writes_nothing` | `a_later_failing_file_leaves_the_earlier_matching_file_untouched`（跨文件） |
| A2 多文件回滚 | `stale_delete_rolls_back_earlier_commits_without_clobbering_external_changes` | `a_rollback_that_would_clobber_a_concurrent_write_fails_loudly` |
| A3 stale replace 拒绝 | `concurrent_replaces_cannot_both_commit_from_the_same_version` | — |
| A4 stale apply_patch 拒绝 | `rejects_a_patch_to_a_file_changed_since_it_was_read` 等 3 条 | — |
| A5 歧义 replace | `ambiguous_without_replace_all_is_refused`（断言零变更） | — |
| A6 formatter 指纹 | `gofmt_reformats_after_edit_when_available` + `consecutive_patches_...` | `an_edit_after_the_formatter_rewrites_the_file_is_not_stale` |
| A7 modified-files 上报 | replace/move 3 条 | — |
| A8 move/delete 原子性 | `move_refuses_an_existing_destination_...` 等 | `a_failed_later_op_rolls_back_a_committed_move_completely` |

**8/8 PASS，全部在未修改的生产上通过**（leveler-tools 244/244）。

> Measurement note：4 条新测试中 2 条初稿在生产上跑红，复核后**两次都是测试侧错误**
> （回滚失败经 `ToolError` 通道而非 `ToolOutput`；测试 ctx 默认关 `auto_format`）。
> 生产行为两次均正确。无 `C3 DETERMINISTIC BASELINE FAILURE`。

## C3 Benchmark Cases（`294088e`）

8 个 case，全部以 navsvc fixture 为底材；**导航刻意不构成难度** —— 每个任务点名文件/表面，
失败只能来自"改"而不是"找"。

| slot | case | 测什么 |
| --- | --- | --- |
| E1 | `c3-e1-sequential-same-file` | 同文件两处独立修改，后编辑不得回退前编辑 |
| E2 | `c3-e2-contract-propagation` | 行为契约传播 4 个实现，**无编译耦合**（漏一个照样编译） |
| E3 | `c3-e3-ambiguous-exact` | 两个逐行相同的 helper，只改其一，孪生被钉死 |
| E4 | `c3-e4-repeated-context` | 孪生循环只有一个带 bug，补丁必须落在正确函数 |
| E5 | `c3-e5-move-dependency` | move+rename：旧名消失、新文件存在、caller 跟随 |
| E6 | `c3-e6-formatter-mediated` | gofmt-dirty 文件两处义务；首编辑触发全文件重排后第二处仍须落地 |
| E7 | `c3-e7-overlapping-config` | 两个特性穿同一 struct/parser/写循环，声明了合成顺序 |
| E8 | `c3-e8-final-tree` | 一行三个计数器，部分实现"看起来完成" |

## Case Validity Matrix

沿用 C2-BV 六项闸门（checker 最小泛化：reference 目录从 cases 目录派生，仍是唯一机制）。
**逐 case 修完立即过闸，VALID 才进下一个。** 最终：

```
evals/edit 8/8 VALID, exit 0
C2 evals/navigation 复验仍 8/8 VALID（尺子未被破坏）
gate regressions 7/7 · loader 58/58
```

Answer-key containment：references 位于密封的仓库根 subpath 内，与 navigation 同一结构；
密封 deny 机制有既有 red-team regression（`declared_read_denials_survive_every_escape_...`）。
24 个 run 无一读取 `evals/edit/reference/`。

## Benchmark Freeze / Model Config / Experimental Design

冻结于 `294088e`（JSON `meta.git_sha` 一致），benchmark 与 production 双冻结。
模型 `deepseek/deepseek-v4-flash`、mode=direct、与 C2-BV2 同构：**E1–E8 × 3 = 24 runs
预先固定**，单 invocation 跑满，无提前停止、无 replacement（无 infra incident）。

## Raw Result Hash / Run Identity

```
evals/baselines/c3-edit-reliability.json
sha256 = 48e805093a4294affa51d3a0a9c6920c8c034b06b1ba8f4b12e17bec8962b8e3
```

分析前锁定。Identity **24/24** 确定性映射（`evals/baselines/c3-run-identity.txt`），
24 棵终态树全部保留。

## Outcome Matrix

| Case | Rep1 | Rep2 | Rep3 | FalseCompletion |
| --- | --- | --- | --- | ---: |
| E1–E8（全部） | PASS | PASS | PASS | 0 |

```
FunctionalPass    24/24        FalseCompletion   0/24
TargetRecall      24/24        ImpactTouch       24/24
ForbiddenEdits    0/24         loop trips        0/24
verification ran  24/24        avg steps         10.5
```

## Edit Failure Recovery

**EditFailureRecovered = 1**（E8 rep3）。事件库取证：一次 `apply_patch` 在 commit 时被
CAS 拒绝（"changed on disk between planning and writing"），干净拒写 → agent 重读 →
重建补丁 → 通过。**这是 A4 守卫在真实负载下的正确触发**，harness 记 self-recovered。
按 §32 这是 healthy recovery，不是 FAIL。

## Silent Partial Mutation / Stale Write Safety

```
SilentPartialMutation = 0
StaleOverwrite        = 0
```

依据：唯一一次工具报错是上述 CAS 干净拒绝（拒绝零变更由 A1/A4 确定性契约保证并被
Layer A 测试钉住）；全程无任何"检测到 stale 仍覆盖"的事件；24 棵终态树全部通过独立验收。

## Multi-file Consistency / Final-tree Regression

```
MultiFileConsistencyFailure = 0    （E2 三次全部把契约传播到全部 4 个实现）
FinalTreeRegression         = 0    （E1/E6 三次×2 case 两处义务全部存活）
```

E6 终态文件为 gofmt 对齐格式 —— formatter 确实在 case 中途重写了整个文件，两处义务仍落地，
机制被真实行使而非空转。

## Failure Classification

**空集。** 无 functional FAIL、无 ENVIRONMENT_INFRA、无 BENCHMARK_INVALID、无
IDENTITY_UNRESOLVED、无 C2_NAVIGATION_REGRESSION。逐 case trajectory 深挖无对象。

## C3 Stage Decision

§34 关闭条件逐项：Deterministic contracts PASS ✅ · Benchmark VALID 8/8 ✅ ·
Real-model 24/24 ✅ · FalseCompletion 0 ✅ · SilentPartialMutation 0 ✅ ·
StaleOverwrite 0 ✅ · ForbiddenEdits 0 ✅ · 无可复现 C3-specific blocker ✅。

# C3 STATUS: CLOSED — implementation/edit reliability objectives satisfied

不再创建 C3-R1 / C3.5 —— 不为阶段存在感制造优化项目。

## What Is Not Claimed

- 不声称在任意语言/任意仓库/任意模型上的编辑可靠性 —— 这是 8 个 Go case、单一模型的测量。
- 不声称 C4（self-healing / long task）已被测量 —— 本轮无一次持续失败可供观察恢复行为。
- 不声称 benchmark 难度已达工业级极限；它测的是明确定义的 8 类编辑可靠性形态。

## NEXT

```
C4 — Self-Healing / Long Task Reliability
```

不在本阶段开始。
