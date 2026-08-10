# C2-BV2 Trustworthy Product Baseline

日期：2026-08-10。分支 `feat/coding-context-efficiency-c2`。**MEASUREMENT ONLY —— 生产代码零改动。**

**结论前置：24/24 functional PASS，FalseCompletion 0/24，Target Recall 24/24，
Impact Touch 24/24，ForbiddenPathsEdited 0。无任何 benchmark incident。
`C2 STATUS: CLOSED — navigation/context objectives satisfied`。**

## Baseline HEAD

```
C2_BV2_BASELINE_HEAD  = 797b72e68d3c84de54cd6b074be23cb9001f4639
BENCHMARK_FREEZE_HEAD = 797b72e68d3c84de54cd6b074be23cb9001f4639
```

权威 JSON 的 `meta.git_sha` 与冻结 HEAD 一致。运行期间 benchmark 与 production 双冻结，
全程未修改任何 case / fixture / reference / oracle / 生产代码。

## Benchmark Freeze & Validity

**模型调用之前**在冻结 HEAD 上重证了全部前置（任何一项失败即 ABORT，未发生）：

| 前置 | 结果 |
| --- | --- |
| `check_fixture_validity.py`（六项闸门） | **8/8 VALID，exit 0** |
| gate regressions（含正类对照） | 7/7 PASS |
| obligation oracles | 4/4 双向 PROVEN |
| `cargo test -p leveler-eval --lib`（含 loader） | 58 passed / 0 failed |
| `cargo fmt --check` | 干净 |

## Model / Provider / Runtime Config

| 项 | 值 |
| --- | --- |
| model | `deepseek/deepseek-v4-flash`（与全部 C2 历史 baseline 一致） |
| mode | `direct` |
| engine_version | 0.1.4 |
| repetitions | 3（harness 原生 `--repetitions`） |
| workspace isolation | per-execution（`becac4d`），`LEVELER_EVAL_KEEP_WORKSPACE=1` |
| sampling 参数 | UNKNOWN（provider 配置未在 run meta 中导出，未编造） |
| max rounds | per-case `max_rounds`（N1–N8 均 60） |

## Experimental Design

**预先固定 N1–N8 × 3 = 24 runs**，单一 invocation 顺序执行，无论中间结果如何全部跑完。
无提前停止、无 run-until-PASS、无 replacement（产品原因的 replacement 本就禁止；
本轮也没有 ENVIRONMENT_INFRA 需要 replacement）。

### 一次已记录的 infra incident（发生在正式结果集之前）

第一次 invocation（pid 28128）在第 21 个 run（N7 rep3）进行中被外部终止，权威 JSON 未落盘。
该次的 20 个完成 run **整体不采信、不进结果集**（存档 `bv2_run_aborted_pid28128.txt`），
经确认后全新重跑 24 个。此决定不依赖任何已观察结果 —— 重跑是全量的，不是选择性的。

## Run Identity

**24/24 确定性映射**：harness 逐 run 输出 `case / repetition / session_id / workspace_path`
四元组（完整 stdout 落盘，未经 `tail`），24 棵终态树全部保留在磁盘。
映射存档 `evals/baselines/c2-bv2-run-identity.txt`。未使用 mtime / 最新目录 / case+pid 推断。

## Raw Result Set

```
evals/baselines/c2-bv2-navigation.json
sha256 = e9b6564300eb19133ffc57e2b320503b9c74c8607cc98f58ad2c979b5544b101
```

分析开始前先锁定 hash。下文每个数字都可从该 JSON 逐 run 回溯。

## N1-N8 Outcome Matrix

| Case | Rep1 | Rep2 | Rep3 | Pass Rate | FalseCompletion |
| --- | --- | --- | --- | --- | ---: |
| N1 unknown-location | PASS | PASS | PASS | 3/3 | 0 |
| N2 competing-implementations | PASS | PASS | PASS | 3/3 | 0 |
| N3 caller-propagation | PASS | PASS | PASS | 3/3 | 0 |
| N4 interface-contract | PASS | PASS | PASS | 3/3 | 0 |
| N5 config-to-runtime | PASS | PASS | PASS | 3/3 | 0 |
| N6 large-file-region | PASS | PASS | PASS | 3/3 | 0 |
| N7 cross-package | PASS | PASS | PASS | 3/3 | 0 |
| N8 distractor-resistance | PASS | PASS | PASS | 3/3 | 0 |

## Functional Pass Rate

**24/24（100%）。** 每个 run 的 `expect_passed = true` —— 独立隐藏验收，非引擎自验。

## False Completion

**0/24。** 无任何"宣布完成但隐藏验收失败"的 run；也无诚实未完成（不存在 FAIL，两类都空）。
`false_completion_rate = 0.0`（harness 原生统计）。

## Target Recall

**24/24 —— 无 MISS。** 每个 run 触达其全部 `relevant_paths`（N3/N4/N6 多次超出必需面，
额外读了相邻实现，属正常探索）。

## Impact Discovery

**24/24 —— 无 MISS。** 每个 run 触达其全部 `required_impact_paths`，含 N4 的 5 路径
接口契约传播与 N5/N7 的 4 路径 config→runtime 贯通。`MissedImpactPaths = 0`。

## Forbidden Edits

**0/24。** `legacy/` 与 `examples/` 全程未被编辑。distractor 路径有零星**读取**
（24 run 合计 21 次读，最多单 run 2 次）—— 读 distractor 后放弃是预期行为，不是缺陷。

## Execution Shape

| 指标 | 值 |
| --- | --- |
| avg steps | 16.0（对照 C2 FINAL 时期 clean baseline 约 19.75，且当时 2/8 失败） |
| avg tool calls | 24.4 |
| loop guard trips | **0/24** |
| verification ran | **24/24** |
| edit failures | 24 run 合计 1 次（N5 rep2，同 run 内自行恢复，harness 记 self-recovered） |
| tokens | 未作为 gate（§18）；逐 run 记录在 JSON。N7 最重（rep2 输入 619k/输出 61k） |

## Failure Classification

**空集。** 无 functional FAIL，无 ENVIRONMENT_INFRA（正式结果集内），无 BENCHMARK_INVALID，
无 IDENTITY_UNRESOLVED。逐 case 深挖 trajectory 的条款（§36 N1–N8 Analysis）因此无对象 ——
本文件不为 24 个正常 PASS 写流水账。

## C2 Stage Boundary

§38 关闭条件逐项核对：

| 条件 | 状态 |
| --- | --- |
| Benchmark VALID 8/8 | ✅（run 前重证） |
| 无可复现的 primary C2_CONTEXT_NAVIGATION blocker | ✅（无任何 FAIL） |
| 无 benchmark validity incident | ✅ |
| 无 answer-key leakage | ✅（密封结构未变；24 run 无一触及 reference / obligation 面） |
| Forbidden edit safety | ✅（0/24） |

## C2 Final Status

```
C2 STATUS: CLOSED — navigation/context objectives satisfied

Product benchmark:  24/24 functional PASS
FalseCompletion:    0/24
Target Recall:      24/24
Impact Touch:       24/24
ForbiddenEdits:     0/24
```

## Transferred Blockers

**NONE。** 无 FAIL 可转交 C3/C4。

## What Is Not Claimed

- **不声称**产品在任意仓库 / 任意任务上正确 —— 这是 8 个 Go navigation case、单一模型的测量。
- **不声称** C3（implementation/edit reliability）已被证明 —— 它尚未被独立测量，只是本轮
  没有出现属于它的失败。
- **不声称**旧失败（N3/N4 历史 FalseCompletion）被"修好了" —— 它们所在的 case 当时不可满足 /
  欠定，修的是尺子；本轮是在有效尺子上的**首次**可信测量。
- **不重开**已证伪 / 已关闭的研究线：Completion Evidence Presence、Path-Coverage-as-Proxy、
  prompt 导航纪律、read supersession、R1 判别力问题。

## NEXT

```
C3 — Implementation / Edit Reliability
```

不在本阶段开始。
