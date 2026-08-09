# C2 — Completion Evidence: Control Variance & Hypothesis Falsification

日期：2026-08-09。分支 `feat/coding-context-efficiency-c2`。`HEAD = d80c691`。
模型 `deepseek/deepseek-v4-flash`，默认产品行为，**未改动任何生产代码**。

**结论前置：`COMPLETION_WITHOUT_BEHAVIORAL_EVIDENCE` 被本轮数据证伪。行为证据的"存在"对成败没有正判别力，本轮方向甚至相反。**

---

## 1. 实验设置

N3 与 N4 各 3 次 control，同一模型 / provider / fixture / budget / verifier / prompt / Eval containment / 生产代码。除重复次数外无任何变量。

## 2. Session Alignment Methodology（先说方法，因为它改过一次结论）

eval 的工作区与 project 目录路径是 `leveler-eval-<case-id>-<pid>` —— **不含 repetition**。同一进程的 3 次重复因此**共用一个 project 目录**，其 `sessions.db` 里有 **3 条 session**。

> **`session_id` 是 run identity。project 目录不是。**

本轮最初按目录 mtime 取"最近 3 个目录"来对齐 repetition，结果把更早的运行混了进来，**得出过一版方向相反的相关性**。改按 `sessions` 表逐 `session_id` 切分后才得到下表。

这是一次真实的 measurement integrity 缺陷，不是脚本便利性问题。任何后续轨迹分析器**必须 group by `session_id`**，禁止用以下任一方式猜 repetition：目录 mtime、最新目录、project 路径、`case + PID`。

## 3. 3×3 Control Matrix

结果取自 eval 的权威 JSON（逐 repetition）；行为证据计数取自按 `session_id` 切分的事件日志。

**行为证据候选**的判据：该次运行是否**真正执行了产物** —— `go run ./cmd/navsvc`，或构建出二进制（`-o …navsvc…`）后运行它。只读检查（`git show`、`sed`、`grep`）不计。

| Case | rep | Hidden Acceptance | Completed | FalseCompletion | 命令数 | **行为证据候选** |
| --- | ---: | --- | --- | --- | ---: | ---: |
| **N3** | 1 | ✅ PASS | ✅ | — | 6 | **0** |
| **N3** | 2 | ✅ PASS | ✅ | — | 16 | **0** |
| **N3** | 3 | ❌ FAIL | ✅ | **YES** | 9 | **3** |
| **N4** | 1 | ❌ FAIL | ✅ | **YES** | 8 | **2** |
| **N4** | 2 | ❌ FAIL | ✅ | **YES** | 4 | **3** |
| **N4** | 3 | ✅ PASS | ✅ | — | 7 | **4** |

| 汇总 | 值 |
| --- | --- |
| N3 | 2 PASS / 1 FAIL |
| N4 | 1 PASS / 2 FAIL |
| **FalseCompletion** | **3** |
| 诚实未完成（Incomplete / Unverified） | **0** |

**每一次失败都是 FalseCompletion。** 六次运行全部以 `Completed` 结束，没有任何一次诚实地停在未完成。

---

## 4. 被证伪的假设

原假设（写入过 `docs/C2_FINAL_ACCEPTANCE.md`）：

> `COMPLETION_WITHOUT_BEHAVIORAL_EVIDENCE` —— Agent 在只有 build / 既有测试等通用绿检查、没有针对所要求行为的证据时宣布完成。

**数据不支持：**

| 观察 | 值 |
| --- | --- |
| N3 两次 PASS 的行为证据候选 | **0 / 0** |
| N3 一次 FAIL 的行为证据候选 | **3** |
| N4 两次 FAIL 的行为证据候选 | **2 / 3** |
| N4 一次 PASS 的行为证据候选 | 4 |

失败的运行**并不缺**行为证据 —— 它们构建了二进制、喂了输入、观察了输出。而 N3 两次成功的运行**一次都没有执行过产物**。

> **Evidence Presence 对成败没有正判别力；在 N3 上方向相反。**

因此：

- `COMPLETION_WITHOUT_BEHAVIORAL_EVIDENCE` = **FALSIFIED**
- **不得实现 Completion Evidence Presence Gate。** 它会漏掉 N4 的两次失败（证据已存在且成功执行），并误触发 N3 的两次成功（无行为证据却做对了）—— 两个方向都错。

---

## 5. N3 / N4 的准确根因

两者都不是"缺少行为证据"。准确描述是 **Requirement / Obligation Coverage mismatch**。

### N3 —— 证据真实，但覆盖不全

| | |
| --- | --- |
| 需求 | report 层对 invalid 记录的语义必须保持正确 |
| 实际观察到的证据 | CLI 行为（构建二进制并喂输入） |
| 遗漏的 obligation | **`report.Distinct`** |
| 为什么漏 | 该 obligation **不经过所观察的 CLI surface** |

### N4 —— 证据真实，但观察错了行为面

| | |
| --- | --- |
| 需求 | 输出 footer `wrote N records` |
| 实际观察到的证据 | CLI 生产路径 + **stdout** |
| 需求所在行为面 | **stderr** |
| 为什么漏 | 证据执行正确，但观察的是错误的 behavioral surface |

**不要把这两者重新统称为"缺少行为证据"。**

---

## 6. Path Coverage 不能代理 Obligation Coverage

N3 是一个确定性反例：

| 指标 | 值 |
| --- | --- |
| `required_impact_paths` | `internal/report/summary.go` · `internal/report/aggregate.go` |
| Path Touch | **2/2** |
| FullyRead | **2/2** |
| 两个文件都被修改 | **是** |
| 最终结果 | `aggregate.go` 的 `Distinct` guard **被撤销**，隐藏验收 FAIL |

> **路径覆盖满分，需求仍未满足。**

因此：`required_paths` 只能作为 **supporting / diagnostic metadata**。**禁止**从"路径已覆盖"推出"行为 obligation 已满足"。

未来若引入 `required_obligations`，其 ground truth **必须是行为级**：

```yaml
required_obligations:
  - id: distinct_skips_invalid
    behavior: ...
    behavioral_surface: ...      # ← 承重字段
    supporting_paths:            # ← 仅诊断
      - internal/report/aggregate.go
```

---

## 7. 一个重要的开放问题（不下结论）

**观察**：N3 的两次 PASS 完全没有执行过产物级 behavioral probe，但最终实现满足全部隐藏验收。

**含义**：成功可能来自**实现本身覆盖了全部 obligation**，而不是存在某种独立的行为证据。

**开放问题**：后续研究必须区分两层，并找出**哪一层能区分 PASS/FAIL**：

| 层 | 含义 |
| --- | --- |
| **A. Implementation Obligation Coverage** | 最终代码是否覆盖了全部 obligation |
| **B. Evidence Obligation Coverage** | 已获取的证据是否足以支持上述判断 |

**不要预设成功必须来自 behavioral evidence。** 研究问题已经从"是否存在行为证据"升级为"最终实现是否覆盖全部 obligation，以及现有证据是否足以支持这个判断"。

---

## 8. 当前 blocker

```
FALSIFIED:        COMPLETION_WITHOUT_BEHAVIORAL_EVIDENCE
CURRENT BLOCKER:  REQUIREMENT_TO_EVIDENCE_ALIGNMENT
NEXT:             C2-R1 — Requirement Coverage & Evidence Alignment Baseline
STATUS:           NOT STARTED
```

**`REQUIREMENT_TO_EVIDENCE_ALIGNMENT` 只是下一个研究问题，不是已被证明的生产修复方案。**

## 9. R1 前置条件

1. 轨迹分析必须 **group by `session_id`**（§2）。
2. `required_paths` 不得用作 obligation coverage 代理（§6）。
3. 必须分别测量 Implementation Coverage 与 Evidence Coverage（§7）。
4. 任何 obligation ground truth 是 **metrics-only**，绝不进入 Agent context。

## 10. 本轮未做

未改动生产代码：verifier 语义、Agent 完成逻辑、navigation、`read_file`、compaction 均未触碰。未实现任何 Gate。未开始 C2-R1。
