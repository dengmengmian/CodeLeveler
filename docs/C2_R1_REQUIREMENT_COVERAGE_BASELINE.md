# C2-R1 Requirement Coverage Baseline

日期：2026-08-09。分支 `feat/coding-context-efficiency-c2`。模型 `deepseek/deepseek-v4-flash`。

**结论前置：`C2-R1 = INCONCLUSIVE — insufficient PASS samples`。因为本批没有任何 PASS run，
数据只有一个 outcome class，无法判定 Implementation Coverage / Evidence Coverage 是否具备判别力。**

> [!WARNING]
> ## 两处记录已被 C2-SR 更正
>
> **1.「六次 FAIL 全部存在真实 implementation obligation gap」—— 撤销。**
> C2-SR 追 trajectory 时发现两个 case 本身失效：N3 经 `check_fixture_validity.py` 判定
> `INVALID`（语义正确的参考实现会打破既有的 `TestSummaryCountsPerName`，而任务同时要求
> 不得改测试且 `go test ./...` 必须通过 —— 三条约束不能同时成立）；N4 的隐藏验收唯一
> 判别点落在任务文本未消歧之处（null sink 该报 accepted=3 还是 written=0），而其余每一条
> 无歧义要求三次运行都满足了。**因此本文的 obligation FAIL 计数不能读作 Agent 能力结论。**
>
> **2.「`session_id` 本轮只留存 2/6」—— 更正为 6/6 确定性可恢复。**
> `~/.leveler/projects/` 的项目目录名内嵌完整 workspace 路径，每个 execution 一个目录、
> 一条 session，无需任何推断。
>
> **R1 的 verdict 本身不变** —— 它取决于 PASS = 0，与 fixture 有效性无关。
> 详见 `docs/C2_SCOPE_RECLASSIFICATION.md`。

## Baseline HEAD

```
branch  feat/coding-context-efficiency-c2
HEAD    f8b83eab913279d9453daa96963ae01646bcfc9e
```

| commit | 作用 |
| --- | --- |
| `becac4d2eaea8a18cc62635dcc646d8f9dcf2b20` | `fix(eval): isolate workspaces per case execution` |
| `f8b83eab913279d9453daa96963ae01646bcfc9e` | `test(eval): add obligation-level coverage measurement` |

两者均为 Eval / measurement 侧。**本轮生产代码改动为 NONE。**

## Purpose

回答 `docs/C2_COMPLETION_EVIDENCE_CONTROL_VARIANCE.md` §7 留下的开放问题：PASS/FAIL 到底由
**A. Implementation Obligation Coverage**、**B. Evidence Obligation Coverage**、两者兼需，还是
都不判别。**measurement-only —— 不构建任何生产机制。**

约束（沿用 control variance §9）：obligation ground truth 全程 metrics-only，绝不进入 Agent
context；`required_paths` 不得用作 obligation coverage 代理；轨迹分析必须 group by `session_id`。

## Measurement Integrity

| 项 | 结果 |
| --- | --- |
| Obligation oracle 双向证明 | **4/4 PROVEN** |
| Workspace isolation | **PASS**（6/6 独立保留） |
| Build sanity | **6/6 编译通过** |
| Hidden obligation metadata 进入 Agent context | **否**（metrics-only） |
| 生产代码改动 | **NONE** |

## Workspace Isolation

R1 之前，eval 工作区路径是 `leveler-eval-<case-id>-<pid>`，**不含 repetition**。同一进程的多次重复
因此复用同一目录，且进入时的 `remove_dir_all` 会抹掉上一次保留的树 —— 这使得"对最终实现树取证"
在多重复场景下根本不可靠。

`becac4d` 把工作区身份改为 **per-execution 唯一**：

```
leveler-eval-<case-id>-<pid>-exec<execution>-r<repetition>
```

`execution` 来自进程内单调递增的 `EXECUTION_SEQ`，因此 `compare` / `ablate` 在同一进程内重入
`run_eval` 也不会碰撞。同时输出确定性映射行：

```
kept workspace: case=<id> repetition=<n> session=<session_id> path=<path>
```

**禁止的替代做法**（均已在本项目造成过错误结论）：目录 mtime 推断、"最新目录"推断、
`case + PID` 推断、从 patch/event 重建最终实现树代替真实 final workspace。

### 本轮映射的实际留存程度（如实记录）

`case / repetition / workspace_path` 三元组**完整**，由路径本身确定性给出，无需推断。

`session_id` 本轮只留存 **2/6**（`8653a1c5`、`fe1770a2`）—— 不是机制缺陷，而是本轮 replay 的
stdout 被 `tail -24` 截断，前四行映射未被捕获，且 eval 的权威 JSON 不含 `session_id` 字段。
**未对缺失的四个做任何推断补齐。** 这直接限制了 Evidence Coverage 的可计算范围（见下）。

## Obligation Model

两个 case 各建两条行为级 obligation。承重字段是 `behavioral_surface`；`supporting_paths` 仅诊断。

| case | obligation | source | behavioral surface |
| --- | --- | --- | --- |
| N3 | `summary_skips_invalid` | user-stated | `report` 聚合渲染输出 |
| N3 | `distinct_skips_invalid` | **code-derived** | `report.Distinct` 返回值 |
| N4 | `sink_reports_written_count` | user-stated | `Sink` 的已写计数 |
| N4 | `cli_prints_footer_on_stderr` | user-stated | 进程 **stderr** |

`distinct_skips_invalid` 之所以标 code-derived：需求文本只说"report 层忽略 invalid 记录"，
`Distinct` 是该语义的第二个承载者，用户从未点名。这正是 N3 失败的那一条。

## Oracle Bidirectional Proof

一个只会失败的 oracle 什么都不测（N6 的教训），一个只会通过的 oracle 会把 harness 缺陷算到
Agent 头上。因此每条 obligation 的 oracle 必须双向证明：

```
未修改 fixture + overlay  →  oracle MUST FAIL
同上 + reference fix      →  oracle MUST PASS
```

在 HEAD `f8b83ea` 上复跑 `scripts/check_obligation_oracles.py`：

```
n3-caller-propagation:
  summary_skips_invalid              PROVEN (fails broken, passes fixed)
  distinct_skips_invalid             PROVEN (fails broken, passes fixed)
n4-interface-contract:
  sink_reports_written_count         PROVEN (fails broken, passes fixed)
  cli_prints_footer_on_stderr        PROVEN (fails broken, passes fixed)

all 4 obligation oracles proven in both directions
```

**OracleProven = 4/4。** 未达成双向证明的 obligation 报 `ORACLE_UNPROVEN` 且不得承载因果结论。

oracle 独立于"Agent 做了什么"：它只对最终工作区施加行为探测，不读取 Agent 的补丁、计划或叙述。
N4 的 `sink_reports_written_count` 用反射驱动 registry，因此任何形状正确的访问器都能通过 ——
它测行为，不测命名。

## N3 Results

| rep | exec | Implementation Coverage | `summary_skips_invalid` | `distinct_skips_invalid` |
| ---: | ---: | --- | --- | --- |
| 1 | 1 | **1/2** | FAIL | **PASS** |
| 2 | 2 | **0/2** | FAIL | FAIL |
| 3 | 3 | **0/2** | FAIL | FAIL |

rep1 的形态与 clean baseline 相反（当时是 `summary` 成立而 `Distinct` 守卫被撤销）。本批三次都
未满足 `summary_skips_invalid`。

## N4 Results

| rep | exec | Implementation Coverage | `sink_reports_written_count` | `cli_prints_footer_on_stderr` |
| ---: | ---: | --- | --- | --- |
| 1 | 4 | **0/2** | FAIL | FAIL |
| 2 | 5 | **0/2** | FAIL | FAIL |
| 3 | 6 | **0/2** | FAIL | FAIL |

### Measurement Note — 一次被撤销的 false-FAIL 怀疑

分析中我一度怀疑 N4 的 oracle 产生 false FAIL，理由是 exec4 的树里 `Sink` 接口含
`RecordsWritten() int`、`main.go` 含 `fmt.Fprintf(os.Stderr, "wrote %d records\n", …)`、
甚至还有 Agent 自写的 `count_test.go`。**该怀疑经复验后撤销。**

复验做法是直接运行产物：

```
$ printf '…3 条记录…' | ./navsvc conf   # output.sink = null
wrote 0 records
```

根因：**null sink 不递增已接受计数**。隐藏验收要求 `wrote 3 records`，实际输出 `wrote 0 records`，
两条 obligation 因同一个缺口一起失败 —— footer 存在但数值错误，计数方法存在但不计数。

因此：

```
sink_reports_written_count  = REAL IMPLEMENTATION FAIL
cli_prints_footer_on_stderr = REAL IMPLEMENTATION FAIL
```

**不是 harness false negative。** 此条记录用于防止后续引用那一版错误判断。

## Build Sanity

六个最终工作区逐个 `go build ./...`：

```
n3 exec1-r1  OK      n4 exec4-r1  OK
n3 exec2-r2  OK      n4 exec5-r2  OK
n3 exec3-r3  OK      n4 exec6-r3  OK
```

**6/6 通过。** 因此 implementation obligation 的 FAIL **不能**归因于 build failure、
broken workspace 或 incomplete checkout。这一步是必须的：否则"树根本编译不过"会伪装成
"义务未实现"，产生一批看起来干净实则无意义的 0/2。

## Implementation Coverage

```
N3:  1/2   0/2   0/2
N4:  0/2   0/2   0/2
```

12 条 obligation 实例中满足 1 条。

## Evidence Coverage

```
NOT USED FOR DISCRIMINATIVE CONCLUSION
```

原因有二，都不通过继续实验来消除：

1. **判别力问题本身已不可计算**（PASS = 0，单一 outcome class）。即使补齐 Evidence Coverage，
   它也无法进入任何相关性结论。
2. 本轮 `session_id` 只留存 2/6（见上）。Evidence Coverage 必须按 `session_id` 切分事件日志，
   而按目录 mtime / 最新目录 / `case + PID` 对齐正是 control variance §2 明令禁止、且历史上
   真的产出过方向相反结论的做法。

**未为了填表继续实验。**

## Outcome Class Distribution

```
PASS = 0
FAIL = 6
```

对照：修复 workspace isolation 之前的同配置 control 是 3 PASS / 3 FAIL。本轮 0/6 是观察事实，
未就其原因下结论。

## What Is Proven

> 本节第一条已被 C2-SR 撤销（见顶部横幅）：六次失败**不能**读作真实的 implementation gap，
> 因为两个 case 的判别点本身失效。以下保留其余仍然成立的部分。

- ~~本批六次失败全部含有真实的 implementation obligation gap。~~ **撤销**
- Obligation-level oracle **能定位**整体隐藏验收 FAIL 内部究竟哪一条行为需求未满足
  （N3 rep1：`distinct` 成立而 `summary` 不成立）。该定位能力本身成立，与 case 是否有效无关。
- **oracle 双向证明有一个已知盲区**：它用 `go test -run <单条 obligation 测试>`，不跑该包的
  既有测试，因此无法发现"case 在其自身约束下不可满足"。N3 正是这样漏过去的。
  证明 oracle 与证明 case 可满足**必须分开做**。
- **Path Coverage 仍然不能代理 Implementation Obligation Coverage。** N3 在 clean baseline 上
  路径 2/2 touch、2/2 FullyRead、两文件均被修改，隐藏验收仍失败。

## What Is NOT Proven

```
IMPLEMENTATION_COVERAGE_DISCRIMINATES = NOT ESTABLISHED
EVIDENCE_COVERAGE_DISCRIMINATES       = NOT ESTABLISHED
BOTH_REQUIRED                         = NOT ESTABLISHED
```

以及：**任何生产 completion 机制均未被证明。** 本轮没有提出、也没有实现任何 gate。

需要单独说明一点：即便本批出现 PASS 样本，Implementation Coverage 的判别力也天然虚高 ——
隐藏验收本身就是这些 obligation 的合取，二者不独立。它的价值在于**定位漏了哪一条**，
不在于充当判别器；而且它依赖隐藏 oracle，**在生产中不存在对应物**。

## Statistical Limitation

```
single-class sample  (PASS = 0)
```

无正类样本 ⇒ 任何 discriminative power 都无法计算，无论 A、B 还是 A∧B。这是数据结构性的限制，
不是分析方法的不足。

## Why Sampling Stops Here

**不继续跑 N3/N4 直到撞出 PASS。** 那会把停止条件变成"跑到出现想要的类别为止"，即
outcome-dependent optional stopping —— 用 benchmark 去凑一个预期结论，测量可信性直接归零。

**不给 N1/N2/N5–N8 新建 obligation model。** 那会把 R1 从"刻画 blocking completion failure"
扩成"整个 benchmark 的 obligation 本体工程"，明显放大 C2 scope，且不保证解决当前 blocker。

## R1 Verdict

```
Measurement Integrity     PASS
Obligation Oracle         PASS   (4/4 PROVEN)
Workspace Isolation       PASS
Build Sanity              PASS   (6/6)
Observed Implementation Gaps   CONFIRMED
Discriminative Power      INCONCLUSIVE

C2-R1: INCONCLUSIVE — insufficient PASS samples
```

判别问题无法回答的完整原因（第一条为本轮结论，后两条由 C2-SR 补充）：

```
- PASS = 0                          单一 outcome class，判别力在数学上不可计算
- N3 INVALID / UNSATISFIABLE        隐藏验收与任务约束不能同时成立
- N4 SEMANTICALLY UNDER-SPECIFIED   唯一判别点落在任务文本未消歧之处
```

**不因 benchmark 缺陷把 R1 改写成 FAIL** —— 判别问题本来就没有被回答出来，
新增的两条只是让"为什么没回答出来"更完整。

> All six observed FAIL runs contain genuine unsatisfied implementation obligations.
> However, because this replay produced zero PASS runs, the dataset contains only one
> outcome class and cannot establish whether Implementation Coverage, Evidence Coverage,
> or their combination discriminates successful from unsuccessful completion.

**R1 不是 FAIL。** 不得写 `OBLIGATION_MODEL_NOT_DISCRIMINATIVE` —— obligation model 没有被
证伪，是样本不足以回答判别问题。R1 既不是 PASS 也不是 FAIL，是 **INCONCLUSIVE**。

## Instrumentation Retained

`becac4d`、`f8b83ea` **保留，不因 R1 inconclusive 而回滚**。二者已被证明是有效的 Eval
correctness / measurement infrastructure：前者修掉了一个会静默破坏取证的真实缺陷，
后者的 oracle 已双向证明。

obligation metadata 继续保持 **metrics-only，不得进入 Agent context**。

## Production Changes

```
NONE
```

未改动 Agent、Verifier、Runtime、navigation、`read_file`、compaction、plan、budgets。
未新增 Completion Gate / Requirement Gate / LLM judge。

## NEXT

```
C2 scope / blocker reclassification
```

需要重新判断：当前剩余的 N3/N4 failure 究竟仍属于 C2（context / evidence 能力），
还是应转交 **C3 — implementation/edit reliability** 或其它明确的 capability slice。

**不在本文件中提出任何具体生产 Gate，也不开始该判断。**
