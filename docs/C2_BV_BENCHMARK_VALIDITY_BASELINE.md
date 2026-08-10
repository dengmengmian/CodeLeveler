# C2-BV Benchmark Validity Baseline

日期：2026-08-09（BV0）/ 2026-08-10（BV1）。分支 `feat/coding-context-efficiency-c2`。

**结论前置：BV1 已完成 —— N1–N8 现为 `VALID 8/8`，checker exit 0，benchmark ready。**
修复前的真实健康度是 `VALID 1/8`（仅 N6），本文前半部分保留该 pre-repair 基线，
§BV1 Repairs 记录每个 case 的修复。**全程未跑任何模型，生产代码零改动** ——
8/8 只证明尺子可用，不证明 Agent 会通过它。

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

### Loader Compatibility —— 以及它立刻抓到的一个真实破坏

新增的 `validity:` metadata 必须对真实 Eval loader 透明。这一点**不接受静态推断**
（"`EvaluationCase` 没有 `deny_unknown_fields`，所以应该会忽略"），而是跑真实 loader：

`crates/leveler-eval/src/lib.rs::navigation_cases_still_load_with_benchmark_validity_metadata`
用 `EvaluationCase::load_dir` 加载 `evals/navigation`，断言 8 个 case 全部到齐且
`task` / `expect` / navigation metadata 未受影响；再用 `EvaluationCase::load` 加载一个
只带 `validity:` 的最小 case。

**这条测试一加上就红了，而且不是因为 `validity:`。**

`EvaluationCase::load_dir` **递归**遍历目录，并要求其下**每个** `.yaml` 都解析成合法 case。
`f8b83ea` 把 obligation metadata 放在 `evals/navigation/obligations/`，于是全树加载直接失败：

```
Parse("missing field `id` at line 11 column 1")
```

受影响的既有测试有两个 —— `scenario_suite_parses_and_ids_are_unique_across_the_tree` 与
`root_suite_is_recursive_and_covers_all_first_class_languages`。**它们从 `f8b83ea` 起就是红的，
并且已经推到 origin**：那次提交只跑了 Python 侧的 oracle 证明，没有跑 `cargo test`。

修复：obligation 文件不是 eval case，移出 case 树到 `fixtures/navigation-obligations/`
（`scripts/check_obligation_oracles.py` 的 `OBLIGATIONS_DIR` 同步更新）。
修复后 `cargo test -p leveler-eval --lib` **58 passed / 0 failed**，
4 条 obligation oracle 在新路径仍全部 PROVEN。

> 这正是本阶段的论点自证一次：**静态推断不是证据。** 我原本准备以
> "serde 结构上会忽略未知字段"结案，而真实 loader 一跑就暴露了一个与该推断无关、
> 但已经影响 origin 的破坏。

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
compaction、provider、completion 逻辑。

Eval 侧的改动有三处，都不属于 Agent 产品行为：case 的 `validity:` metadata、
`crates/leveler-eval` 里新增的 loader regression（**仅测试**，`EvaluationCase` 本身未改）、
以及把 obligation metadata 移出 case 树。工作区闸门：`cargo fmt --check` ✅ ·
`cargo check --workspace --all-targets` ✅ 0 error / 0 warning ·
`cargo test -p leveler-eval --lib` ✅ 58 passed / 0 failed。本阶段也未改 N3 fixture、N4 task/expect/reference，
未新增任何 reference patch。**未运行任何模型。**

## BV1 Repairs

日期：2026-08-10。`BV1_BASELINE_HEAD = 42edec3b34d55fceb76adf13b086a0ec4cdf7bb1`。
顺序 N3 → N4 → N1 → N2 → N5 → N7 → N8；每修一个 case 立即跑该 case 的六项闸门 +
真实 `EvaluationCase` loader regression，`VALID` 才进下一个。N6 未改动，仅作 positive regression。

### N3 —— satisfiability repair（`6753938`）

| | |
| --- | --- |
| before | `INVALID`：正确 guard 打破 `TestSummaryCountsPerName`，任务禁止改测试 |
| defect | 该测试的记录未设 `Valid`，Go 零值让"正常记录"意外变成 invalid —— 测试本意是数数，不是验证 validity 语义 |
| repair | **修 fixture 生成器**：三条记录显式 `Valid: true`，重新生成 `navsvc` 与 `navsvc-n6` 两仓。任务约束一字未动，隐藏验收强度未降 |
| result | 六项全 PASS；N6 仍 VALID；N3 两条 obligation oracle 仍双向 PROVEN |

### N4 —— semantic disambiguation（`7d5fef5`）

| | |
| --- | --- |
| before | `UNDER_SPECIFIED`：任务同时说 "actually accepted"（→3）与 "really written / free to reject"（→0），隐藏验收静默取 3 |
| repair | 任务文本改为只支持 accepted-count 一种读法：成功的 `Write` 即接受其记录、null sink 照数它故意丢弃的记录、失败的 `Write` 不贡献计数。"really written" 措辞删除。**oracle / reference / 隐藏验收未动** —— 它们本就编码 accepted-count |
| result | G6 attestation → `UNAMBIGUOUS`；六项全 PASS；N4 两条 oracle 仍 PROVEN |

### N1 N2 N5 N7 N8 —— references（`dfd1063`）

五个 case 首次获得"可解"证明。每个 reference 都是对着 materialize 后的 workspace
（clone+overlay）写的最小产品修复，不特判任何隐藏 fixture：

| case | reference | 覆盖的 obligation |
| --- | --- | --- |
| N1 | normalize 阶段恢复 `strings.ToLower`（1 行） | 大小写折叠；whitespace 处理由既有路径保持 |
| N2 | `Render` 末尾追加 `TOTAL count=%d total=%g`，空输入也输出 | TOTAL 行、位置、空集语义 |
| N5 | `PipelineConfig.MinValue` + `pipeline.min_value` 解析 + 负值校验 + filter 阶段内阈值 | 配置贯通、缺省不变、负值报错、仅随 filter 生效 |
| N7 | `OutputConfig.MaxRecords` + 解析/校验 + **写循环处**封顶（跨 sink 生效、跨 batch 裁剪、达到即停写） | 精确 cap、0=无限、`--summary` 不受影响 |
| N8 | `rowName()`：带 `env` label 的记录按 `name{env=v}` 记账 | 分组、排序随打印名、格式不变 |

每个都在进入下一个 case 之前单独跑到 `VALID`。

## Final Validity Matrix

日期：2026-08-10。`check_fixture_validity.py` 全量运行，**退出码 0**：

```
case                                G1    G2    G3    G4    G5    G6   status
n1-unknown-location               PASS  PASS  PASS  PASS  PASS  PASS   VALID
n2-competing-implementations      PASS  PASS  PASS  PASS  PASS  PASS   VALID
n3-caller-propagation             PASS  PASS  PASS  PASS  PASS  PASS   VALID
n4-interface-contract             PASS  PASS  PASS  PASS  PASS  PASS   VALID
n5-config-to-runtime              PASS  PASS  PASS  PASS  PASS  PASS   VALID
n6-large-file-region              PASS  PASS  PASS  PASS  PASS  PASS   VALID
n7-cross-package                  PASS  PASS  PASS  PASS  PASS  PASS   VALID
n8-distractor-resistance          PASS  PASS  PASS  PASS  PASS  PASS   VALID

VALID 8/8
```

同时全绿：7 条 checker gate regression、4 条 obligation oracle（双向）、
`cargo test -p leveler-eval --lib` 58 passed / 0 failed（含 loader regression）。

Answer-key containment：reference 从 3 个扩到 8 个不需要额外密封 ——
`seal_eval_answer_keys()` 按 subpath 密封整个仓库根，新文件自动在拒读范围内。

## BV1 之后仍然成立的边界

- **8/8 VALID 只证明尺子可用**，不证明 Agent 会通过任何一个 case。
- C2 PRODUCT VERDICT 仍是 `NOT CURRENTLY ADJUDICABLE` —— 修 benchmark 不是重新测产品。
- 生产代码零改动，模型运行 0 次。

## NEXT

```
C2-BV2 — Re-establish trustworthy C2 product baseline
```

用修好的尺子重新跑 Agent。不在本阶段开始。
