# C5-S1 Migration

日期：2026-08-10。基线 `bf326aa`。**结构迁移，行为变更 = NONE。**

## Before Architecture

```
ModelLimits { context_window, reliable_context, max_output_tokens, ... }
        │
        └─ policy_resolver: context_budget = limits.reliable_context   ← 事实与策略在此混合
                └─ factory → executor（执行环折叠阈值）

chat 路径（engine.rs ×5 / interactive.rs ×1）:
        leveler_agent::PRE_REQUEST_COMPACT_THRESHOLD (24k)             ← 第二个口径，独立常量
```

审计结论（先于改动）：`ModelCapabilities`/`ModelLimits` 本身**已经**是"只描述事实"的正确
形态——问题不在能力层结构，而在（a）策略值直接从事实字段取出、无策略概念；
（b）chat 与执行环两个阈值走两条路，口径漂移无人察觉（C2.1 已记录）。

## After Architecture

```
ModelLimits（不变，语义钉死）           ContextQuality（新，可选，必须带 measured_at）
        │                                        │
        └────────── ModelProfile ────────────────┘
                        │
        ExecutionPolicyResolver（同一个缝，未新建 resolver）
                        │
              ContextPolicy { initial_budget, max_budget, compaction, retention }
                ├─ for_profile(p)  = reliable_context（执行环，值不变）
                └─ chat_default()  = 24_000（chat 路径，值不变，同缝解析）
                        │
        ResolvedExecutionPolicy { context, context_budget（兼容镜像）, ... }
```

单变体枚举命名现状：`CompactionPolicy::AnchoredBriefing`、`RetentionPolicy::BriefingOnly`
—— S3/S4 在同一缝后加变体，而不是加新调用点。

## Field Migration

| 字段 | 判定 | 处置 |
| --- | --- | --- |
| `context_window` / `max_output_tokens` / capability flags | 模型事实 | 原地不动 |
| `reliable_context` | **质量声明**（非运行限制） | 原地保留；doc comment 钉死语义："Policy may knowingly exceed it; nothing may treat it as a hard limit"。未改名 —— 改名会破坏全部模型配置文件，收益不抵（migration note 记录于字段注释） |
| `context_budget`（resolver 输出） | 运行策略 | 保留为 `context.initial_budget` 的**兼容镜像**，迁移注释指引新代码读 `context` |
| `PRE_REQUEST_COMPACT_THRESHOLD` | 运行策略值 | 常量仍在 compaction.rs（值的家）；6 个调用点全部改经 `ContextPolicy::chat_default()` —— 双口径合一到同一解析缝 |
| `context_quality`（新） | 测量声明 | `Option<ContextQuality { degradation_onset, measured_at }>`，serde default，未测=None，禁止拍脑袋填 |

## Compatibility

- 模型 YAML 配置：零改动可加载（新字段 optional）。
- provider / resolver / eval：`context_budget` 兼容镜像保证既有消费者（factory→executor、
  eval seam）零改动。
- 5 处 `ModelProfile` 字面量构造点补 `context_quality: None`（测试与 doctor fixture）。

## Tests（新增 3 条 + 既有全量）

| 测试 | 钉什么 |
| --- | --- |
| `context_policy_migration_is_behavior_identical` | `context.initial_budget == limits.reliable_context == context_budget`（镜像不漂移）；S1 无扩张（max==initial）；单变体枚举 |
| `chat_policy_pins_the_historical_pre_request_threshold` | chat 路径经新缝后仍 = 24_000 = 旧常量 —— 谁改值谁必须显式过这条 |
| `model_limits_carry_no_runtime_policy` | 能力层纯净：策略永远由 resolver 派生，`context_quality` 未测保持 None |

## Non Goals（S1 明确不做，属后续阶段）

`estimate_tokens` 校准（S2）、DeepSeek 1M 实测（S2）、自适应扩张（S3）、
指针化保留（S4）、任何阈值数值变化、任何索引/图谱。

## Rollback Plan

单 commit 纯增量：删除 `ContextPolicy`/`ContextQuality` + 还原 6 个 chat 调用点 +
resolver 一行，即回到 `bf326aa` 语义。兼容镜像保证期间任何中间状态行为一致。
