# C5-S3 Adaptive Context

日期：2026-08-10。基线 `f190a4e` → 实现 `34d8386` + 旋钮 `eab1a5c`。

**结论前置：机制已实现并被确定性契约钉住；A/B（4 个 VALID 长任务 × 2 臂 × 3 reps）双臂
功能 100% 无回归；但两臂 24 run 的 expansion=compaction=**0** —— 现有 VALID suite 的单请求
转录从未逼近 256k 档，决策点从未到来。按本阶段铁律：`MECHANISM NOT EXERCISED`，
**S3 REMAINS OPEN**，生产默认保持 Disabled。**

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
**VALID** 的 4 个最长任务：icg-3（中位对照）/icg-5/c4-r7/c4-r8（80 轮）。E1–E4 仍是 S3 收口的
待办，尤其 **C5-E2 history-dependent** —— 它正是能把转录推过 256k 的形状。

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

## Remaining Risks / S4 Inputs

- 收口 S3 需要 C5-E2 形状：多大文件读取/百步级任务，转录自然过 256k，六项闸门 VALID。
  scale-s1500 fixture 是现成底材（目前 UNVERIFIED，需补 reference）。
- compaction 在真实 eval 面从未触发 → S4（指针化保留）的收益验证同样需要 E2 形状先行。
- flash 的 safe-ceiling 算术（655k < 786k）值得在 S4 前单独澄清 window 语义。
