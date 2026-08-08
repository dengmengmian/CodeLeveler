# C1.4 — Exploration Scale Ladder

日期：2026-08-08。分支 `feat/coding-real-task-completion-c1`。模型 `deepseek/deepseek-v4-flash`。Run #1 为真值。生产代码零改动（仅 `evals/`、`scripts/`、eval-only instrumentation、`docs/`）。

**结论前置：假设被证伪。** 仓库文件数不是发散驱动力 —— 100 → 1500 文件（15×）在每一项指标上都是平的，且**移除架构地图后依然平**。唯一发散的 ripgrep 是另一种轨迹：**53/100 轮花在第一次编辑之前**，48 次读里 23 次是重读已读文件（**零 compaction** —— 重读的内容仍逐字躺在它自己的上下文里）。而且它**最终把代码写对了**（独立验收 PASS），只是没能在预算内收敛。

## 1. Ladder 设计

同一仓库家族 `telemetryd`，同一缺陷，同一依赖深度，四种规模：

```
cmd/telemetryd → internal/pipeline → internal/sink（接口） → internal/sink/rollup（实现） → internal/window（缺陷）
```

缺陷：`window.Start()` 用四舍五入除法，把边界前一秒的事件推进下一个窗口，违反文档写明的半开区间约定。症状用户可见（告警晚一个窗口），修复是单点的。规模只增加可信的周边包（类型 + 逻辑 + helper + 测试；每三个包带一个**正确的**自有 bucketing/merge helper，让关键词搜索返回大量诚实命中）。生成器 `scripts/gen_scale_repos.py` 确定性输出，不入库（同 ripgrep 的 fetch 脚本惯例）。

预算固定：`max_rounds: 100`、同模型、同工具策略、同验证方式，四档一致。

反作弊：hidden acceptance 用**未在任何提示里出现过的**事件序列断言四行精确输出，并要求 `git status` 的改动**只能落在 windowing 包内**（碰任一 distractor 即红，本地已实证），且必须附带测试改动。

## 2. 结果表

| Case | Files | PASS | Termination | Rounds | Tools | Reads | Searches | UniqueReads | RepeatedReads | UniqueQueries | FirstRelevant@ | FirstEdit@ | InputTokens | ExplorationRatio |
| --- | ---: | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| S100 | 101 | ✅ | completed | 21 | 28 | 11 | 2 | 9 | 2 | 2 | **2** | 9 | 364,139 | 46% |
| S300 | 300 | ✅ | completed | 17 | 23 | 9 | 1 | 8 | 1 | 1 | **2** | 5 | 326,289 | 43% |
| S800 | 803 | ✅ | completed | 17 | 24 | 9 | 2 | 9 | 0 | 2 | **2** | 8 | 422,206 | 46% |
| S1500 | 1501 | ✅ | completed | 19 | 26 | 11 | 1 | 8 | 3 | 1 | **2** | 8 | 611,013 | 46% |
| S300-nomap | 304 | ✅ | completed | 16 | 21 | 9 | 1 | 7 | 2 | 1 | **2** | 7 | 300,861 | 48% |
| S1500-nomap | 1500 | ✅ | completed | 17 | 25 | 10 | 1 | 7 | 3 | 1 | **2** | 6 | 550,344 | 44% |
| **ripgrep** | ~2500 | ❌ | **budget_limited** | **100** | 123 | **48** | **30** | 25 | **23** | 30 | n/a¹ | **53** | **7,787,456** | **63%** |

¹ ripgrep case 未声明 `relevant_paths`（该字段是本轮新增的 metrics-only 字段），所以它的 `first_relevant_file_round` 是"未声明"而非"从未找到"——不作为证据使用。

## 3. Scale Curve

| 指标 | S100 → S1500（15× 文件） |
| --- | --- |
| Rounds | 21 → 19（**无增长**） |
| Tool calls | 28 → 26（无增长） |
| Unique files read | 9 → 8（**无增长**） |
| Searches | 2 → 1（无增长） |
| First relevant file | 2 → 2（**恒定**） |
| Input tokens | 364k → 611k（1.7×，**亚线性**，且主要来自更大的目录列表结果） |

**去地图对照**：删掉 README 的布局段与 `docs/pipeline.md`（单一变量），S300-nomap / S1500-nomap 的每项指标与带地图版本**无差别**，first relevant file 仍在第 2 轮。所以第 2 轮命中不是被架构文档"喂"的 —— 是 Go 的包布局 + 概念词（"window boundary" → `internal/window`）本身就自带映射。

## 4. Breakpoint

**在 ≤1500 文件的合成阶梯上不存在拐点。** 曲线是平的，不是线性、亚线性或悬崖 —— 是**无关**。因此"规模拐点"这一提法本身被数据否定：驱动发散的不是文件数。

## 5. Trajectory Comparison

| | Ladder（S100-S1500，含 nomap） | ripgrep |
| --- | --- | --- |
| 首次编辑前的调用 | 3-9 次 | **74 次**（44 read + 25 grep + 2 list + 2 plan + 1 git_status，**96% 纯探索、零命令、零编辑**） |
| 首次编辑轮 | r5-r9 | **r53**（预算过半） |
| 搜索 | 1-2 次，全新查询 | **25 次 grep，30 个全不重复的查询**（不是循环，是**漂移**） |
| 重复读 | 0-3（0-27%） | **23/48 = 48%** |
| Compaction | 0 | **0** ← 重读的内容仍在上下文里，不是被压缩挤掉的 |
| 首次编辑后 | 收敛 | 49 次调用：26 shell + 14 patch + 7 探索；其中 9 次 cargo build/test、3 次 sccache/toolchain 排障 |
| 结局 | Verified | 100 轮硬顶（executor `MAX_TURN_ROUNDS`）→ budget_limited |
| **独立验收** | PASS | **PASS**（`--total-count` 真的实现对了） |

## 6. 必答问题

**A. Scale breakpoint 在哪？** 合成阶梯上没有。文件数从 101 到 1501 每项指标持平；去掉架构地图也不变。

**B. 复杂度增长形态？** 对文件数**无关**（rounds 21→19，unique reads 9→8）；token 亚线性（1.7×/15×）。ripgrep 与阶梯之间不是同一曲线上的两点，而是**两种 regime**。

**C. 增长主要来自哪里？** ripgrep 的成本集中在**第一次编辑之前的读与搜**：74 次调用中 71 次是探索（44 读 + 25 grep），占掉 53/100 轮。不是 verification（9 次构建）、不是 editing（14 次 patch）、也不是 context duplication（**0 次 compaction**）。7.79M input tokens / 100 轮 = 78k/轮，是阶梯（32k/轮）的 2.4 倍——上下文单调增长且每轮重发。

**D. 找不到目标，还是找到了不敢改？** **两者都不是。** 它找到了、改了（14 次 patch）、而且**改对了**（hidden acceptance PASS）。失败在于定位吃掉半程预算，剩下的实现—验证循环没能在硬顶前收敛并给出完成信号。

**E. ripgrep 与 ladder 是同一种轨迹吗？** **不是。** 阶梯从不发散（1-2 搜、7-9 独立读、r2 命中相关文件）；ripgrep 是 25 次不重复 grep + 48% 重读 + r53 首编。两者共享的只有"最终代码正确"这一点。

## 7. Root Failure Cluster

**Pre-edit localization 消耗**（唯一有量化证据的发散源，n=1）：

1. **53/100 轮在第一次编辑之前**，其中 96% 的调用是读与搜。
2. **48% 的读是重读已读文件，且全程零 compaction** —— 它在重新推导仍然逐字存在于自己上下文中的信息。这是最直接、最可测的浪费。
3. **30 个互不重复的搜索查询**（重复查询 = 0）：不是 loop guard 能抓的死循环，是查询漂移——一直在换问法，说明没有收敛到"目标在哪"的判断。
4. 相比之下，实现阶段是健康的（14 patch / 1 失败，9 次构建），只是没预算了。

次要噪声（记录不修）：ripgrep 尾段有 3 次 sccache/RUSTC_WRAPPER 排障调用，属真实仓工具链摩擦。

## 8. C1.5 RECOMMENDED TOP-1

**C1.5 RECOMMENDED TOP-1: Exploration Convergence — 有界定位与已读工作集**

Evidence（全部来自唯一发散轨迹的实测，n=1，需在实施前用 1-2 个真实仓 case 复核）：

| 依据 | 数据 |
| --- | --- |
| impact | 定位阶段独占 **53/100 轮**；若该阶段收敛到 ~20 轮，实现阶段（实测需 47 轮，含构建摩擦）本可在预算内完成 —— 而代码**本来就写对了**，唯一缺的是预算 |
| 可测浪费 | **23/48 次读是重读**，且 **compaction = 0**：被重读的内容仍在上下文里。这不是上下文丢失，是没有"我已经读过什么"的工作集意识 |
| 收敛信号缺失 | **30 个全不重复的查询**，loop guard 按设计不会触发（args 每次都不同）——现有护栏对"查询漂移"完全无感 |
| 反证据（重要） | 规模本身**不**是原因：15× 文件数、去掉架构地图，指标全平。所以不要做 repo map / 索引，那不是本次数据支持的方向 |

**明确不推荐**（数据不支持）：Repository Context Compression（0 次 compaction，上下文没被压缩挤掉）；Repo Map / 索引（去地图对照证明地图不影响定位）；Plan Commitment（阶梯全部按时收敛，ripgrep 也确实产出了正确实现）。

只推荐，不实现。

## 9. Regression

`cargo fmt --check` ✅ · `cargo check --workspace --all-targets` ✅ 0 error · `cargo test --workspace --no-fail-fast` ✅ 0 failed。

本轮 instrumentation 为纯增量字段（既有指标未改动语义），未重跑 C1.2/C1.3 的真实模型集。

## 10. 复现方式

```bash
python3 scripts/gen_scale_repos.py                 # S100/S300/S800/S1500
python3 scripts/gen_scale_repos.py --no-map 300 1500
leveler eval run --cases evals/scale --model deepseek/deepseek-v4-flash
```
