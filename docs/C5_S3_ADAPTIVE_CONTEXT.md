# C5-S3 Adaptive Context

> **Final C5 status（2026-08-11）：S3 = EXPERIMENTAL / NOT PROMOTED，生产默认 DISABLED。**
> 下方的 S3 阶段评估在当时以 `MECHANISM NOT EXERCISED / REMAINS OPEN` 收束 ——
> 那是一个**历史性的阶段 verdict**，随后被 E2 reality check 之后的 C5-S5 closeout 取代。
> 最终权威状态见 `docs/C5_CLOSEOUT.md`。

日期：2026-08-10。基线 `f190a4e` → 实现 `34d8386` + 旋钮 `eab1a5c`。

**结论前置（historical S3-stage verdict）：机制已实现并被确定性契约钉住；A/B（4 个 VALID
长任务 × 2 臂 × 3 reps）双臂功能 100% 无回归；但两臂 24 run 的 expansion=compaction=**0**
—— 现有 VALID suite 的单请求转录从未逼近 256k 档，决策点从未到来。按本阶段铁律记
`MECHANISM NOT EXERCISED`，**当时阶段状态为 REMAINS OPEN**（现有 benchmark 从未触发
机制，价值无法判定）。C5-S5 随后以 `S3 EXPERIMENTAL / NOT PROMOTED` 关闭里程碑。**

## Runtime State Model / Policy vs State

`ContextPolicy`（resolver，解析后不变）↔ `ContextBudgetState`（drive 环内活状态）。
唯一决策点 `decide_context_action() -> Keep|Expand|Compact`（纯函数，同输入同动作）。
`context_budget` 兼容镜像永远=initial，活值只在 state —— 镜像不漂移由测试钉住。

## Budget Ladder

`expansion_tiers(profile)`：[256k, 512k, reliable] clamp/dedup/严格递增；小窗口模型退化为单档。
**生产不越 reliable**：审计发现 flash 的 `window(1M) - max_output(384k) ≈ 655k < reliable(786k)`，
safe input ceiling 语义未定 —— 越界仅存在于 repair 强证据路径且被记录，未进任何默认。

## Expansion Signals

| 信号 | 接线 | 来源 |
| --- | --- | --- |
| 重读压力 | ✓ | `RepeatedReadGuard::total_trips()`（新增聚合口）：折叠后 ≥3 次对未变内容的重复读；同批 trips 不得二次消费 |
| repair 升级 | ✓ | engine 按 `TurnKind::Repair` 授予单次强证据 —— 唯一可越 reliable 的路径，越界记录于事件 |
| 影响面增长 | **未接线** | executor 侧无权威聚合值；不造伪信号（`not wired because no authoritative signal exists`） |

Non-expansion：预算压力本身不是证据（决策点≠理由）；折叠前的 trips 无意义；同信号不重复；
到顶只剩折叠。closeout/distractor 条件因无权威状态源，v1 不实现（如实记录）。

## Cache-Aware Decision

`expand-before-compact`：扩张保前缀、折叠重写前缀 —— 有资格的扩张永远先于压缩。
v1 不做 cache optimizer；`cached_input_tokens=0` 不解释为无缓存。

## Durability / Replay

`ContextExpanded` 事件（durable、LocalOnly-projectable 分类、**拒绝公共投影**）；
turn runner 以 AtomicU32 记忆同任务最高预算（goal→repair 连续）；三个 engine 入口以
`EventLog::max_expanded_context_budget()`（复用 `load_last_by_type` 索引）从持久层播种 ——
resume 后不缩回初始档。决策纯函数保证 same events → same state。

## Ablation

`ExecutionOverrides.adaptive_context` + `eval ablate adaptive_context`（control=生产默认 OFF，
ablated=候选 ON —— 方向显式：此旋钮测的是候选）。OFF 逐字节复现 S2；chat 24k 静态；子 agent 静态。

## Eval Validity / Baseline vs Candidate

**Deviation（如实记录）**：E1–E4 bespoke 形状未新造（上下文预算内不可行），载具改为既有
**VALID** 的 4 个最长任务：icg-3（中位对照）/icg-5/c4-r7/c4-r8（80 轮）。当时预期
**C5-E2 history-dependent** 能把转录推过 256k —— **后续事实：E2 随后建成并跑了两轮
VALID shape（96 adapters / 1.1MB），仍未产生 >256k 压力**，见
`docs/C5_E2_HISTORY_DEPENDENT_CASE.md`。

## Functional Results（冻结 `eab1a5c`，24/24 完成）

```
control (OFF): 12/12 PASS · ablated (ON): 12/12 PASS · 无功能回归
rounds: control 19.2 vs ablated 21.2 (+1.9)
```

## Context Metrics / Mechanism（判决项）

```
两臂全部 24 run：expansion_count = 0 · compaction_count = 0
```

根因：run 摘要里的 `tokens=` 是**任务累计计费**（逐轮全前缀重复计费之和），不是单请求
转录大小。这批任务 9–30 步的单请求转录估算仅数万 token，从未越过候选的 256k 起步档 ——
决策点一次都没到来。**候选臂的 +1.9 rounds 与预算机制无关**（机制零触发，两臂在上下文
行为上事实等价；差异是运行方差）。

> 这也顺带修正了一个测量误读：此前把 `tokens=619k` 类数字当作"单请求逼近 786k"，
> 实为累计口径 —— 现有全部 benchmark 任务的真实转录都远小于任何折叠阈值，
> 也就是说**历史上 compaction 在 eval 面上从未触发过**。

## Decision（C5-S5 Closeout 改判，2026-08-11）

```
S3 STATUS: EXPERIMENTAL / NOT PROMOTED
生产默认: Disabled（长期状态，非待判决）
```

含义精确区分：**不是"S3 被证明有效"，也不是"S3 失败"** —— 工程实现完成且被
15 条确定性契约钉住，但产品价值未被证明（E2 两轮 reality check 未能展示折叠压力），
因此不晋升 production。机制留在 ablation seam 后，重新打开条件见
`docs/C5_CLOSEOUT.md`。

机制的确定性正确性已证（15 条新单测：12 决策核 + 3 resolver；全 workspace 绿）；
真实负载证据缺一个**能自然产生 >256k 转录**的 VALID case。不硬造、不降档凑触发
（把 tier 调小到 32k 能"触发"，但那测的不是产品行为）。

## Deferred Questions after C5 Closeout

本节原为"Remaining Risks / S4 Inputs"（把 E2 写作待办）；那些事项已被实际历史处理：

- **E2 已执行**：两轮 VALID shape（telemetryd-e2，1500 files / 96 relevant adapters /
  1.1MB relevant source），6 个候选 probe，0 compaction / 0 expansion —— agent 以
  grep + 范围读把有效转录控制在约 12–16 万 token，>256k 压力未自然出现。
- **S4 Pointer Retention 已 DEFERRED**：仅当未来真实 Dogfooding 出现
  `transcript > 256k → 自然 Compact → 有意义的 post-fold 重读 / RepeatedReadGuard 压力`
  时重新评估。
- **flash safe-ceiling 语义**（window 1M − max_output 384k ≈ 655k < reliable 786k）保留为
  开放问题 —— **不是当前 C5 blocker**，仅在未来 C5-R1 重新打开时需要继续澄清。

重新打开条件与 `docs/C5_CLOSEOUT.md` 一致：由真实使用数据触发（自然折叠 + 折叠后大量
重读）→ 开 **C5-R1 — Adaptive Context Revalidation**，并把该真实任务脱敏后做成新的 E2。
