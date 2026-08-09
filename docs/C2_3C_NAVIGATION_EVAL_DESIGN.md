# C2.3C — Navigation Capability Eval: Design

日期：2026-08-09。分支 `feat/coding-context-efficiency-c2`。

**这是一份尺子的设计文档，不是能力改进文档。** 目标是让导航失败**可见且可分类**，不是让 Agent 表现得更好。

---

## 1. 为什么需要新的 case set

C2.3B 用 C1 代表集做 Control/Treatment，结论是"导航纪律没有可验证收益"。但那个结论有一个结构性缺陷：

| C1 代表集实测 | 值 |
| --- | --- |
| 八个可比 case 中只搜索 1 次的 | **6 个** |
| Control → Treatment 的 searches 变化 | 15 → 21 |
| Control → Treatment 的 reads 变化 | 66 → 70 |

**定位成本接近零的任务无法检验定位纪律。** C1 fixture 每个 6 个文件，答案基本在唯一可能的地方。它们适合验证 correctness / verification / false completion / half-fix —— 但对导航能力没有分辨率。

---

## 2. 共享底座：`navsvc`

`scripts/gen_nav_fixtures.py` 生成一个可复现（无随机）的 Go 服务：

| | |
| --- | --- |
| 文件 | 29（27 个 `.go`） |
| Go 代码行 | 1,879 |
| 最大文件 | `internal/ingest/decoder.go`，**1,024 行** |
| 包 | `config` · `ingest` · `pipeline` · `sink` · `report` · `testutil` · `pkg/api` · `legacy` · `examples` |
| 测试 | `pipeline` · `report` · `config` 三个包自带测试 |

四个刻意的结构特征：

| 特征 | 实现 | 导致的导航难度 |
| --- | --- | --- |
| **调度间接** | stage 与 sink 通过 `map[string]…` 注册表解析 | 配置里的 `"normalize"` / `"stdout"` 是 map key，不是 Go 符号 —— 任务里的词无法直达实现 |
| **活/死并存** | `legacy/`（pre-1.0）与 `examples/`（`//go:build ignore`） | 死实现的命名**比活实现更贴近用户措辞** |
| **真实大文件** | `decoder.go` 目标位于 ~510 行，前后共 60 个 `parseValueN` / `normalizeLabelN` | 读前 200 行找不到答案；`rg` 会先命中形似符号 |
| **可断链** | `config → load → validate → runtime → stage → behavior` | 少一环仍然编译、现有测试仍然全绿 |

`legacy/` 在 README 与包注释中明写"kept for reference while downstream tooling migrates, not wired into the running service" —— 符合 §33「不要为了难而难」：一个认真读代码的开发者能判断它是死的，但一次 `rg` 判断不了。

---

## 3. 八个 case

每个 case = `navsvc` + 一个 overlay（注入缺陷或移除特性）+ 独立隐藏验收。

| # | Case | 导航能力 | 难点（§32） |
| --- | --- | --- | --- |
| **N1** | `n1-unknown-location` | Target Localization | 任务只描述症状（大小写拆分计数），不含文件/符号/包名。用症状词汇 grep 先命中 `legacy/oldpipeline.go` 的 `NormalizeName` —— 一行 `ToLower`，看起来就是答案，但是死代码 |
| **N2** | `n2-competing-implementations` | Implementation Disambiguation | 两套 summarizer。`rg "Summarize"` **只**命中死的那个；活路径要顺 `main.go → report.Aggregate → Summary.Render` 走 |
| **N3** | `n3-caller-propagation` | Impact Surface Discovery | 改 `Summary.Observe` 后 `report.Distinct` 也要改，但两者**无调用关系**。漏掉它编译通过、现有测试全绿 —— 只有隐藏验收抓得到 |
| **N4** | `n4-interface-contract` | Dependency Following | `Sink` 接口 + 3 个注册实现 + 1 个结构性满足接口的测试替身。注册表是 `map[string]func() Sink`，实现藏在构造器后 |
| **N5** | `n5-config-to-runtime` | Config→Runtime Flow | 新选项要走 4 跳、跨 2 个包。断任一环仍编译 |
| **N6** | `n6-large-file-region` | Large-file Localization | 1,024 行文件，目标 ~510 行。`rg "depth"` 会先命中 `pkg/api/types.go`、flatten stage、config 字段 |
| **N7** | `n7-cross-package` | Cross-module Navigation | 跨 config / sink / cmd 三包。**sink 注册表签名是 `func() Sink`，无处接收参数** —— 必须找到接缝，不是填空 |
| **N8** | `n8-distractor-resistance` | Distractor Resistance | 任务刻意用死包词汇；`SummarizeRecords` 全仓唯一命中且是错的；第三个 lookalike 在 `examples/` 被 build tag 排除 |

---

## 4. Hidden Metadata

四个 metrics-only 列表，全部 `#[serde(default)]`（向后兼容 §39）：

| 字段 | 含义 | 用途 |
| --- | --- | --- |
| `relevant_paths` | 缺陷/特性真正所在 | Target Recall |
| `required_impact_paths` | 完整修改必须覆盖到的路径 | Required Impact **Path** Touch / FullyRead Recall · MissedImpactPaths（**不是** range 级 evidence recall，见 §6） |
| `distractor_paths` | 看起来像答案但不是 | **只统计，不判错**（§26） |
| `forbidden_edit_paths` | 正确的修改绝不能改动 | 编辑它 = Localization Error |

**读 distractor 与改 forbidden 严格分开**：考察候选并排除它正是好的定位行为；只有**编辑**才是错误。

---

## 5. Anti-Leakage（§37 A–C）

模型可见面收敛到**一个函数**：

```rust
impl EvaluationCase {
    pub fn model_visible_input(&self) -> &str { &self.task }
    pub fn hidden_metadata_paths(&self) -> Vec<&str> { /* 四个列表 */ }
}
```

三条确定性测试：

| 测试 | 断言 |
| --- | --- |
| `no_hidden_path_reaches_the_model_visible_input` | 哨兵路径（`ZZRELEVANTZZ/` 等）不出现在任务文本中 |
| `hidden_paths_do_not_materialize_into_the_workspace` | 声明一个 hidden path 不会凭空造出该文件 |
| `navigation_metadata_is_optional_for_existing_cases` | 旧 case YAML 原样解析，四个列表均为空 |

链路上唯一进入模型的是 `case.task`（`eval_cmd.rs:662` / `:1058`）。distractor 与 forbidden 路径的**文件本身**在 fixture 里真实存在（它们必须存在才有干扰力），但**哪些是 distractor** 这一判断只在 harness 内。

---

## 6. Evidence 的硬定义（§24）

三个层次，**不得合并成一个 bool**：

| 概念 | 判据 | 实现位置 |
| --- | --- | --- |
| `PathTouched` | 命名该路径的调用**成功返回** | live collector（`eval_signals.rs`） |
| `PathFullyRead` | **实际返回**从第 1 行到末行且未截断 | `leveler-eval/src/read_coverage.rs`（规范定义 + 单元测试）· `scripts/analyze_read_coverage.py`（轨迹复算） |
| `EvidenceCovered` | 返回区间**包含**隐藏的 relevant 行区间 | **未实现** —— 隐藏元数据只声明路径，不声明行号 |

**请求 ≠ 返回。** `read_file(path)` 无区间时是*请求*整文件；字节上限仍可能只返回前缀，此时工具会追加 `… [truncated` 标记。按参数判定覆盖会把没收到的证据记成已收到，而且这种漏判**看不见** —— 路径明明就在调用里。

`AgentEvent::ToolResult` 只带 1200 字符 preview，所以 live 层结构上做不到区间判定；`context_snapshot` 保存了模型实际收到的完整文本，因此区间在**事后**可精确还原。这也是为什么本轮**没有**改动生产 `ToolOutput` schema。

### 成功返回是硬门槛

**覆盖率只由成功返回的工具结果计入。**

C2.3 的原实现在 `ToolCall` 时就按参数记账 —— 一次失败的 `read_file` 会被算成"已看过该文件"，即把失明记成洞察。C2.3C 改为：调用时登记 pending，`ToolResult` 且 `!is_error` 时才落账。

由此得到的三个新指标：

| 指标 | 定义 |
| --- | --- |
| `relevant_paths_before_edit` / `impact_paths_before_edit` | 首次编辑那一刻冻结的覆盖快照 —— 区分"动手前就知道"与"事后才发现" |
| `distractor_paths_read` / `forbidden_paths_edited` | 前者诊断，后者错误 |
| `verification_driven_impact_discovery` | 某条 impact path 是在一次检查**失败之后**才首次触达 —— 这类 run 隐藏验收可以 PASS，但它是编译器找到的影响面，不是导航找到的 |

---

## 7. Difficulty Rationale：每个 case 为什么不是一次 grep

| Case | 一次 literal grep 会得到什么 |
| --- | --- |
| N1 | `rg -i "lower\|case\|normalize"` → `legacy/oldpipeline.go:NormalizeName`（死）+ `internal/pipeline/stage.go` 的 map key |
| N2 | `rg "Summarize"` → **仅** `legacy/oldsummary.go`（死） |
| N3 | `rg "Valid"` → `pkg/api/types.go` · `internal/pipeline/filter.go`（干扰）· `summary.go`。`Distinct` 不含 `Valid` 一词 |
| N4 | `rg "Sink"` → 接口 + 注册表 + 3 实现 + 替身 + main，**不指出哪些必须改** |
| N5 | 任务里的 `min_value` 在仓库中**零命中**（尚不存在） |
| N6 | `rg "depth"` → `types.go` · `flatten` stage · `config.go` · 新增的 `depth_report.go`（干扰），`decoder.go` 里没有 `depth` 一词 |
| N7 | `max_records` 零命中；`rg "Sink"` 同 N4 |
| N8 | `rg "summarize records"` → `legacy/oldsummary.go` 注释，唯一命中，错误答案 |

**N5 / N7 的关键词在仓库中根本不存在** —— 这两个 case 从设计上就不可能被 literal grep 解决。

---

## 8. Human Sanity Check（§34）

八个 case 均可由一个会用 `rg` / 符号跳转 / 引用查找的开发者在合理时间内完成：

- 活路径始终可从 `cmd/navsvc/main.go` 顺调用链抵达；
- `legacy/` 的死代码身份写在 README 与包注释里；
- `examples/` 由 `//go:build ignore` 明确排除；
- 没有反射、没有代码生成、没有隐藏的元编程 —— 全部是普通的注册表模式（Go 生态里极常见）。

**答案不依赖任何不可见知识。**

---

## 9. Baseline 有效性自检（§34 / §42.6）

八个 case 的隐藏验收在**未修改的 fixture** 上逐一执行：

| Case | 基线退出码 |
| --- | --- |
| N1 | 1（失败 ✓） |
| N2 | 1（失败 ✓） |
| N3 | 2（失败 ✓） |
| N4 | 2（失败 ✓） |
| N5 | 2（失败 ✓） |
| N6 | 2（失败 ✓） |
| N7 | 2（失败 ✓） |
| N8 | 1（失败 ✓） |

**8/8 全部失败 —— 无 vacuous case。** 每个隐藏验收都在真的测东西，而不是恒真。

---

## 10. 明确未做的事

- 未新建 benchmark runner（复用 `crates/leveler-eval` + `evals/` + `leveler eval`）。
- 未为 N1–N8 写 case-specific prompt。
- 未修改 verifier 让 case 更容易通过。
- 未根据 baseline 结果改动任何生产导航行为。
- 语言只有 Go（§17：宁可一个设计良好的 case，不要为凑语言写劣质 case）。
