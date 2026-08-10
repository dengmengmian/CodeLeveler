# C5 Closeout — Context Intelligence

日期：2026-08-11。**C5 CLOSED。** 生产代码本阶段零改动（纯文档 + 回归确认）。

## Final State

```
C5 — Context Intelligence

SPEC    ✅
S1      ✅  Context Policy Architecture（策略/事实分离，行为零变更）
S2      ✅  Measurement Calibration（estimator 校准 + 945k 双模型零退化实测）
S3      ✅  Experimental mechanism implemented
            Production: DISABLED（长期状态）
            Value: NOT DEMONSTRATED
E2      ✅  Reality check completed
            Result: context pressure hypothesis not demonstrated
S4      ⏸  DEFERRED — prerequisite pressure not demonstrated

C5      ✅  CLOSED
```

措辞的精确边界：S3 **不是**"被证明有效"，也**不是**"失败" —— 工程完成、契约钉住、
价值未证，故不晋升。S4 **不是**"做失败了" —— 没有证据表明现在值得做，故主动 defer。

## C5 真正回答了什么

最初的问题是"长任务上下文不够怎么办"。得到的答案比做成 S3/S4 更有价值：

1. **模型窗口不是当前瓶颈**（S2：pro/flash 单针召回至 945k 无退化）。
2. **测量确实曾经不准，且已修**（S2：tool 载荷密度 2.5–2.9 bytes/token，
   max 低估 -38.3% → -3.9%）——这是 C5 已落地的**永久收益**。
3. **机制已就位**：S3 的 ContextBudgetState / 决策纯函数 / durable 事件 / ablation 缝
   全部在 seam 后待命，未来需要时不从零开始。
4. **真实 agent 会主动规避上下文压力**（E2：1500 文件 / 96 relevant adapters / 1.1MB
   真实差异化源码，两轮针对性 reshape 仍被 grep+范围读击穿，有效转录 ~12–16 万 token）：

```
大仓库 ≠ 大上下文需求
好的 Navigation + grep + range read + 结构化工具 = 主动把上下文需求压下来
```

这同时是对 C2 Navigation 与 Agent Loop 的一次反向确认。

## Established Facts（含 cache economics，全部引用既有测量，无新调用）

| 事实 | 来源 |
| --- | --- |
| estimator 全域无系统性低估（残差在安全侧） | S2，冻结 fixture 回归钉住 |
| 稳定前缀有真实 cache 价值（历史命中 96.8%；独立语料命中 0） | S2-D / 历史测量 |
| Expansion 保前缀、Compaction 重写前缀 → expand-before-compact 是正确的序 | S3 设计 + 实现 |
| 折叠压力在"事实可检索"任务类未被观察到 | S3 A/B + E2 两轮（0 folds / 30 个真实 run） |
| chat 24k、生产折叠阈值 = reliable_context，均未变 | S1/S3 回归 |

## Reopening Conditions（未来由真实数据触发，不靠猜）

后续 Dogfooding（真实 Rust/Go/Next.js 项目）与日常使用中关注这些**已有**观测量：
`context_snapshot` 尺寸（单请求转录）、`compacted` 事件、`RepeatedReadGuard` trips、
会话时长与 resume。当真实任务自然出现：

```
transcript > 256k → 自然 Compact → Compact 后大量重复读取
```

即重新打开 **C5-R1 — Adaptive Context Revalidation**，并且**把那个真实任务脱敏后
做成新的 E2** —— 比人工设计"抗检索任务"有价值得多。在那之前 S3 保持
EXPERIMENTAL / NOT PROMOTED，S4 保持 DEFERRED。

## Regression（本次收口确认）

- 生产默认 adaptive context 关闭：`adaptive_override_starts_small_and_default_stays_static`
  等测试钉住（default = `ExpansionPolicy::Disabled`，initial = reliable_context）。
- chat 24k / 兼容镜像 / estimator 校准 fixture：全部由既有测试持续保护。
- 本次运行的回归面见收口 commit。

## NEXT

```
TUI 优化 → Real Project Dogfooding (D1…) → 收集真实 context pressure
```
