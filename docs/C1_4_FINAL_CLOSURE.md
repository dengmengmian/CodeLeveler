# C1.4 Final Closure — Real-Repo Localization Validation

日期：2026-08-08。分支 `feat/coding-real-task-completion-c1`。模型 `deepseek/deepseek-v4-flash`。Run #1 为真值。生产代码零改动（`crates/leveler-eval`、CLI eval instrumentation、`evals/`、`scripts/`、`docs/`）。

**结论前置**：补测两个真实仓后，C1.4 的"定位耗时"结论被修正。真实仓的共同签名不是"找不到"，而是 **first relevant 极早、first edit 极晚** —— ripgrep 第 **3** 轮就读到正确文件，第 **64** 轮才第一次编辑；yq 第 **2** 轮读到，第 **31** 轮才编辑。对照阶梯（同样 r2 命中）间隔只有 3-7 轮。这是决策矩阵的 **CASE B**。

## 1. Instrumentation Correction

审计结论与预期不同，如实记录：`first_relevant_file_round` 的写入**本来就有 `!touched_relevant_files` 守卫**（eval_signals.rs:144），只记录第一次，不会被后续访问覆盖。**没有需要修的行为**。

补三条确定性测试把语义钉住（防止将来有人去掉守卫）：

| 测试 | 断言 |
| --- | --- |
| `first_relevant_round_records_the_first_touch_only` | r2 命中后再在 r3-r8 命中六次 → 仍为 **2** |
| `first_relevant_round_skips_irrelevant_exploration` | 前 4 轮读无关文件、r5 命中 → **5** |
| `first_relevant_round_is_absent_when_never_touched` | 从未命中 → **None**（绝不编造轮次） |

因此 **C1.4 阶梯的 first-relevant 数字全部有效，无需修正**。

## 2. ripgrep：first relevant vs first edit

`relevant_paths` 补齐为 `crates/core/flags/defs.rs`、`crates/core/flags/hiargs.rs`、`crates/core/main.rs`（metrics-only：不进 prompt、不给 Agent、不影响工具行为）。重跑一次：

| 指标 | 值 |
| --- | --- |
| **first relevant @** | **r3**（第 4 个工具调用就 `read_file crates/core/flags/defs.rs`） |
| **first edit @** | **r64** |
| **间隔** | **61 轮** |
| rounds / tools | 100（硬顶）/ 130 |
| termination | budget_limited |

**所以此前 `n/a` 确实是"没声明"而非"没找到"。** 结论翻转：不是 CASE A（很晚才找到），是 CASE B（很早找到、很晚才动手）。

## 3. 新增真实仓 case

`evals/realrepo/yq-doc-count.yaml`（Go，**477 文件**，pinned `v4.44.3`，经 `scripts/fetch_eval_repos.sh yq` 获取，不入库）。任务与 ripgrep 同型：加一个全局 flag `--doc-count`，需要从 CLI 表面接到求值路径。提示不含文件名/符号名；无 overlay（`git log` 无泄漏）；hidden acceptance 独立（help 可见 + 三文档流计数 + 文件参数 + 不带 flag 行为不变 + 既有测试通过）。已本地两态验证：基线红、参考实现绿。

helm（1363 文件）已抓取备用，本轮未跑（时间预算）。

## 4. 结果表（8 次运行）

| Case | Files | PASS | Term | Rounds | Tools | FirstRel@ | FirstEdit@ | **Gap** | Reads | Uniq | Rpt | Searches | Uniq | InputTok | Tok/Round |
| --- | ---: | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| S100 | 101 | ✅ | completed | 21 | 28 | 2 | 9 | **7** | 11 | 9 | 2 | 2 | 2 | 364k | 17k |
| S300 | 300 | ✅ | completed | 17 | 23 | 2 | 5 | **3** | 9 | 8 | 1 | 1 | 1 | 326k | 19k |
| S800 | 803 | ✅ | completed | 17 | 24 | 2 | 8 | **6** | 9 | 9 | 0 | 2 | 2 | 422k | 25k |
| S1500 | 1501 | ✅ | completed | 19 | 26 | 2 | 8 | **6** | 11 | 8 | 3 | 1 | 1 | 611k | 32k |
| S300-nomap | 304 | ✅ | completed | 16 | 21 | 2 | 7 | **5** | 9 | 7 | 2 | 1 | 1 | 301k | 19k |
| S1500-nomap | 1500 | ✅ | completed | 17 | 25 | 2 | 6 | **4** | 10 | 7 | 3 | 1 | 1 | 550k | 32k |
| **yq** | 477 | ✅ | completed | **72** | 87 | **2** | **31** | **29** | 26 | 23 | 3 | 10 | 10 | **3.15M** | **44k** |
| **ripgrep** | ~7300 | ❌ | budget_limited | **100** | 130 | **3** | **64** | **61** | 51 | 27 | 24 | 38 | 36 | **7.36M** | **74k** |

## 5. Pre-edit vs Post-edit

| | yq | ripgrep |
| --- | --- | --- |
| pre_edit_tool_calls | 45 | **92** |
| pre_edit_reads / distinct | 25 / 22 | 49 / 27 |
| **pre_edit_repeated_unchanged_reads** | **3** | **22** |
| pre_edit_searches / distinct | 9 / 9 | 37 / 35 |
| post_edit_tool_calls | 42（27 shell + 10 patch） | 38（21 shell + 11 patch） |
| 主导成本 | **post-edit 实现/验证循环** | **pre-edit 探索** |

**两个真实仓的主导子成本不同**，但 gap（29 / 61 轮）都远高于阶梯（3-7 轮）。这是唯一跨仓一致的信号。

## 6. Search Query Trajectory（§5 要求的判定）

**ripgrep = non-convergent reformulation。** 38 条 grep，逐条看：

- 1-9 **在正确区域**（`stats` / `struct HiArgs` / `matched_lines` / `fn search_worker` / `LowArgs`）—— 而且第 4 个调用已经读了 `flags/defs.rs`。
- 10-11 `enum Category` **逐字重复两次**。
- 17-18 被 ripgrep 自带的"FLAGS 必须有序"测试带偏（`fn test_|is_sorted`、`is_sorted_by_key`）。
- 19-31 **13 条查询钻进 `fn lines` / `lines_with_terminator` / `LineTerminator`** —— 与验收标准（help 里出现 flag、末行打印 `TOTAL:<N>`）无关的死胡同。
- 33-38 回到 `FLAGS|Category|help generation` —— 在已经把 `defs.rs` 读了 **9 次** 之后。

判定：**不是渐进收窄，是反复重构问法并回访已解决区域**。

**yq = progressive narrowing。** 9 条查询干净下钻：`func readStream` → `readStream\(` → `func FormatFromString` → `NewYamlDecoder\(` → `DecoderFactory|NewGoccy|NewYamlDecoder` → `DecoderFactory: func` → `DecoderFactory`。零死胡同、零回访。

**所以 query drift 是 ripgrep 专有，不是真实仓通病。**

## 7. Read Repetition（§6 要求的区分）

按要求只统计**高价值信号 C**（pre-edit 阶段、文件尚未被改动过的重读 —— 编辑前没有任何写入，所以这类重读定义上就是纯重复）：

- **ripgrep：22 次**（`flags/defs.rs` 读 9 次、`hiargs.rs` 5 次、`printer/standard.rs` 5 次）。
- **yq：3 次**。

C1.4 原文把"48% 重读"当作普遍驱动因子；**修正**：重读是 ripgrep 专有的浪费，yq 几乎没有。它不是跨仓共同签名。

## 8. Context 成本解释（§7 要求的措辞更正）

**已排除**：compaction 导致的遗忘（两个真实仓 compaction 均为 0）。

**未排除，且有正向信号**：context accumulation / 每轮 prompt 增长 / working-set 管理 / 相关证据留存 / 重复上下文搬运。tokens-per-round 随任务规模单调上升：阶梯 17-32k → yq **44k** → ripgrep **74k**。ripgrep 的 7.36M 输入不是被压缩挤掉的历史，而是**单调累积并每轮重发**的上下文。

## 9. Final Decision Matrix

| | 判定 |
| --- | --- |
| CASE A（找得晚 + 改得晚 + 大量搜索） | ❌ 排除：两个真实仓 first relevant 都在 r2-r3 |
| **CASE B（找得早 + 改得晚 + 重读/漂移）** | ✅ **命中**：gap 29（yq）/ 61（ripgrep）轮，阶梯对照仅 3-7 轮 |
| CASE C（定位与编辑都正常，token/round 后期爆炸） | 部分符合（tok/round 44k-74k），但 gap 更早、更大，是 B 的伴生现象 |
| CASE D（只有 ripgrep 发散） | ❌ 排除：yq 也偏离（72 轮 / 3.15M tokens，是阶梯的 4×/6×），只是没撞硬顶 |

## 10. C1.5 RECOMMENDED TOP-1

**C1.5 RECOMMENDED TOP-1: Localization Commitment — 找到目标后尽早承诺第一次编辑**

narrow definition：当轨迹已经读到足以动手的目标代码后，把"继续勘察"的预算收紧，让第一次编辑更早发生，用编辑后的真实反馈（构建/测试）替代继续勘察。**不是** repo map、**不是** 搜索策略重写、**不是** compaction 调参。

**Confidence: MEDIUM**

Evidence：

| 依据 | 数据 |
| --- | --- |
| 跨仓一致 | 两个不同语言的真实仓、两种不同的主导子成本，gap 都远超对照：yq r2→r31（29 轮）、ripgrep r3→r64（61 轮）；6 个阶梯对照 r2→r5-9（3-7 轮） |
| 勘察没有换来正确性 | 两个仓在长勘察之后**仍然**各自迭代了 10 / 11 次 patch —— 勘察并未减少编辑迭代，只是推迟了它 |
| 代价可归因 | ripgrep 的 61 轮 gap 直接吃掉预算（硬顶 100 轮），post-edit 只剩 36 轮；yq 的 29 轮 gap 让总轮数达到阶梯的 4 倍 |
| 反证据已排除 | 规模（15× 文件全平）、架构地图（去掉无差别）、compaction 遗忘（均为 0）、重读（yq 仅 3 次）、query drift（yq 干净下钻）—— 都不是跨仓共同因 |

**为何只给 MEDIUM**：n=2 真实仓，其中一个 PASS 一个 FAIL；两者的次级成本来源不同（yq 在 post-edit 构建循环，ripgrep 在 pre-edit 勘察）；"更早编辑一定更好"尚未被反向实验证明（需要一次 ablation：人为约束首编轮次后再跑同样两个 case）。建议 C1.5 实施前先做该 ablation。

不实现。

## 11. Regression

`cargo fmt --check` ✅ · `cargo check --workspace --all-targets` ✅ 0 error · `cargo test --workspace --no-fail-fast` ✅ 0 failed（含 3 条新 instrumentation 测试）。

## 12. 复现方式

```bash
scripts/fetch_eval_repos.sh yq
leveler eval run --cases evals/realrepo  --model deepseek/deepseek-v4-flash
leveler eval run --cases evals/scenarios/feature --model deepseek/deepseek-v4-flash
```
