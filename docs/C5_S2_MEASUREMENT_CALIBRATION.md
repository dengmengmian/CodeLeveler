# C5-S2 Measurement Calibration

## Baseline

HEAD `33b6b40` · 校准模型 `deepseek-chat`（S2-A）· 长上下文模型 `deepseek-v4-pro`（S2-B）·
测量日期 2026-08-10 · ground truth = provider-reported `prompt_tokens`（streaming/非 streaming
均经 `openai_chat` 适配器完整捕获，含 `prompt_cache_hit_tokens`）。

**结论前置：S2-A 修复了按内容类别分化的系统性低估（tool 载荷 -38% → 全域低估 ≤4%）；
S2-B 在 128k→945k 双 seed 全程 5/5 召回，退化 = NOT OBSERVED，`context_quality` 依规保持
None。`reliable_context` 未改。**

## Token Estimator Contract

`estimate_tokens(messages)` 只估 message 内容（Text / ToolCall name+args / ToolResult），
**不含** system 注入、tool schema、协议 framing —— 这些属于 request overhead，由 provider
`prompt_tokens` 整体计入。framing 实测 ≈4 tokens（近空探针）；tool schema 不在本函数契约内，
校准因此以 framing-corrected 内容 token 为 ground truth，未把 unrelated overhead 吸进字节权重。

## Estimator Before（17 样本，6 类 × ≤3 档，语料 = 真实仓库内容非合成重复）

| 类别 | 偏差范围 | 判定 |
| --- | --- | --- |
| rust-source / ts-web | -3.9% ~ +5.3% | **本就无偏** |
| mixed-docs | -1.4% ~ +2.9% | 无偏 |
| cjk-heavy | +2.4% ~ +6.7% | 既有防护正确（略高，安全侧） |
| **json-config** | **-15.6% ~ -27.6%** | 系统性低估 |
| **tool-output** | **-35.5% ~ -38.3%** | 系统性低估（最重） |

## Root Cause

历史"对源码低估 16–22%"（C2.1 §3）是**转录混合效应**：源码/prose 在 ÷4 下无偏；
低估全部来自 JSON/日志形状的内容 —— 括号、引号、重复键、hex id 使其在 DeepSeek tokenizer
下密度实测 **2.5–2.9 bytes/token**（json-config 2.92 / tool-output 2.47），而非假设的 4。
Agent 转录恰被 tool result（读文件回显、shell 输出、JSON）主导，故整体表现为低估。

## Estimator Change（最小、可解释、无 magic 常数）

estimator 本就按 `ContentPart` 分支 —— 修复只改权重归属：`ToolCall` 参数与 `ToolResult`
内容的 ASCII 字节记 `×2/5`（=2.5 bytes/token，取实测密度带下沿，使残差落在**高估=安全**侧）；
`Text` ASCII 维持 ÷4；宽字符 ÷3 与图片权重不变。无每模型系数、无全局乘数、不污染能力层。

## Estimator After（同一语料、同一 actual，零新增调用）

| 指标 | before | after |
| --- | --- | --- |
| 最大低估 | **-38.3%** | **-3.9%**（rust-source/large，非 tool 类） |
| tool-output | -35.5 / -38.3% | **+3.1 / -1.7%** |
| json-config | -27.6 / -27.0 / -15.6% | +15.8 / +16.8 / +35.1%（高估=安全侧，接受并记录） |
| 无偏类别（rust/ts/docs/cjk） | 不变 | 不变（零回归） |

Acceptance：低估 P90 门（≤10%）以 17 样本的 **max observed under = -3.9%** 满足
（样本数不足以合法声称 P90，按规改报 max/median：median = +2.4%）。纯 JSON 大样本
+35.1% 高估是 2.5 权重对密度 3.38 样本的代价 —— 方向安全、影响为压缩略提前，接受。

冻结回归：3 个实测样本逐字节入 `tests/fixtures/calib-*.txt`，
`calibration_fixtures_stay_within_measured_tolerance` 断言 [-10%, +40%] 带（unit test
不触网）；`tool_payloads_are_weighted_denser_than_prose` 钉权重结构。

## Long Context Method

`c5-s2-long-context-recall-v1`：确定性 code-like 语料（Rust 函数/config/结构化日志/prose
四类块，seed 可复现），5 个 16-hex 唯一 needle 植于 2%/25%/50%/75%/97% 位置，
exact-match 自动评分，`thinking: disabled`，`max_tokens=300`，ToolChoice 无。
隔离了 agent/工具/导航/验证全部变量。

> 流程伪影一则（已入 raw data）：首个探针未禁 thinking —— DeepSeek 服务端默认开启，
> 300 输出 token 全被 reasoning 吃掉、content 为空、0/5 —— 记 INVALID，不计入曲线。

## Probe Points（有效 10 探针，raw：`docs/measurements/c5-s2/long-context-recall.json`）

| target | seeds | actual input | recall | 位置明细 |
| ---: | :---: | ---: | :---: | :--- |
| 128k | 1,2 | 127,567 / 127,491 | 5/5 · 5/5 | 全位置 ✓ |
| 256k | 1,2 | 254,870 / 254,858 | 5/5 · 5/5 | 全位置 ✓ |
| 512k | 1,2 | 509,536 / 509,478 | 5/5 · 5/5 | 全位置 ✓ |
| 768k | 1,2 | 764,167 / 764,179 | 5/5 · 5/5 | 全位置 ✓ |
| **950k** | 1,2 | **945,119 / 945,200** | **5/5 · 5/5** | 全位置 ✓（窗口的 90%） |

**deepseek-v4-flash 补测**（同仪器、同 seed 语料，压顶部两档 + 中位对照）：

| target | seeds | actual input | recall |
| ---: | :---: | ---: | :---: |
| 512k | 1,2 | 509,536 / 509,478 | 5/5 · 5/5 |
| **950k** | 1,2 | 945,119 / 945,200 | **5/5 · 5/5** |

flash 结论同 pro：**NOT OBSERVED**，`context_quality` 同样保持 None。
两个工作模型在单针召回维度均干净到窗口 90%。

cached_input_tokens 全程 0（每探针独立语料，无前缀复用）——无缓存混杂。

## Recall Curve

平坦：overall 与全部 5 个位置桶在每档均为 100%，无位置性退化迹象。

## Degradation Result

```
NOT OBSERVED

No reproducible degradation observed through 945,200
provider-reported input tokens (≈90% of the 1,048,576 window),
2 independent seeds per point, 5 positional needles per probe.
```

无边界可加密（没有 first_bad），依预算纪律停止：**15/16 调用、8,270,174/10M input tokens**
（pro 11 + flash 4）。

## ContextQuality Decision

```
populated: NO — kept None
```

未观察到退化 ≠ 退化恰好在 window 处。依 S2 规则不写 `degradation_onset=1048576` 这类
构造值。事实记录于本文；S3 依 fallback（`reliable_context`）解析策略，且现在知道该值
**在单针精确召回维度上明显保守** —— 是否上调是 S3 的策略决定。

## reliable_context

**未改**（786,432 原样）。`ContextPolicy::for_profile()` 仍从它解析——改它即改生产折叠
行为，属 S3。

## Cost / Usage

| 段 | 调用 | input tokens | output tokens |
| --- | ---: | ---: | ---: |
| S2-A 校准（framing 探针 + 17 样本） | 19 | ≈332k | 19 |
| S2-B 长上下文 pro（含 1 无效试点） | 11 | 5,360,841 | ≈3.3k |
| S2-B 长上下文 flash 补测 | 4 | 2,909,333 | ≈1.2k |

## S3 Inputs（已测事实，供 S3 消费）

1. estimator 全域无系统性低估（max under -3.9%）——S3 的阈值可以信任计量单位。
2. tool 载荷密度 2.5–2.9 bytes/token；纯 JSON 会被高估至 +35%（安全侧）。
3. **deepseek-v4-pro 与 v4-flash 单针精确召回到 945k 均无退化**——786,432 的 fallback 在该维度保守；
   S3 的 max_budget 档位设计可据此考虑上探（策略决定，另测更难任务形态后再定亦可）。
4. 长上下文延迟：~62-116s @512-768k，~30s @950k（波动大，provider 侧因素）；
   逐轮全前缀重复计费的成本曲线数据在 raw JSON。
5. 独立语料 cache 命中为 0 —— cache 收益完全依赖前缀稳定性，支撑 S3"晚折叠、保前缀"设计。

## Risks / Limitations

- 单针精确召回是**最弱的退化探测器**——多跳/跨段推理型退化可能更早出现；
  "945k 无退化"只在该维度成立，不外推到复杂任务质量。
- 校准以 deepseek tokenizer 为 ground truth；其它 provider 密度未测（权重是通用启发，
  非 DeepSeek 特例硬编码）。
- 17 样本不支撑正式分位数声明；报的是 max/median。
- json-config/large 的 +35% 高估会让纯 JSON 重转录略早折叠——方向安全，成本可测。
