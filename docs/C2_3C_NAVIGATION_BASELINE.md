# C2.3C NAVIGATION CAPABILITY EVAL — Baseline

> ## ⚠ INVALIDATED DUE TO EVAL DATA LEAKAGE
>
> 本文档记录的 baseline（**6/8**、Target Recall 8/8、Impact 19/19 FullyRead）
> **全部作废**。后续发现 Eval Agent 可以通过 shell + 权限升级读取隐藏的 case
> 定义与隐藏验收 —— 详见 `docs/C2_3C_S_EVAL_SANDBOX_INTEGRITY.md`。
>
> 这些数字只能作为 **HISTORICAL / CONTAMINATED** 证据保留，
> **禁止与干净数据合并计算**。
>
> 另外，本文档 §9 声称"8 个 case 的隐藏验收在未修改 fixture 上 8/8 失败"
> **是用错误方法得到的**（检查脚本破坏了 heredoc）。逐字复验后：
> **7/8 有效，N6 是无效 fixture** —— 它描述的缺陷在原始 fixture 中并不存在。
> 因此本文档关于 N6 的 `TARGET_ABANDONED` 结论同样不成立。
>
> 干净 baseline 待 C2.3C-F（N6 fixture 修正）后重新建立。


日期：2026-08-09。分支 `feat/coding-context-efficiency-c2`。模型 `deepseek/deepseek-v4-flash`，默认产品行为，无 ablation。

**BASELINE CAPABILITY: 6/8。** 更重要的是这句：**八个 case 全部达成 relevant 与 impact 满召回 —— 包括两个失败的。两次失败都发生在定位之后。**

> **指标语义已于最终收口修正（§3A）。** 原文把 path-level 覆盖称作 "Impact Surface Recall"，那个名字比证据强。经按实际返回区间复核：**19 条声明路径全部被完整、未截断地读回**（`FULL 19 / PARTIAL 0 / MISS 0`），因此本 baseline 的数字**未被高估**，且可以用比 path-touch 更强的 `PathFullyRead` 表述。仍不能声明 range 级 `EvidenceCovered` —— 隐藏元数据只有路径，没有行号 ground truth。

---

## 1. Baseline Product State

| 组成 | 状态 |
| --- | --- |
| C2.3A | 完整保留：无 forced `update_plan`、无 `tools=[update_plan]`、导航工具不因缺 plan 被拒、one-shot soft nudge 保留、multi-step mutation 仍需 plan |
| C2.3B `base.md` 十条导航纪律 | **已移除**（`cb75387`）—— A/B 实测轮次 +17%、token +28%、yq 信息增量 91%→81%、重复确认 0→6 |
| C2.3B `grep` / `read_file` 描述中的行为引导 | **已移除**，只保留能力说明（"omitting both returns the whole file"） |
| C2.3B `prompt.rs` 把 plan 要求收窄到 mutation 之前 | **保留** —— 它不是导航建议，是 C2.3A 的必要补全 |

`base.md` 中 `Code navigation` 段落计数为 **0**，本 baseline 即在此状态上测得。

---

## 2. Eval Architecture

复用现有体系，未建平行 runner：`crates/leveler-eval`（case 模型 + 指标）· `evals/navigation/`（8 个 case）· `leveler eval run --cases evals/navigation`。

共享底座 `navsvc` 由 `scripts/gen_nav_fixtures.py` 可复现生成（无随机）：29 文件 / 27 个 `.go` / 1,879 LOC，最大文件 `internal/ingest/decoder.go` **1,024 行**。设计细节见 `docs/C2_3C_NAVIGATION_EVAL_DESIGN.md`。

---

## 3. Anti-Leakage —— 本轮抓到并修复了一个真实漏洞

### 3.1 静态防泄漏（设计时）

模型可见面收敛到 `EvaluationCase::model_visible_input()`（只有 `task`）。三条测试锁住哨兵路径不进任务文本、不落地成文件、旧 case 仍原样解析。

### 3.2 动态防泄漏（baseline 跑出来的）

**N6 首轮实测**：Agent 日志原话 —— *"Found it — there's an eval definition file. Let me read it."*

它的路径：

```
git remote -v                         → 拿到宿主仓路径（clone 的 origin）
git -C <宿主路径> show HEAD:decoder.go → 读原始 fixture，diff 出注入的改动
ls  <宿主路径>/evals/                  → 走向 case 文件
find <宿主路径> -iname '*eval*'
read_file <宿主路径>/evals/navigation/n6-large-file-region.yaml   ← 被工作区守卫拒绝
```

**`read_file` 被拒了，`shell_command` 没有。** 守卫不能被信赖 —— 面包屑本身必须消失。

**修复**：eval 克隆后立即 `git remote remove origin`，并由确定性测试
`a_cloned_eval_workspace_keeps_no_pointer_to_its_source` 锁住（断言 `git remote -v` 为空且 `.git/config` 不含源路径）。**这同时使 yq / ripgrep 等所有真实仓 case 受益。**

**该轮 N6 的 60 轮耗尽因此不是干净的能力测量** —— 大量轮次花在走宿主路径上。N3 与 N6 已在堵漏后的 harness 上**重跑**，本报告的 scorecard 使用重跑结果。

---

## 3A. Coverage Semantics Audit（最终收口）

### 当前 pipeline 在哪一层判定覆盖

```
ToolCall(arguments)                  ← 登记 pending（按参数里出现的路径）
   ↓
ToolResult{ is_error, preview }      ← 成功才落账 —— 这一层已正确
   ↓
relevant_paths_touched / impact_paths_touched
```

`AgentEvent::ToolResult` 只携带 **1200 字符的 preview**（`dispatch::preview`，`MAX = 1200`）。因此 live collector **在结构上无法**判断返回区间：一次被字节上限截断的整文件读取，与一次完整返回，在它眼里完全一样。

**这就是原报告 §9 里 §42.6 只能"部分满足"的真正原因** —— 不是遗漏，是 live 层的信息不足。

### 三个必须分开的概念

| 概念 | 定义 | 本轮是否实现 |
| --- | --- | --- |
| **PathTouched** | 一次命名该路径的调用**成功返回** | ✅ live collector（失败读取不计，测试锁定） |
| **PathFullyRead** | 实际返回从第 1 行到文件末行，且**未截断** | ✅ analyzer（`scripts/analyze_read_coverage.py` + `leveler-eval/src/read_coverage.rs` 规范定义 + 8 条单元测试） |
| **EvidenceCovered** | 返回区间**包含**隐藏的 relevant 行区间 | ❌ **未实现** —— 隐藏元数据只有路径，无行号 ground truth |

按 §5 的两个合法选项，本轮落在 **OPTION A 与 OPTION B 之间**：能证明的比 path-touch 强（完整读取），但达不到 range 级 evidence coverage。**因此报告中不使用不加限定的 "Impact Surface Recall"。**

### 为什么没有改生产 schema

§9 明确要求：若需 production ToolOutput schema 改动则先停。本轮**不需要** —— `read_file` 已为每行编号并在提前停止时追加 `… [truncated` 标记，`context_snapshot` 事件又保存了模型实际收到的完整文本，所以返回区间可以**事后从 durable event 精确还原**。C2.2 曾为此加过 `read_lifecycle` metadata，后按无消费方撤回；现在证明也确实不需要它。

### 复核结果：本 baseline 未被截断污染

`scripts/analyze_read_coverage.py` 对 8 个 case 的全部 19 条声明路径逐条按实际返回区间判定：

| 判定 | 数量 |
| --- | ---: |
| **FULL**（第 1 行到末行，未截断） | **19** |
| PARTIAL | **0** |
| MISS | **0** |

**关键个案 —— N6**（§11 要求的 sanity check）：`internal/ingest/decoder.go` 共 **1,024 行 / 30,221 字节**，而 `MAX_BYTES = 262,144`。**该文件在物理上不可能被截断。** 实测返回区间 `1-1024`，`clipped=False`。

所以 N6 属于 §11 的情形 **A（确实读到了目标 relevant region）**，不是 B。**原 scorecard 的 1/1 不需要下修。**

## 4. Capability Scorecard

| Case | 结果 | Target Path<br>FullyRead | Impact Path<br>FullyRead | Distractor Err | HalfFix | FirstRelevant | FirstEdit | Nav Calls | FailureClass |
| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | --- |
| **N1** Unknown Location | ✅ PASS | **1/1** | 1/1 | 0 | — | r2 | call 23 | 18 | — |
| **N2** Competing Impls | ✅ PASS | **1/1** | 1/1 | 0 | — | r2 | call 9 | 7 | — |
| **N3** Caller Propagation | ❌ FAIL | **1/1** | **2/2** | 0 | **YES** | r2 | call 16 | 45 | 见 §5.1 |
| **N4** Interface Contract | ✅ PASS | **1/1** | **5/5** | 0 | — | r2 | call 22 | 22 | — |
| **N5** Config→Runtime | ✅ PASS | **2/2** | **4/4** | 0 | — | r2 | call 17 | 15 | — |
| **N6** Large-file Region | ❌ FAIL | **1/1** | 1/1 | 0 | — | r3 | call 17 | 33 | 见 §5.2 |
| **N7** Cross-package | ✅ PASS | **2/2** | **4/4** | 0 | — | r2 | call 31 | 28 | — |
| **N8** Distractor Resistance | ✅ PASS | **1/1** | 1/1 | 0 | — | r2 | call 9 | 8 | — |

| 汇总 | 值 |
| --- | --- |
| **BASELINE CAPABILITY** | **6/8** |
| **Target Path Touch / FullyRead Recall** | **8/8**（全部完整读取，见 §3A） |
| **Required Impact Path Touch / FullyRead Recall** | **8/8**（19/19 条路径全部完整读取；含 N4 的 5/5、N5 与 N7 的 4/4） |
| Impact **Evidence** Recall（range 级） | **未定义** —— 无行号 ground truth（§3A） |
| MissedImpactPaths | **0** |
| **ForbiddenPathsEdited** | **0** —— 没有一次运行改动过 `legacy/` 或 `examples/` |
| DistractorPathsTouched | 0 计入（distractor 被读取属正常导航，本批未触发） |
| FalseCompletion（Agent 侧） | **1**（N3，见 §5.1） |

---

## 5. Failure Taxonomy

### 5.1 N3 —— 找对了，做对了，然后自己撤销了

轨迹（逐 patch 核实）：

1. 定位到 `internal/report/summary.go`（r2），继而找到 `internal/report/aggregate.go` —— **两条 required impact path 全中**。
2. r6 提交了**完全正确**的补丁：`Observe` 与 `Distinct` 各加一个 `if !record.Valid { continue }`。
3. 随后**把两处都撤销**，把过滤搬到 `internal/ingest/reader.go`。
4. 引擎自身验证通过（`repair=1(pass)`，终态 `verified`），隐藏验收失败 —— 隐藏测试直接调用 `Aggregate` / `Distinct`，报告层仍在计数 invalid 记录。

导航指标：45 次导航调用、信息增量仅 **60%**、**18 次 DUPLICATE**（全 suite 最高）。

**这不是定位失败，也不是影响面遗漏。** 它是"已经拿到正确解、却换成了一个更窄的错误解"。§29 的九个分类**没有一类覆盖这种情况** —— 最近的 `NAV_READ_SCOPE`（定位正确但读取不足）并不成立，它读得足够多（45 次导航）。这是本轮暴露的分类学缺口，记录在此。

**这正是 §19 要求的 half-fix 探测器起作用的样子**：编译通过 + 引擎验证通过 + 行为契约未满足，只有独立隐藏验收抓得到。

### 5.2 N6 —— 读到了目标文件，从未修改它

60 轮耗尽。30 reads / 3 searches / 8 次编辑，但编辑对象是：

```
cmd/depthcheck/main.go          ← 自建的验证程序
internal/ingest/scratch_test.go
internal/ingest/scratch2_test.go
internal/ingest/verify_depth_test.go
```

**`internal/ingest/decoder.go` 一次都没被编辑。** 它读到了（Target Recall 1/1），然后把 60 轮预算花在搭验证脚手架上。

分类：**`NAV_POST_LOCALIZATION_COMMITMENT` / `TARGET_ABANDONED`**。影响面已稳定（1/1）且信息增量高达 94% —— 它并没有在做低价值探索，它只是**从不把证据转成对目标文件的修改**。

### 5.3 归因汇总

| 类别 | 次数 |
| --- | ---: |
| `NAV_LOCATE_MISS` | **0** |
| `NAV_DISTRACTOR` | **0** |
| `NAV_IMPACT_MISS` | **0** |
| `NAV_PREMATURE_EDIT` | 0 |
| `NAV_OVER_EXPLORE` | 0（N6 改归下方，理由见 §5.4） |
| `NAV_READ_SCOPE` | 0 |
| `NAV_RELEARN` | 0（N3 有 18 次 DUPLICATE，但那是撤销-重做的副产物，不是首要原因） |
| `NAV_VERIFICATION_DRIVEN_DISCOVERY` | 0 |
| `NAV_TOOL_FAILURE` / `NAV_ENVIRONMENT` / `NAV_PROVIDER` | 0 |
| **`NAV_POST_LOCALIZATION_COMMITMENT`**（新增，见下） | **2**（N3 · N6） |

### 5.4 新增分类：`NAV_POST_LOCALIZATION_COMMITMENT`

§29 的九类都描述"没找到 / 找错 / 没找全"。本轮两次失败都不是这些 —— 定位与影响面覆盖全部达标，失败发生在**之后**。正式定义：

> The agent has already localized the correct target and acquired sufficient
> impact evidence, but fails to commit that evidence into a stable, correct
> final edit.

诊断性子类型（只用于归因，不扩张主分类）：

| Subtype | 表现 | 本轮 |
| --- | --- | --- |
| `CORRECT_PATCH_REVERTED` | 已产出正确补丁，随后撤销并改为另一实现 | **N3** |
| `TARGET_ABANDONED` | 已读到目标文件，始终未修改它；预算被脚手架/探针工作耗尽 | **N6** |

注意 §5.2 中 N6 原先归的 `NAV_OVER_EXPLORE` 并不准确：它的信息增量高达 **94%**，几乎每次导航都带来新信息 —— 问题不是"低价值探索太多"，而是**从不收敛到对目标文件的修改**。归入 `TARGET_ABANDONED` 更贴合证据。

---

## 6. Information Gain

| Case | 导航调用 | 有信息增量 | DUPLICATE | LOW_VALUE |
| --- | ---: | ---: | ---: | ---: |
| N1 | 18 | 83% | 3 | 0 |
| N2 | 7 | 86% | 1 | 0 |
| **N3** | **45** | **60%** | **18** | 0 |
| N4 | 22 | 77% | 3 | 2 |
| N5 | 15 | 93% | 0 | 1 |
| N6 | 33 | 94% | 2 | 0 |
| N7 | 28 | 86% | 3 | 1 |
| N8 | 8 | 75% | 1 | 1 |

分类基于可观察事实（新路径 / 新符号 / 搜索结果重叠 / 读取区间覆盖），未调用 LLM 判定（§22）。

**两个失败 case 的信息增量指向相反方向**：N3 是 60%（大量重复确认），N6 是 94%（几乎每次导航都有新信息，但从不收敛到编辑）。**低信息增量既不是失败的必要条件，也不是充分条件** —— 这是本轮对 C2.3B「靠信息增量判断该不该停」这一假设的又一次证伪。

---

## 7. Verification-driven Discovery

`verification_driven_impact_discovery` 在八个 case 中**全部为 false**：没有一条 required impact path 是在检查失败之后才首次触达的。影响面全部由导航本身找到，不是编译器找到的。

---

## 8. C1 vs N1–N8：分辨率对比（§35）

| | C1 代表集（Control 臂，11 case） | **N1–N8（8 case）** | 比值 |
| --- | ---: | ---: | ---: |
| 导航调用合计 | 81 | **176** | — |
| **每 case 平均导航调用** | **7.4** | **22.0** | **3.0×** |
| 搜索调用合计 | 15 | 31 | — |
| **每 case 平均搜索** | **1.4** | **3.9** | **2.9×** |
| 只搜索 1 次的 case | **6 / 8 可比 case** | **0 / 8** | — |
| 每 case 平均轮次 | 10.4 | 20.8 | 2.0× |
| required impact path ≥ 4 的 case | 0 | **3**（N4 5 条、N5 与 N7 各 4 条） | — |
| 通过率 | 11/11 = 100% | **6/8 = 75%** | — |

**C1 代表集的通过率是 100%，N1–N8 是 75%。** 前者对导航能力没有分辨率（八个可比 case 中六个只搜一次即中），后者能把运行分成"过"与"没过"，且**两次失败的成因各不相同**。

§43 的自检：baseline 不是 8/8，且失败点落在两个不同能力上 —— **不需要重做 case**。

---

## 9. Deterministic Validation（§37）

| 编号 | 要求 | 测试 | 结果 |
| --- | --- | --- | --- |
| A/B/C | hidden metadata 不进 model prompt | `no_hidden_path_reaches_the_model_visible_input` | ✅ |
| — | hidden path 不凭空造出文件 | `hidden_paths_do_not_materialize_into_the_workspace` | ✅ |
| D | 隐藏验收独立运行 | 8 个 case 各自 `expect`，在未修改 fixture 上 **8/8 失败** | ✅ |
| E | failed read 不计 coverage | `a_failed_read_does_not_count_as_coverage` | ✅ |
| G | path coverage 正确 | `relevant_and_impact_coverage_are_tracked_per_path` | ✅ |
| H | impact recall 正确 | `coverage_before_the_first_edit_is_frozen_at_that_edit` | ✅ |
| I | distractor 读取 ≠ forbidden 编辑 | `reading_a_distractor_is_not_the_same_as_editing_a_forbidden_path` · `editing_a_forbidden_path_is_recorded_as_an_error` | ✅ |
| J | verification-driven discovery 可识别 | `impact_found_only_after_a_failed_check_is_attributed_to_verification` + 其对偶 | ✅ |
| L | C1 既有 eval 语义不变 | `navigation_metadata_is_optional_for_existing_cases` + 全量套件 | ✅ |
| §31 | 工作区不留回源路径 | `a_cloned_eval_workspace_keeps_no_pointer_to_its_source` | ✅ |

**F（clipped read 不误算整文件）：部分满足。** `broad_reads` / `narrow_reads` 依据模型**请求**的区间分类，而覆盖率依据成功返回。真正的"返回区间"级覆盖需要工具侧 metadata（C2.2 做过又按 §17 撤回，因当时无消费方）。当前实现不会把失败读取算成证据（E 已锁），但一次被字节上限截断的整文件读取仍被记为"触达该路径"。记为已知限制（§11）。

---

## 10. Workspace Gate（§38）

`cargo fmt --check` ✅ · `cargo check --workspace --all-targets` ✅ **0 error** · `cargo test --workspace --no-fail-fast` ✅ **122 suites / 2,561 tests / 0 failed**（较 C2.3B 基线的 2,547 增加 14 条：`read_coverage` 8 条、path-touch 语义 1 条、anti-gaming 与本地 git 导航 1 条、hidden-metadata leakage 3 条、coverage 快照 1 条）。

## 11. C1 Regression（§39）

Eval 语义向后兼容：四个新 metadata 列表全部 `#[serde(default)]`，`navigation_metadata_is_optional_for_existing_cases` 断言旧 case YAML 原样解析。C2.3B 已在同一 product state 的前一版跑过 C1 代表集 11/11；本轮未改动生产导航行为，仅改动 eval 基础设施与 clone 步骤（后者只会移除信息，不会增加）。

---

## 12. Known Limitations

1. **每个 case 单次运行。** §27 建议对 N2/N6/N8 补重复运行以观察方差，本轮未做（N3/N6 各有一次重跑，但那是为了消除 harness 漏洞的影响，不是方差测量）。
2. **N6 首轮受 harness 漏洞污染**，已重跑；但重跑仍是单次。
3. **语言单一（Go）。** 按 §17 的取舍：宁可一个设计良好的 case，不为凑语言写劣质 case。
4. **range 级 `EvidenceCovered` 未实现** —— 隐藏元数据只有路径。`PathFullyRead` 已实现并证明本轮 19/19 全部完整读取，所以当前 baseline 的结论不受影响；但若将来某个 case 的目标文件大到会被字节上限截断，就必须补行号 ground truth 才能继续做 evidence 级判定。
5. **信息增量分类是启发式的**，对趋势可靠，不适合当绝对值。
6. **§29 分类学有缺口**：N3 的失败形态（已得正确解后自行撤销）不属于任何既有类别。

---

## 13. FINAL VERDICT

按 §42 逐条核验：

| # | 要求 | 结果 |
| --- | --- | --- |
| 1 | N1–N8 全部实现 | ✅ 8 个 |
| 2 | 全部有独立 hidden acceptance | ✅ 且在未修改 fixture 上 8/8 失败 |
| 3 | hidden metadata 无 leakage | ✅ 静态三测试 + 动态漏洞已修并锁定 |
| 4 | 覆盖指标基于 actual successful tool result | ✅ 本轮修掉了按参数记账的缺陷 |
| 5 | failed read 不算 evidence | ✅ 测试锁定 |
| 6 | clipped read 不误算 full coverage | ✅ **已满足** —— `read_coverage::ReturnedRange::is_complete` 对截断结果恒返回 false（8 条单元测试），analyzer 复核本 baseline 19/19 全部 FULL（§3A） |
| 7 | 多个 case 具真正 multi-step navigation cost | ✅ 平均 22 次导航调用，最高 45 |
| 8 | 覆盖 disambiguation / impact propagation / large-file / cross-module / distractor | ✅ N2 · N3+N4+N5 · N6 · N7 · N8 |
| 9 | 明显比 C1 代表集更具分辨率 | ✅ **导航调用 3.0×、搜索 2.9×，通过率 100% → 75%** |
| 10 | baseline 已用真实模型完成 | ✅ |
| 11 | failure taxonomy 能解释 baseline failure | ✅ 两次失败均有逐 patch 证据；并诚实记录了分类学缺口 |
| 12 | 未修改生产 Navigation 行为救 benchmark | ✅ 唯一的生产侧改动是**移除** C2.3B 的负收益 guidance，发生在 baseline 之前 |

十二条全部满足。唯一未实现的是 range 级 `EvidenceCovered`，它不在 §42 的条件里，且已在 §12 明确记录为限制 —— 本轮不为凑指标名称伪造精度。

# C2.3C: PASS

**BASELINE CAPABILITY: 6/8**

**TOP FAILURE CLASS: `NAV_POST_LOCALIZATION_COMMITMENT`** —— 两次失败**都不是**定位失败。Target Recall 8/8、Impact Recall 8/8、MissedImpactPaths 0、ForbiddenPathsEdited 0。N3 拿到了正确解又撤销成一个更窄的错误解；N6 读到了目标文件却从未修改它，把 60 轮花在自建验证脚手架上。**共同点是：证据已经足够，却没能转化成一次正确且完整的修改。**

**NEXT: C2.3D — Evidence-to-Edit Commitment**（把"证据已足够"变成模型可自检、Harness 可观测的判据；C2.3B 已证明定性的收敛措辞不起作用，而本轮证明可攻击面在定位之后，不在定位本身）。

本轮不开始 NEXT。
