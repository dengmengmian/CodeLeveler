# C2 FINAL ACCEPTANCE

日期：2026-08-09。分支 `feat/coding-context-efficiency-c2`。模型 `deepseek/deepseek-v4-flash`。

## Executive Verdict

# C2 FINAL: FAIL

> ### ⚠ 本文件的 Top Failure 与 NEXT 已于 2026-08-09 更正
>
> 初版把 Top Failure 记为 `COMPLETION_WITHOUT_BEHAVIORAL_EVIDENCE`、NEXT 记为
> Completion Evidence Gate。随后的 N3/N4 **3×3 control variance 证伪了那个假设** ——
> 失败的运行**持有**已执行的生产路径行为证据，而 N3 两次成功的运行**一次都没有**。
> 详见 `docs/C2_COMPLETION_EVIDENCE_CONTROL_VARIANCE.md`。
>
> **`C2 FINAL: FAIL` 不变**，但失败的归因与下一步已经改变（见 §Final Verdict / §NEXT）。

判在 §38 的硬闸门：**FalseCompletion = 2**（单次 clean baseline；3×3 control 中另计 3 次）。N3 与 N4 都宣布完成、引擎自验通过，而独立隐藏验收失败。

按 §46，这是必须停下的情形 —— **未进入 PHASE D，未写一行 C2.3D 生产代码**。

---

## Scope

本轮执行了 PHASE A（N6 fixture 修正）、PHASE B（干净 8-case baseline）、PHASE C（根因分类）。PHASE D/E/F 未执行，原因见上。

## What C2 Does NOT Claim

不声称达到 Claude Code / Codex 水平。不声称超大仓完成保证。不声称 `read_file` 的所有边界情况已解决 —— PHASE E 本轮未做。

---

## Historical Hypotheses

| 阶段 | 结论 | 如何收敛到当前架构 |
| --- | --- | --- |
| **C2.2** | FALSIFIED | "旧 read 结果可无损回收"实测 reclaim = 0；真实轨迹是 broad-then-narrow，与假设方向相反。**不再以删除旧读取为方向** |
| **C2.3A** | PASS | 移除 plan 对导航的结构性打断。`Plan is execution aid, not navigation license` 成为不变量 |
| **C2.3B** | FALSIFIED | 纯 prompt 导航纪律：正确性持平、轮次 +17%、token +28%、导航质量下降。**不再以加大段 prompt 为方向** |
| **C2.3C-S** | PASS | 修复 Eval 权限升级 + 沙箱逃逸。**测量可信性先于优化** |
| **C2.3C-F** | PASS | N6 fixture 有缺陷才算 case；建立 fixture-validity 契约 |

---

## Eval Integrity

| 指标 | 值 |
| --- | ---: |
| **LeakageSuccessCount** | **0** |
| **PrivilegeEscalationGrantedCount** | **0** |
| LeakageAttemptCount | 0（干净 N6 运行中为 0） |
| Red-team | **7/7 blocked** |
| Production approver | UNCHANGED |

详见 `docs/C2_3C_S_EVAL_SANDBOX_INTEGRITY.md`。

## Fixture Validity

八个 case **全部在未修改 fixture 上 FAIL**（捕获空 case 的那一半闸门生效）。

| Case | 状态 |
| --- | --- |
| N6 | **VALID**（fails-untouched + passes-with-reference-fix） |
| N4 | 参考实现实测 acceptance exit 0 —— 可通过方向已验证 |
| N1 N2 N5 N7 N8 | 已由真实 Agent 运行通过，**经验性证明可通过** |
| N3 | 仅验证 fails-untouched；无参考补丁 |

## Clean Navigation Baseline

| Case | Hidden | Completed | Impact FullyRead | Forbidden | rounds |
| --- | --- | --- | --- | --- | ---: |
| N1 | ✅ | ✅ | 1/1 | 0 | 19 |
| N2 | ✅ | ✅ | 1/1 | 0 | 7 |
| **N3** | **❌** | ✅ | 2/2 | 0 | 46 |
| **N4** | **❌** | ✅ | 5/5 | 0 | 17 |
| N5 | ✅ | ✅ | 4/4 | 0 | 33 |
| **N6（修正后）** | ✅ | ✅ | 1/1 | 0 | **8** |
| N7 | ✅ | ✅ | 4/4 | 0 | 21 |
| N8 | ✅ | ✅ | 1/1 | 0 | 7 |

| 指标 | 值 |
| --- | --- |
| **Hidden Acceptance** | **6/8** |
| **Target Recall** | **8/8** |
| **Required Impact Path Touch** | **8/8** |
| **Required Impact Path FullyRead** | 19 条路径中 **18 FULL / 1 PARTIAL / 0 MISS** |
| **MissedImpactPaths** | **0** |
| **ForbiddenPathsEdited** | **0** |
| **FalseCompletion** | **2** ← 硬闸门 |

**定位与影响面覆盖没有问题。两次失败都发生在拿到完整证据之后。**

---

## N6 Corrected Case

旧 N6 指向的 fixture 里 jsonl 解码器**本来就正确解析 depth** —— 描述的缺陷不存在，隐藏验收恒真。修正后（`--defect jsonl-depth` 变体）：

**N6 在 8 轮内通过**，4 次读取 / 2 次搜索 / r4 首次编辑。

**`TARGET_ABANDONED` 结论正式撤销。** 那次"读了 `decoder.go` 却从不修改"是它反复探测、确认 depth 本来就工作、因而没有东西可改 —— 正确的工程行为被坏 case 记成了失败。

## N3 Root Cause

逐 patch 核实（clean run）：

| 补丁 | 内容 |
| --- | --- |
| #1 | `aggregate.go` + `summary.go`，两处都加 `!record.Valid` 守卫 —— **完全正确** |
| #2 | **只改 `summary.go`**，`Distinct` 的守卫消失 |

终态 `verified`，隐藏验收失败于 `TestDistinctSkipsInvalid`。

**分类：`CORRECT_PATCH_REVERTED`** —— 已产出正确解，随后在无矛盾证据的情况下收窄成错误解。

## N4 Root Cause

**先排除了 harness 缺陷**：按 N6 的教训，我写了参考实现（接口 + 三个 sink + 测试替身 + main footer），N4 的隐藏验收 **exit 0**。所以失败是 Agent 的。

Agent 的三个补丁只触及 `sink.go` / `file.go` / `main.go`，**没有任何一个包含任务要求的 `wrote N records` footer** —— main.go 的补丁只是把 `out.Close()` 挪了位置。

| 事实 | 值 |
| --- | --- |
| Target Recall | full |
| Impact FullyRead | **5/5** |
| 契约理解 | 正确（接口注释写得准确） |
| 任务要求的可观察行为 | **从未实现** |
| 引擎自验 | PASS（新行为没有既有测试，build + 现有测试抓不到） |

**分类：不是导航失败，也不是 `CORRECT_PATCH_REVERTED`**（无撤销、无目标转移）。

> **更正（control variance 之后）**：初版把它描述为"在没有行为证据的情况下宣布完成"。
> 3×3 control 显示 N4 的失败运行**都执行过产物二进制并观察了输出**（候选 2 / 3）。
> 准确描述是：证据真实且执行正确，但**观察的是 stdout，而需求的 footer 在 stderr** ——
> Requirement / Obligation Coverage mismatch，不是证据缺失。

## Top Failure Class

### 初版结论（已撤销）

初版把 N3 与 N4 归为同一个类别：

```
TOP FAILURE CLASS: COMPLETION_WITHOUT_BEHAVIORAL_EVIDENCE
```

理由是两者都"证据完整 → 引擎自验绿 → 宣布完成 → 隐藏验收失败"，因而推断 Agent 把
"build + 既有测试通过"当成了需求已满足的证据。

### 为什么撤销

N3/N4 各 3 次 control variance（按 `session_id` 对齐）给出的行为证据计数：

| Case | rep | 结果 | 行为证据候选 |
| --- | ---: | --- | ---: |
| N3 | 1 | PASS | **0** |
| N3 | 2 | PASS | **0** |
| N3 | 3 | FAIL | 3 |
| N4 | 1 | FAIL | 2 |
| N4 | 2 | FAIL | 3 |
| N4 | 3 | PASS | 4 |

**失败的运行并不缺行为证据 —— 它们构建二进制、喂输入、观察输出。而 N3 两次成功的运行
一次都没有执行过产物。** Evidence Presence 对成败没有正判别力，在 N3 上方向相反。

```
COMPLETION_WITHOUT_BEHAVIORAL_EVIDENCE = FALSIFIED
```

**不得实现 Completion Evidence Presence Gate**：它会漏掉 N4 的两次失败（证据已存在
且成功执行），并误触发 N3 的两次成功（无行为证据却做对了）。

### 准确的根因

两者都是 **Requirement / Obligation Coverage mismatch**，不是证据缺失：

- **N3**：证据真实，但遗漏的 obligation（`report.Distinct`）**不经过所观察的 CLI surface**。
- **N4**：证据真实且执行正确，但观察的是 **stdout**，而需求的 footer 在 **stderr**。

### 另一条必须记录的边界

**Path coverage 不能代理 obligation coverage。** N3 的 `required_impact_paths` 是
2/2 touch、2/2 FullyRead、两个文件都被修改，最终仍因 `aggregate.go` 的 guard 被撤销
而失败。`required_paths` 今后只能作为 supporting / diagnostic metadata。

### 一个开放问题（不下结论）

N3 两次 PASS 没有执行任何产物级 probe 却实现正确 —— 成功可能来自**实现本身覆盖了
全部 obligation**，而非存在独立的行为证据。后续必须分别测量
**Implementation Obligation Coverage** 与 **Evidence Obligation Coverage**，
找出哪一层能区分 PASS/FAIL，**不要预设答案是 evidence**。

## False Completion

**FalseCompletion = 2（N3、N4）。**

C1 的 `False Completion = 0` 是产品级硬不变量。C2 的干净 baseline 出现 2 例，属于 §38 的直接 FAIL 条件。

需要说明边界：这两例发生在 **N 系列导航 case** 上，不是 C1 代表集。C1 代表集在 C2.3B 阶段的最后一次实测仍是 11/11、False Completion 0。但本轮**未重跑 C1 代表集**（PHASE F 未执行），因此不能声称 C1 无回退。

---

## 未执行的部分（如实记录）

| 阶段 | 状态 | 原因 |
| --- | --- | --- |
| PHASE D — Evidence-to-Edit Treatment | **未执行** | §18 门槛不成立：Top-1 已变，且 N3/N4 机制不同 |
| Completion Evidence Gate | **未实现，且不应实现** | N3/N4 各 3 次 variance 已跑完并证伪其前提（Evidence Presence 无判别力） |
| PHASE E — Read Evidence Engineering | **未执行** | 停在 PHASE C |
| PHASE F — C2 Final Regression | **未执行** | C1 代表集、yq、ripgrep 均未重跑 |
| N1/N2/N3/N5/N7/N8 参考补丁 | 部分 | N3 缺参考补丁；其余五个由真实通过运行经验性验证 |

---

## Workspace Gate

`cargo fmt --check` ✅ · `cargo check --workspace --all-targets` ✅ 0 error / 0 warning · `cargo test --workspace --no-fail-fast` ✅ **122 suites / 2,570 tests / 0 failed**（C2.3C-S 之后的最后一次全量；PHASE A 之后只改动了 `scripts/` 与 `evals/`，未触碰 Rust 代码）。

---

## 五个必答问题

**1. 能不能稳定找到真正生效的代码？** —— **能。** Target Recall 8/8，含 distractor case（N8）、竞争实现（N2）、1,022 行大文件（N6）。ForbiddenPathsEdited = 0，从未改过 `legacy/` 或 `examples/`。

**2. 能不能找到明显影响面、避免 half-fix？** —— **能找到。** Required Impact Path Touch 8/8，FullyRead 18/19（1 条 PARTIAL 是 N6 对大文件的区间读取，属正确行为）。MissedImpactPaths = 0。**但"找到"不等于"落实"** —— 见问题 3。

**3. 拿到正确证据后能不能稳定落实成 final edit？** —— **不能，但原因不是初版写的那个。** 8 个 case 中 2 个失败，且都在定位与影响面完整之后。3×3 control 进一步显示：失败运行**持有**已执行的生产路径证据，而 N3 两次成功的运行**一次都没有**。真正的缺口是**证据与需求的对应关系**（N3 漏了不经 CLI 的 `Distinct`；N4 观察 stdout 而需求在 stderr），不是证据的有无。这一层目前**无法测量** —— 见 `docs/C2_COMPLETION_EVIDENCE_CONTROL_VARIANCE.md`。

**4. read evidence 是否存在 blocking correctness hole？** —— **未知。** PHASE E 未执行。已知的相关事实是覆盖率语义已收口（失败读取不计证据、截断不误算整读），但未做行业对照与完整审计。

**5. C2 是否有资格 CLOSED？** —— **没有。** FalseCompletion = 2 是硬闸门。

---

## Final Verdict

# C2 FINAL: FAIL

```
FALSIFIED:        COMPLETION_WITHOUT_BEHAVIORAL_EVIDENCE
CURRENT BLOCKER:  REQUIREMENT_TO_EVIDENCE_ALIGNMENT
```

`REQUIREMENT_TO_EVIDENCE_ALIGNMENT` 是**下一个研究问题**，不是已被证明的生产修复方案。

## NEXT

**C2-R1 — Requirement Coverage & Evidence Alignment Baseline**（measurement-only）

```
STATUS: NOT STARTED
```

前置条件见 `docs/C2_COMPLETION_EVIDENCE_CONTROL_VARIANCE.md` §9。

不开始 NEXT。
