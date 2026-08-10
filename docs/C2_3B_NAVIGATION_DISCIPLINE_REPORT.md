# C2.3B NAVIGATION DISCIPLINE REPORT

日期：2026-08-09。分支 `feat/coding-context-efficiency-c2`。模型 `deepseek/deepseek-v4-flash`，默认产品行为，无 ablation。

**结论前置：C2.3B = FAIL。** 导航纪律按规格完整实现并通过全部确定性测试，正确性与安全语义零退化 —— 但在 11 个配对 case 上**一致地更贵**（轮次 +17%、token +28%，7 涨 3 跌 1 平），在唯一的真实仓 case 上**导航质量下降**（信息增量 91% → 81%，重复确认 0 → 6 次），而**所有主指标（recall、impact coverage、half-fix、hidden acceptance）完全持平**。§36 的"证据足够后能更快停止低价值探索"未达成，且方向相反。

---

## 1. Baseline

`docs/C2_3B_NAVIGATION_DISCIPLINE_BASELINE.md` 的结论：

| 纪律 | 改动前 |
| --- | --- |
| Search First | **CONFLICTING** |
| Progressive Read | **MISSING** |
| Impact Expansion | **MISSING** |
| Exploration Convergence | **MISSING** |

最关键冲突 **C1**：C2.3A 已在引擎侧删除硬墙，但提示词仍称只读探索为 "optional"、称 `update_plan` 为 "first substantive action" —— 引擎放行，提示词踩刹车。

---

## 2. Production Guidance

三处改动（commit `13ff592`），全部在 §20 允许范围内：

| # | 位置 | 内容 |
| --- | --- | --- |
| 1 | `prompts/base.md` 新增 `## Code navigation` | 十条纪律，见 §3–§8 |
| 2 | `prompt.rs` `require_explicit_plan` 块 | 删除 "optional read-only explore" / "first substantive action" 措辞；plan 要求收窄到 "before you start changing files"；明确"定位与阅读不需要 plan" |
| 3 | `grep` / `read_file` 描述 | 见 §9。schema 未动 |

**注入位置的选择**（§17）：`base.md` 是唯一同时满足 harness baseline、模型无关、非 eval-only、非 provider-specific、非条件注入的位置。`base_instructions` 是 per-model 覆盖，`require_explicit_plan` 块是条件注入 —— 都会让导航纪律只对部分场景生效。

---

## 3. Location Confidence

未新增 Rust enum（§4 允许）。三级作为执行纪律表达在提示词首条：

| 等级 | 触发 | 纪律 |
| --- | --- | --- |
| KNOWN | 用户点名文件/符号/行，或编译/测试错误给出位置 | "read it directly; a ceremonial search first is wasted" |
| UNKNOWN | 只有行为/bug/需求描述 | "locate before reading broadly" + 点名 `grep` / `find_files` / `find_symbol` / `find_references` |
| LIKELY | 有候选但未确认 | 由"搜索必须推进定位"一条覆盖 |

---

## 4. Search Discipline

> "A search should turn 'unknown' into a candidate, or a candidate into a confirmed location. If two or three searches return the same places or nothing new, stop rewording the query — read the strongest candidate, follow a symbol or reference, or question the assumption behind the query."

**符号工具首次被点名**：`find_symbol`（定义）、`find_references`（使用点）。C2.3 baseline 实测它们在三次真实仓运行中调用次数为 0。

---

## 5. Progressive Read

> "With a location signal … start with the surrounding region rather than the whole file, then expand when the code tells you to … When a file is small, or a whole state machine / parser / protocol / trait impl / config path is genuinely involved, read all of it. **Broader reads are correct when the structure genuinely requires them**; what wastes effort is a broad read taken *before* you know where to look."

**未硬性限制任何读取**（§20）：`MAX_BYTES` 未改，`read_file` 无行数上限，schema 未动。测试 `navigation_discipline_never_forbids_broad_reads_or_argues_from_tokens` 禁止 guidance 出现 "never read the whole file" / "save tokens" / "minimize reads" 等措辞。

---

## 6. Impact Surface Discipline

> "Before a non-trivial edit, follow the real dependencies out from what you found: callers and consumers, the interface or trait the code implements, other implementations of the same contract, constructors and serialization, the configuration-to-runtime path, and the tests or fixtures that cover it. Follow what this code actually connects to — do not walk a fixed checklist. Changing the first match you find and stopping is the most common way to ship a half-fix."

---

## 7. Exploration Convergence

> "Keep exploring while what you learn still moves the target location, your understanding of the behavior, the impact surface, the edit, or how you will verify it. Once those hold steady, **stop exploring and make the change** — re-confirming what you already know, or checking paths nothing points at, is not progress."

**这一条是本轮失败的核心（见 §13）。** §34 预警过"只有 Search First 会把盲读变成无限 grep"，因此收敛纪律与搜索纪律同批写入 —— 但实测它没有约束住行为。

---

## 8. Verification Feedback Loop

> "A failed verification is a location signal. Read the diagnostic for the file, line, and symbol it names, go to that code, correct your model of the impact surface, and edit again. Do not re-run the same failing command unchanged, and do not restart exploration from the top of the repository."

外加 §15 的对偶条款（补上 baseline audit 的 C4 缺口）：

> "After a successful `apply_patch` or `replace`, do not re-read it just to confirm — a failed edit reports failure."

---

## 9. Tool Description Changes

| 工具 | 新增语义 |
| --- | --- |
| `grep` | "The first thing to reach for when you do not yet know which file implements a behavior; each hit's line number is where to start reading." |
| `read_file` | "`start_line`/`end_line` give an inclusive range: with a location signal … read that region and widen when the surrounding structure turns out to matter. **Omit the range to read the whole file when the file is small or entirely relevant.**" |

Schema 逐字段验证未变（`read_file_schema_is_unchanged_by_the_navigation_guidance`）：`required` 仍只有 `path`。

---

## 10. Control vs Treatment

**CONTROL** = `7bb1647`（C2.3A + C2.3 指标instrumentation，无导航纪律），跑在 `git worktree` 隔离检出上，`base.md` 中 `Code navigation` 计数为 0。
**TREATMENT** = `13ff592`。除提示词与工具描述外，model / provider / budget / fixtures / verifier / runtime 完全一致。两臂**串行**执行，避免并发 rate limit 扭曲。

### 10.1 C1 Representative Set（11 case 配对）

| Case | rounds C→T | tokens C→T | reads | search | edits | 隐藏验收 |
| --- | --- | --- | --- | --- | --- | --- |
| BB1 | 5→5 | 70,973→75,791 | 3→3 | 1→1 | 1→1 | 两臂 PASS |
| BB2 | 10→5 | 149,888→76,255 | 5→3 | 1→1 | 4→1 | 两臂 PASS |
| EDIT1 | 12→10 | 178,704→156,103 | 3→3 | 3→2 | 2→2 | 两臂 PASS |
| EXP1 | 13→**17** | 226,741→**326,910** | 11→13 | 2→2 | 2→2 | 两臂 PASS |
| EXP2 | 13→**20** | 233,581→**395,481** | 16→18 | 1→**5** | 2→4 | 两臂 PASS |
| MF1 | 15→14 | 248,273→251,473 | 6→6 | 1→1 | 4→4 | 两臂 PASS |
| MF2 | 8→**11** | 141,322→**220,720** | 6→6 | 1→1 | 1→1 | 两臂 PASS |
| MF3 | 16→**21** | 280,044→**385,895** | 7→7 | 1→1 | 7→9 | 两臂 PASS |
| R1 | 7→8 | 105,703→133,813 | 3→4 | 1→1 | 1→1 | 两臂 PASS |
| R2 | 8→**11** | 119,799→**178,226** | 4→3 | 1→2 | 1→2 | 两臂 PASS |
| WV1 | 7→**11** | 99,369→**172,722** | 2→4 | 2→**4** | 1→2 | 两臂 PASS |
| **合计** | **114→133（+17%）** | **1,854,397→2,373,389（+28%）** | 66→70 | 15→21 | 26→29 | **11/11 → 11/11** |

轮次：**7 例上升 / 3 例下降 / 1 例持平。**

| 主指标 | Control | Treatment |
| --- | --- | --- |
| Hidden Acceptance | **11/11** | **11/11** |
| False Completion | **0** | **0** |
| Half-fix（MF1/MF2/MF3） | 0 | **0** |
| WV1 终态 | CompletedUnverified | **CompletedUnverified** |
| BB1 终态 | CompletedUnverified | **CompletedUnverified** |
| BB2 终态 | Completed | **Completed** |

**安全语义三条精确保持，正确性零退化 —— 但也零提升。**

关键观察：**reads 66→70、searches 15→21，几乎没动。** 这个 case set 的定位成本本就接近零（八个 case 里六个只搜 1 次），**它无法检验 locate-first 纪律**。多出来的 17% 轮次不是花在导航上，是花在更多编辑与更长的推理上。

### 10.2 yq（真实仓）

| 指标 | Control | Treatment |
| --- | ---: | ---: |
| 隐藏验收 | **PASS** | **PASS** |
| rounds | 46 | **71** |
| input tokens | 2,190,016 | **3,845,889** |
| reads / searches | 25 / 7 | 36 / 7 |
| edits | 3 | 10 |
| first edit | **r19** | **r29** |
| **relevant recall** | **3/3** | **3/3** |
| **impact recall** | **4/4** | **4/4** |
| 导航调用数 | 32 | 43 |
| **informative（有信息增量）** | **91%** | **81%** |
| **DUPLICATE（重复确认）** | **0** | **6（14%）** |
| LOW_VALUE | 3 (9%) | 2 (5%) |

Treatment 的重复确认集中在第一次编辑之前：

```
33 grep  DUPLICATE  {"pattern": "multiDocSample\\s*="}
34 grep  DUPLICATE  {"pattern": "multiDocSample"}          ← 同一问题换写法再问一次
38 read  DUPLICATE  cmd/evaluate_all_command.go
39 read  DUPLICATE  cmd/evaluate_sequence_command.go
40 read  DUPLICATE  cmd/evaluate_all_command.go            ← 一轮内两次回读同一文件
41 read  DUPLICATE  pkg/yqlib/stream_evaluator.go
```

**这正是 §10 明令要避免的形态，而收敛纪律恰恰是为它写的。**

> **方差说明（必须声明）**：yq 每臂只跑一次。跨版本观测到的 yq 轮次为 33（C2.3A）/ 46（本轮 Control）/ 50、55（C2.2，同一份代码两次）/ 71（本轮 Treatment）。**单次对比不足以支撑对轮次与 token 的强因果结论。** 相对稳健的是比率类指标（informative 91%→81%、DUPLICATE 0→6）与 11 个配对 case 的一致方向。

### 10.3 ripgrep

**未运行。** §27 明确 ripgrep 完成不是本阶段硬要求；在 yq 已显示 +54% 轮次的前提下，为一个已处于 `TurnLimitReached` 的 case 再烧约 9.4M token 无法改变判定。这是我主动缩小的范围，明确记录，可按需补跑。

---

## 11. C1 Correctness Regression

见 §10.1。**11/11 → 11/11，False Completion 0，三条安全语义精确保持。§28 的任一 FAIL 条件均未触发。**

---

## 12. Accuracy / Coverage Delta

| 指标 | Control | Treatment | Δ |
| --- | --- | --- | --- |
| Target Recall（yq） | 3/3 | 3/3 | **0** |
| Impact Surface Recall（yq） | 4/4 | 4/4 | **0** |
| MissedImpactPaths | none | none | **0** |
| Half-fix | 0 | 0 | **0** |
| Hidden Acceptance | 12/12 | 12/12 | **0** |
| False Completion | 0 | 0 | **0** |
| **导航信息增量率（yq）** | **91%** | **81%** | **−10pp** |
| **重复确认（yq）** | **0** | **6** | **+6** |

**没有任何主指标改善；唯一有方向的变化是导航质量下降。**

---

## 13. 为什么失败

三条，按证据强度排序：

1. **收敛纪律没有约束住行为。** 提示词写了"证据停止变化就停止探索"，实测 yq 的重复确认从 0 涨到 6，首次编辑从 r19 推迟到 r29。定性的停止条件对模型不构成可执行的判据 —— 它无法可靠判断"证据是否还在变化"。§34 预警的失败模式确实发生了，而且**预先写入收敛条款并不足以避免它**。

2. **影响面纪律在这个 case set 上只有成本没有收益。** MF1/MF2/MF3 的隐藏验收在 Control 下已经全过、yq 的 impact recall 在 Control 下已经 4/4 —— **没有可供改善的漏洞**。纪律让模型多做了检查，检查全部返回"已经覆盖"。

3. **case set 无法检验 locate-first。** searches 15→21、reads 66→70 基本没动，因为这些合成 fixture 的定位成本接近零。**能证明 locate-first 价值的任务（unknown-location、distractor、cross-package）正是 C2.3C 的 N1–N8** —— 它们还不存在。

---

## 14. Secondary Efficiency Delta

| | Control | Treatment | Δ |
| --- | ---: | ---: | ---: |
| C1 rep rounds | 114 | 133 | **+17%** |
| C1 rep input tokens | 1,854,397 | 2,373,389 | **+28%** |
| yq rounds | 46 | 71 | +54%（单次，方差大） |
| yq input tokens | 2,190,016 | 3,845,889 | +76%（同上） |

按 §29，token 本身不是 FAIL 判据。**FAIL 的依据是"成本一致上升而所有主指标持平、且导航质量下降"，不是"token 变多了"。**

---

## 15. Deterministic Tests（§30）

| 编号 | 要求 | 测试 | 结果 |
| --- | --- | --- | --- |
| A | guidance 被 production request 正确注入 | `navigation_discipline_is_baseline_for_every_provider` | ✅ |
| B | 不是 eval-only | 同上（走 `PromptBuilder::build()` 生产路径） | ✅ |
| C | 不同 provider 同一 baseline | 同上（`base.md`，非 `base_instructions`） | ✅ |
| D | C2.3A soft-plan 行为未变 | `plan_guidance_does_not_demote_navigation` | ✅ |
| E | `read_file` contract 未变 | `read_file_description_keeps_whole_file_reads_available` | ✅ |
| F | 未重新强制 ToolChoice | `plan_guidance_does_not_demote_navigation`（断言 "first substantive action" 不存在） | ✅ |
| G | tool schema 兼容 | `read_file_schema_is_unchanged_by_the_navigation_guidance` | ✅ |
| H | verifier completion 语义未动 | 全量套件 + §10.1 三条安全语义 | ✅ |
| — | 反向：不得禁止整读 / 不得用省 token 措辞 | `navigation_discipline_never_forbids_broad_reads_or_argues_from_tokens` | ✅ |
| — | KNOWN 位置不做仪式性搜索 | `navigation_discipline_allows_reading_directly_when_the_location_is_known` | ✅ |
| — | patch 后不回读 | `navigation_discipline_covers_rereading_after_a_successful_patch` | ✅ |

## 16. Workspace Gate（§31）

`cargo fmt --check` ✅ · `cargo check --workspace --all-targets` ✅ 0 error · `cargo test --workspace --no-fail-fast` ✅ **122 suites / 2,547 tests / 0 failed**。

---

## 17. Known Limitations

1. **yq / ripgrep 单次运行**，方差跨度 33–71 轮，轮次与 token 的因果结论不成立（§10.2 已声明）。
2. **ripgrep 未跑**（§10.3 已说明理由）。
3. **case set 与被测能力不匹配** —— 这是本轮最重要的方法论缺陷：用一组定位成本接近零的任务去检验定位纪律。
4. 导航价值分类（`scripts/classify_navigation.py`）是启发式的：`DUPLICATE` 依据区间覆盖与查询重复，`LOW_VALUE` 依据"搜索结果里没有任何文件后来被读取或编辑"。它对**排序与趋势**可靠，不适合当作绝对值。
5. 收敛纪律是定性的，未提供模型可自检的判据。

---

## 18. FINAL VERDICT

按 §36 逐条核验：

| 要求 | 结果 |
| --- | --- |
| False Completion = 0 | ✅ |
| C1 representative 无正确性退化 | ✅ 11/11 → 11/11 |
| yq hidden acceptance PASS | ✅ |
| impact coverage 不下降 | ✅ 4/4 → 4/4（持平，未上升） |
| multi-file half-fix 不增加 | ✅ 0 → 0 |
| unknown-location 更偏 locate-first | ⚠️ 无法判定（case set 定位成本近零，searches 15→21） |
| 已有位置时不机械多一次 Search | ✅ 无仪式性搜索证据 |
| broad read 未被硬禁止 | ✅ 测试锁定 |
| **证据足够后能更快停止低价值探索** | ❌ **相反**：DUPLICATE 0→6，informative 91%→81%，first edit r19→r29 |
| ripgrep navigation shape 无退化 | ⚠️ 未运行 |

# C2.3B: FAIL

判在最后一条硬要求。导航纪律实现完整、测试严密、正确性与安全语义零退化，但**它没有买到任何可测量的东西，并且让导航质量变差了**。

**不建议直接回滚**：`prompt.rs` 的 C1 措辞修正（把 plan 要求收窄到 mutation 之前）是对 C2.3A 的必要补全，与本轮判定无关，应当保留。真正需要重做的是 `base.md` 的十条纪律 —— 至少收敛那一条必须换成模型能自检的形式，而不是"证据是否还在变化"这种定性判断。

**方法论结论（比判定本身更重要）**：本轮用一组**定位成本接近零**的任务去检验**定位纪律**，结构上就不可能证明收益。C2.3C 的 N1–N8（unknown-location / distractor / cross-package / large-file）不是"下一个阶段"，而是**本轮结论成立的前置条件**。

**NEXT: C2.3C — Navigation Capability Eval N1–N8**（先建能检验导航能力的 case set，再重做 guidance）。本轮不开始。
