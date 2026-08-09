# C2 Scope Reclassification

日期：2026-08-09。分支 `feat/coding-context-efficiency-c2`。**CLASSIFICATION ONLY —— 生产代码零改动。**

**结论前置：两个 blocking case 本身失效，因此无法把 N3/N4 归入 C2、C3 或 C4。**
N3 经自动闸门判定 `INVALID`（按其自身约束不可满足）；N4 的隐藏验收唯一的判别点落在任务文本
未消歧之处。**Stage Boundary Decision = INCONCLUSIVE，原因是 benchmark validity，不是 Agent 能力。**

## HEAD

```
branch  feat/coding-context-efficiency-c2
HEAD    85002f28de0c85e37b5102d3e259ea1e64aebf59
```

## Why Reclassification Exists

C2-R1 收在 `INCONCLUSIVE — insufficient PASS samples`，并记录六次 replay 都存在
"真实 implementation obligation gap"。本阶段要回答的是：这个 gap 属于**没找到 / 没理解**（C2），
还是**找到了却没改完整**（C3）。

**结果是两者都不是。** 追 trajectory 时先撞上了一个更前置的问题：两个 case 都不能承载
这个判断。R1 那句"六次失败全部存在真实 implementation gap"因此**必须撤销**（见下）。

## Stage Definitions

| 阶段 | 回答的问题 |
| --- | --- |
| **C2** | 能不能找到正确代码、读够上下文、发现影响面、避免导航导致的半修 |
| **C3** | 找到之后，能不能稳定地改对（完整性、多文件一致性、接口/实现传播、正确补丁被撤销、stale edit、apply_patch 恢复） |
| **C4** | 发现失败之后，能不能自己恢复并继续完成 |
| **Completion Escape** | **secondary** —— 错误实现为什么没有阻止"宣布完成"。不得据此把 primary 定成 C2 |

`Defect Origin ≠ Completion Escape`。最终表现为 FalseCompletion，不代表根因留在 C2。

## Data Integrity

R1 文档记录 `session_id` 只留存 2/6。**本阶段发现该限制不成立** —— `~/.leveler/projects/`
的项目目录名内嵌完整 workspace 路径：

```
-private-var-folders-…-T-leveler-eval-n3-caller-propagation-33006-exec1-r1-2d18a36c0aeaa091
```

每个 execution 一个项目目录、一个 `sessions.db`、一条 session。因此
`case / repetition / session_id / workspace_path` **6/6 确定性映射**，无需任何推断。
其中两条（`8653a1c5`、`fe1770a2`）与 R1 stdout 幸存的两行吻合，互为交叉验证。

| case | rep | exec | session_id |
| --- | ---: | ---: | --- |
| n3-caller-propagation | 1 | 1 | `78558864-98da-43e7-ac95-b3c256f30a4f` |
| n3-caller-propagation | 2 | 2 | `11f9c0a4-80c2-4e61-9c39-873f85ace23a` |
| n3-caller-propagation | 3 | 3 | `02e49a38-af75-4bb5-82c5-32e5648aeae8` |
| n4-interface-contract | 1 | 4 | `92075c55-51b6-4dbb-a103-b8c765caaf1a` |
| n4-interface-contract | 2 | 5 | `8653a1c5-bc19-4d40-9a39-53bef9bfa2e5` |
| n4-interface-contract | 3 | 6 | `fe1770a2-d8a5-42c6-9759-71438f5d2e67` |

全部分析基于该映射。未使用 mtime、最新目录、`case + PID`，未从 event patch 重建最终树 ——
所有终态断言都直接跑在保留下来的真实 workspace 上。

## N3 Failure Timeline

三次运行形态一致：

```
读 summary.go + aggregate.go
  ↓
patch#1：给 Summary.Observe 与 Distinct 同时加 `if !record.Valid { continue }`   ← 完全正确
  ↓
go test ./...  →  FAIL: TestSummaryCountsPerName (summary_test.go:17)
  ↓
查 git history 确认既有测试意图
  ↓
后续 edit 削弱或撤销正确实现
  ↓
Completed / verified
```

`patch#1` 逐字满足两条 obligation。破坏它的是后续编辑：

| run | 后续编辑 | 终态 |
| --- | --- | --- |
| rep1 | **删除** `Summary.Observe` 的守卫 | Distinct 正确，Summary 计入 invalid |
| rep2 | 换成 `usable(r) = r.Valid \|\| r.Value != 0` | 两条都被削弱 |
| rep3 | 同上 | 同上 |

**表面上这是教科书级的 `CORRECT_PATCH_REVERTED`（C3）。但撤销是被一个真实信号触发的。**

### 为什么撤销 —— 这个 case 自相矛盾

既有测试 `internal/report/summary_test.go` 构造记录时不设 `Valid`：

```go
out := Aggregate([]api.Batch{{Records: []api.Record{
    {Name: "a", Value: 1},        // Valid 是零值 = false
    {Name: "a", Value: 2},
```

而任务要求：

> Reporting must ignore records that are not valid. …
> **Do not change existing tests.** `go build ./...` and `go test ./...` must pass.

三条约束不能同时成立：忽略 invalid 记录 ⊕ 保持一个喂 invalid 记录并期望其被计入的测试通过
⊕ 不许改测试。Agent 精确诊断了这一点，原文：

> rep1：*"The existing test constructs records without `Valid` set, so they're skipped by my
> change. Let me check git history for intent before deciding how to proceed."*
>
> rep2：*"The task says don't change tests and tests must pass"* ／
> `next_step: "Verify with grader test suite (updated summary_test.go expected to set Valid: true)"`
> ／ summary: *"stale visible test conflicts with new contract"*

### 自动闸门的判定

`scripts/check_fixture_validity.py`（N6 之后为此建立的闸门）在参考补丁上直接判：

```
n3-caller-propagation   REFERENCE-FIX-FAILS   ← the case cannot pass (rc=1)
n3-caller-propagation   INVALID
```

参考补丁只含严格守卫、只改两个源文件 —— 与 Agent 的 `patch#1` 逐字等价。**语义正确的实现
过不了这个 case。**

### 为什么之前没被发现

- C2 FINAL 时期 N3 **没有参考补丁**，闸门只验证了 `fails-untouched` 一半，记为
  "仅验证 fails-untouched；无参考补丁"。
- R1 为 obligation oracle 补了参考补丁，但 `check_obligation_oracles.py` 用
  `go test -run <单个测试>` 只跑那一条 obligation 测试，**不跑该包的既有测试**，因此
  "oracle 双向证明"在结构上无法发现 case 级的不可满足。
- **方法学教训：用自己写的 reference fix 证明 oracle，证明不了 case 在其自身约束下可满足。**
  两者必须分别验证。

## N3 Primary Classification

```
N3 Primary:            UNCLASSIFIED
N3 Completion Escape:  YES
N3 Confidence:         HIGH (对"不可分类"这一判断本身)
```

不能归 C2：两个语义面都被发现、读取、并在 `patch#1` 中正确修改。
不能归 C3：最终树的偏差由一个真实的、case 自身制造的约束冲突驱动，不是编辑不可靠。
不能归 C4：没有需要恢复的失败。

**N3 的三次失败不携带任何关于 Agent 能力的信息，必须从所有能力结论中剔除。**

## N4 Failure Timeline

三次运行都：读全部 sink 实现（含 `null.go`）→ 把计数加进 `Sink` 契约 → 传播到
stdout / file / testutil 三个实现 → 在 `main.go` 打 stderr footer → build + test 全绿 → 完成。

只测任务文本**无歧义**的部分（直接跑三棵保留的树）：

| exec | stdout sink footer | `--summary` 有无 footer | null sink |
| --- | --- | --- | --- |
| 4 | `wrote 3 records` ✅ | 无 ✅ | `wrote 0 records` |
| 5 | `wrote 3 records` ✅ | 无 ✅ | `wrote 0 records` |
| 6 | `wrote 3 records` ✅ | 无 ✅ | `wrote 0 records` |

**每一条无歧义要求都满足了**：footer 在 stderr、计数来自 sink 契约本身而非调用方重算、
`--summary` 模式无 footer、既有测试未改、build + test 通过。

唯一失败点是 null sink，而隐藏验收的**第一条断言**正是它。

### 任务文本没有消歧这一点

```
N is the number of records the output sink actually accepted across the whole run.
The count has to come from the sink itself … a sink is free to reject or coalesce
records, and the footer must report what was really written.
```

- "actually **accepted**" → null sink 收下了 3 条 → 3
- "what was **really written**" + "free to **reject**" → null sink 什么都没写 → 0

三次运行**独立地**选了同一种读法，并各自写下注释论证（*"the null sink accepts nothing"*、
*"nothing is actually written anywhere"*）。三次独立收敛本身就是文本欠定的证据。
隐藏验收自己的注释 *"for a sink that writes nowhere"* 也承认了这个张力。

N4 与 N3 不同：闸门判 `VALID`（参考实现双向通过），case 可满足。缺陷是**规格在唯一判别点上
欠定**，而现有闸门不检查歧义。

## N4 Primary Classification

```
N4 Primary:            UNCLASSIFIED
N4 Completion Escape:  YES
N4 Confidence:         LOW（对"存在真实 implementation gap"这一说法）
```

不能归 C2：全部相关面被发现并读取。
不能归 C3：契约向所有实现传播完整，无撤销、无 stale edit、无 apply_patch 失败；
唯一分歧是一个语义解释。
不能归 C4：没有被检测到的失败。

## Cross-Case Commonality

**共同点不是能力缺陷，是 case 缺陷：两个 blocking case 的判别点都不能承载它们被用来承载的结论。**

一个独立于 fixture 有效性、值得记录的观察（**不作为本阶段的决定**）：

N3 三次运行都在中途明确识别出需求与既有约束的冲突，随后**静默选边并宣布完成**。
rep1 的终态自述如实写着 *"Restored summary path to count all records"* —— 即它知道自己
放弃了需求的一半 —— 仍然 `status: complete` / `outcome: verified`。rep2 的终态自述
*"Fixed report to ignore invalid records; build and tests pass"* 与实际交付的
`r.Valid || r.Value != 0` **不符**，而它中途那句准确的 *"stale visible test conflicts with
new contract"* 在最终汇报中消失了。

即：**没有任何机制把"我无法在不违反某条约束的前提下满足需求"转成非完成结论或显式 blocker。**

范围限制：本现象目前只在一个自相矛盾的 fixture 上观察到；真实仓库中"新需求与既有测试冲突"
很常见，但这一点尚未在有效 case 上复现，**不足以据此新建任何机制**。

## What Is Not Proven

- N3/N4 的失败**不能**证明存在 implementation/edit reliability 缺陷。
- 也**不能**证明存在 navigation / context 缺陷。
- **不能**证明 self-healing 缺陷。
- 上述 completion 观察**不构成**已确认的产品 blocker。
- 本阶段**未**证明 C2 的目标已经达成 —— 只证明现有 blocking 证据不成立。

## Stage Boundary Decision

```
C2-SR: ABORTED / NOT CLASSIFIABLE

Reason: BENCHMARK VALIDITY NOT ESTABLISHED
```

分类阶段**中止**而非得出"证据不足"的结论 —— 差别在于：不是数据太少，而是**尺子失效**，
分类工作在修好 benchmark 之前无法进行。

**现在不能拿 N3/N4 给产品能力分层。**

不选 `TRANSFER_TO_C3`：没有任何一次失败能被归到 implementation/edit 机制 —— N3 的偏差由
case 的内部矛盾驱动，N4 的偏差由规格欠定驱动。**在无效证据上转阶段，会把 C3 建立在一个
不存在的问题上。**

不选 `C2_REMAINS_OPEN`：没有可复现的 navigation/context 机制造成 implementation miss。
§23 要求命名具体机制，我命名不出来。

不选 `TRANSFER_TO_C4` / `CROSS_CUTTING_COMPLETION_BLOCKER`：前者无对应现象；后者的候选观察
只在无效 fixture 上出现过。

## C2 Status

```
C2 PRODUCT VERDICT: NOT CURRENTLY ADJUDICABLE

Reason: BENCHMARK VALIDITY BLOCKER
```

原判 `C2 FINAL: FAIL` 建立在 §38 硬闸门 **FalseCompletion = 2** 上，而这 2 例正是 N3 与 N4。
N3 的隐藏验收现已判定不可满足，其 FalseCompletion 标签不成立；N4 的判别点欠定。

**既不维持 FAIL，也不改判 PASS。** 维持 FAIL 会把尺子的缺陷算到产品头上；改判 PASS 是过度
伸张 —— 证据受损只说明当前判定站不住，不说明 C2 目标已达成。准确的状态是**当前不可裁决**。

Localization / Impact Discovery / Eval Integrity 的历史测量结果依然乐观
（Target Recall 8/8、Impact Path Touch 8/8、`ForbiddenPathsEdited = 0`），
但**最终验收必须等到 N1–N8 全部成为有效 case 之后**。

Completion Evidence Presence 与 Path-Coverage-as-Proxy 两个假设仍然是 FALSIFIED ——
它们的证伪不依赖 N3/N4 隐藏验收的正确性。

保持不变的事实：Localization / Impact Discovery / Eval Integrity 三项 PASS；
`ForbiddenPathsEdited = 0`；Completion Evidence Presence 与 Path-Coverage-as-Proxy 两个假设
仍然是 FALSIFIED（它们的证伪不依赖 N3/N4 的隐藏验收正确性）。

## Corrections To Prior Documents

| 文档 | 原记录 | 更正 |
| --- | --- | --- |
| `C2_R1_...BASELINE.md` | "六次 FAIL 全部存在真实 implementation obligation gap" | **撤销。** N3 三次源于 case 不可满足；N4 三次只在欠定点上失败 |
| `C2_R1_...BASELINE.md` | "`session_id` 本轮只留存 2/6" | **更正为 6/6 确定性可恢复**（项目目录名内嵌 workspace 路径） |
| `C2_FINAL_ACCEPTANCE.md` | N3 分类 `CORRECT_PATCH_REVERTED` | 撤销动作属实，但触发它的是 case 自身的约束冲突，不是编辑不可靠 |
| `C2_FINAL_ACCEPTANCE.md` | N4 "任务要求的可观察行为从未实现" | **不成立。** footer 已实现且 stdout 路径正确报数；分歧只在 null sink |

R1 的 verdict 本身（`INCONCLUSIVE — insufficient PASS samples`）**不变** ——
它取决于 PASS = 0，与 fixture 有效性无关。

## Production Changes

```
NONE
```

未改动 Agent、Verifier、Runtime、navigation、`read_file`、compaction、plan、budgets、
权限、provider。未新增任何 Gate。**未修改 N3/N4 的 fixture** —— 修复不属于分类阶段。

## NEXT

```
C2-BV — Benchmark Validity Repair
```

这不是 Coding Agent 的产品工作，是 **Eval Integrity 的延伸**。

### Case Validity 的定义必须升级

现有闸门只验证两点，**不足以**防住 N3 这类缺陷：

```
untouched  → FAIL
reference  → hidden oracle PASS
```

必须升级为六项合取：

```
Case Validity =
      Broken State Demonstrably Fails
  AND Reference Solution Satisfies Full Task
  AND Maintained Repository Tests Pass
  AND Hidden Acceptance Passes
  AND Explicit Task Constraints Are Respected
  AND Requirement Semantics Are Unambiguous
```

N3 恰恰卡在第 3 与第 5 项（参考实现打破既有测试 ⊕ 任务禁止改测试），N4 卡在第 6 项。
**`reference PASS` 必须指完整 case PASS，不是单条 oracle PASS。**

### 范围

1. **N3 satisfiability repair** —— 修 fixture 本身，让既有测试的"正常记录"显式带
   `Valid: true`。既有测试仍在、仍禁止 Agent 改测试，但不再与新 contract 冲突。
2. **N4 semantic disambiguation** —— 二选一写死，不保留当前的模糊表达：
   - 若 intended contract 是"Sink 接受的数量"，任务文本须明写
     *"A null sink still accepts the records even though it discards their output"*，
     此时 oracle 取 `3` 才合法；
   - 若是"真实产生输出的数量"，则把 oracle 改成 null sink = 0。
3. **N1–N8 全部补齐双向 validity** —— N1/N2/N5/N7/N8 目前只有 `untouched FAIL`，
   与 N3 出事前的状态完全相同。**当前 C2 的 6/8 baseline 尚不能作为 stage acceptance 依据。**
4. **Tripwire** —— 把上述六项做成硬闸门，防止同类事故第四次发生。

**不在此处开始 C2-BV，也不开始 C3。**

```
C3: NOT STARTED
C4: NOT STARTED
```
