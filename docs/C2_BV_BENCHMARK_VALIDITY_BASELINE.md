# C2-BV Benchmark Validity Baseline

日期：2026-08-09。分支 `feat/coding-context-efficiency-c2`。
**BV0 —— 只造尺子、跑基线。未修任何 case，未跑任何模型，生产代码零改动。**

**结论前置：修复前 N1–N8 的真实健康度是 `VALID 1/8`。** 唯一有效的是 N6。
在此之前所有基于这套 benchmark 的能力结论（含 C2 的 6/8）都缺乏成立基础。

## HEAD

```
branch                feat/coding-context-efficiency-c2
C2_BV0_BASELINE_HEAD  10c9478d264abac1ce783c863653926cdb67c8d1
```

## Why BV Exists

同一类缺陷已经出过三次，每次都绕过了当时在位的检查。BV 的前提是：
**先把尺子造正确，再碰 benchmark case。**

## Previous Blind Spots

### N6 —— 描述的缺陷不存在

隐藏验收在未修改的 workspace 上就通过，case 在给"修复一个从未损坏的东西"打分。
那次"失败"的运行其实行为正确。
→ 催生了 `broken must FAIL` 这一半闸门。

### N3 —— 语义正确的实现打不过维护中的测试

既有 `TestSummaryCountsPerName` 喂 `Valid` 未设的记录并期望被计入；任务要求忽略 invalid、
不得改测试、`go test ./...` 必须通过。三条约束不能同时成立。
**没有任何检查看得见它** —— 隐藏验收只跑它自己关心的那部分，
obligation oracle 只跑 `-run <单条测试>`。

### N4 —— 判别点落在语义未定处

隐藏验收整体压在"null sink 报 accepted=3 还是 written=0"，而任务同时使用
"actually accepted" 与 "what was really written / free to reject"。
reference 全绿，case 依然不能算有效。

> **证明 oracle ≠ 证明 case。** 一个满足自身隐藏验收的 reference，仍可能违反任务的显式约束、
> 打破仓库自身的套件，或编码多种合理读法之一。

## Six-Gate Definition

```
Case Validity =
      G1 Broken State Demonstrably Fails
  AND G2 Reference Solution Satisfies Full Task
  AND G3 Maintained Repository Checks Pass
  AND G4 Hidden Acceptance Passes
  AND G5 Explicit Task Constraints Are Respected
  AND G6 Requirement Semantics Are Unambiguous
```

| 状态 | 含义 |
| --- | --- |
| `VALID` | 六项全部成立 |
| `INVALID` | 任意一项明确失败 |
| `UNDER_SPECIFIED` | 可执行维度全绿，但语义允许第二种合理读法 |
| `UNVERIFIED` | 无法证明（当前主要是缺 reference） |

**`UNVERIFIED` 与 `UNDER_SPECIFIED` 都不是 `VALID`，都不得被静默提升。**

## Operational Semantics of Each Gate

| Gate | 怎么判 |
| --- | --- |
| **G1** | clone + overlay 的真实 eval workspace，不打 reference，跑隐藏验收 —— 必须非零退出 |
| **G2** | **派生量**，不独立测量：`G3 ∧ G4 ∧ G5 ∧ G6` 全 PASS 才 PASS。"patch 能 apply"不叫 full task satisfied |
| **G3** | 打上 reference 后跑 `validity.maintained_checks`（当前为 `go build ./...` + `go test ./...`）。**与隐藏验收是两个独立维度** —— N3 的隐藏验收只跑 `go test ./internal/report/`，冲突就藏在同一个包的既有测试里 |
| **G4** | 打上 reference 后跑完整隐藏验收 —— 不是单条 obligation oracle |
| **G5** | 机械检查两件事：reference diff 不得触碰 `forbidden_edit_paths`；若 case 声明 `existing_tests_unmodified`，则 baseline 与 reference 之间所有既有 `*_test.go` 必须逐字节一致 |
| **G6** | **benchmark-author attestation**，不做自动推断 |

### G5 比较的是哪个 diff

`baseline workspace → reference patch`，其中 baseline 已经包含 benchmark author 的
clone + overlay。因此**benchmark 作者修 fixture 不会被误判成"解法在改测试"**，
被检查的只有 reference solution 自己引入的改动。

### G6 为什么不自动化

禁止让模型读任务文本后宣布 `UNAMBIGUOUS` 并当作硬事实 —— 那是把解释权交回给待测对象。
每个 case 在 `validity.semantics` 里记录四项：`status` / `critical_observable` /
`oracle_interpretation` / `rationale`。

判定规则：**若任务文本允许两种合理实现，而隐藏验收只接受其一，则即使 reference、
隐藏验收、既有测试全绿，case 仍不得 `VALID`。**

### 六个 Gate 不要求统计独立

G2 显式依赖 G3/G4/G5/G6 的结果。报告分列六个维度是为了定位缺陷在哪一层，
不是为了构造六个正交指标，因此没有为此写六套重复 runner。

## Tripwire Implementation

唯一 authoritative 命令仍然只有一条，未新建任何平行 benchmark runner：

```
python3 scripts/check_fixture_validity.py --cases evals/navigation
```

- 仅当 N1–N8 **全部 VALID** 时 exit 0；否则非零。
  修复前实测退出码 = **1**（此前 `UNVERIFIED` 不进 invalid 列表、可能输出
  "no invalid fixture" 并 exit 0，该行为已被移除）。
- **不 fail-fast**：一个 case 失败后继续扫完全部，一次给出完整矩阵。
  否则会变成"修完 N3 才发现 N5，修完 N5 才发现 N7"。
- 输出结构化：每个 case 给出六列 gate + `final_status` + `reason_codes`，
  `--json` 可导出。

case 侧新增 `validity:` metadata（`maintained_checks` / `constraints` /
`semantics`），**metrics-only**。

## Regression Coverage

`scripts/test_fixture_validity_gates.py` —— 每条对应一个真实发生过的缺陷结构，
跑在 synthetic fixture 上，因此 N1–N8 被修复时 tripwire 不会失效或变得空洞：

| regression | 断言 |
| --- | --- |
| `BROKEN_FIXTURE_PASSES` | 未修改状态就通过 → `INVALID`（N6 类） |
| `MAINTAINED_TEST_CONFLICT` | 隐藏验收绿、维护套件红 → `INVALID`，G3 = FAIL（**N3 类**） |
| `SEMANTICS_UNDER_SPECIFIED` | 可执行维度全绿但语义有争议 → `UNDER_SPECIFIED`，非 `VALID`（N4 类） |
| `REFERENCE_MISSING` | 无 reference → `UNVERIFIED`，G2 = FAIL |
| `EXISTING_TEST_MODIFIED` | reference 改写既有测试 → `INVALID`，G5 = FAIL |
| `FORBIDDEN_PATH_EDITED` | reference 触碰禁改路径 → `INVALID`，G5 = FAIL |
| `VALID`（**正类对照**） | 六项全绿确实产出 `VALID` —— 没有它，一个"永远判 INVALID"的 checker 能通过上面全部 6 条 |

实测 **7/7 通过**。

## N1-N8 Pre-Repair Matrix

```
case                                G1    G2    G3    G4    G5    G6   status
n1-unknown-location               PASS  FAIL   n/a   n/a   n/a  PASS   UNVERIFIED
n2-competing-implementations      PASS  FAIL   n/a   n/a   n/a  PASS   UNVERIFIED
n3-caller-propagation             PASS  FAIL  FAIL  FAIL  PASS  PASS   INVALID
n4-interface-contract             PASS  FAIL  PASS  PASS  PASS  FAIL   UNDER_SPECIFIED
n5-config-to-runtime              PASS  FAIL   n/a   n/a   n/a  PASS   UNVERIFIED
n6-large-file-region              PASS  PASS  PASS  PASS  PASS  PASS   VALID
n7-cross-package                  PASS  FAIL   n/a   n/a   n/a  PASS   UNVERIFIED
n8-distractor-resistance          PASS  FAIL   n/a   n/a   n/a  PASS   UNVERIFIED
```

| 汇总 | |
| --- | --- |
| **VALID** | **1/8**（仅 N6） |
| INVALID | 1（N3） |
| UNDER_SPECIFIED | 1（N4） |
| UNVERIFIED | 5（N1 N2 N5 N7 N8） |

**G1 八个全 PASS** —— 每个 case 的未修改状态确实会失败，没有第二个 N6。

## Invalid Cases

**N3** —— `MAINTAINED_CHECKS_FAIL` + `REFERENCE_HIDDEN_ACCEPTANCE_FAIL`。

reference 打上后 `go test ./...` 报：

```
--- FAIL: TestSummaryCountsPerName (0.00s)
    summary_test.go:17: unexpected report: ""
```

隐藏验收本身也 FAIL —— 它末尾跑 `go test ./internal/report/`，而冲突正在该包内。
G5 = PASS（reference 没有去改测试来买绿，它只是过不了），G6 = PASS（语义本身清楚）。
**缺陷定位在 G3/G4，不在语义。**

## Under-Specified Cases

**N4** —— G1/G3/G4/G5 全 PASS，只有 G6 FAIL。

这正是新闸门相对旧闸门的增量：旧闸门会判它 `VALID`（reference 双向通过），
新闸门因语义争议拒绝放行。

## Unverified Cases

**N1 N2 N5 N7 N8** —— 全部只因 `REFERENCE_MISSING`。

按 §14 本阶段**不补** reference，先看清修复前真实矩阵。这五个 case 的
G3/G4/G5 目前无法评估（标 `n/a`），G1 与 G6 已经评估且都 PASS。

> 这意味着 C2 曾经的 `6/8` 里，有五个 case 从未被证明**可以被解出来**。
> 它们与 N3 出事前的状态完全相同。

## N4 Future Semantic Decision

已决定的 intended contract（**NOT APPLIED IN BV0**）：

```
accepted = records passed to a Sink.Write call that returns success
physical output is irrelevant
a null sink accepts records even though it deliberately discards output
a Write returning error does not contribute accepted records
the current Sink interface has no partial-acceptance result
```

因此未来 N4 修复后：null sink + 3 条输入 → `wrote 3 records`。

理由：任务首先说的就是 `actually accepted`，case 主题是 interface contract，
且现有 reference 的接口注释已写成 "how many records this sink accepted"。
BV1 需删除/改写 `really written` 与 `free to reject or coalesce` 这两处模糊措辞。

**BV0 未改 N4 的 task / expect / reference**，让 tripwire 诚实报出
`SEMANTICS_UNDER_SPECIFIED`，留下修复前基线。

## What This Does NOT Prove

- **不证明** Coding Agent 的任何能力。
- **不证明** C2 PASS / FAIL —— C2 仍是 `NOT CURRENTLY ADJUDICABLE`。
- **不证明** C3 根因。
- **不证明** 任何 production treatment。
- **不证明** 修完这些 case 之后模型会通过它们。

本阶段只证明了一件事：**尺子现在能报出自己坏在哪里。**

## Answer-Key Containment

新增的 `validity` metadata、语义 rationale、reference contract、约束声明
**全部只用于 benchmark authoring / offline preflight / post-run analysis**，
不进入 Agent prompt、task text、ToolResult、ContextSnapshot、workspace 或环境变量。
Eval answer-key containment 继续成立。

## Production Changes

```
NONE
```

未改动 leveler-agent、Verifier、Runtime、ToolHost、navigation、`read_file`、
compaction、provider、completion 逻辑。本阶段也未改 N3 fixture、N4 task/expect/reference，
未新增任何 reference patch。**未运行任何模型。**

## NEXT

```
C2-BV1 — Repair invalid / under-specified / unverified cases
```

不开始 BV1。
