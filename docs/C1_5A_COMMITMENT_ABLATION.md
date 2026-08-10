# C1.5A — Localization Commitment Ablation

日期：2026-08-08。分支 `feat/coding-real-task-completion-c1`。模型 `deepseek/deepseek-v4-flash`。Run #1 为真值。
生产默认行为零改动：ablation 默认 OFF，只有设了 `LEVELER_EVAL_COMMITMENT_NUDGE=<N>` 的 arm 才生效。

**结论前置：PARTIALLY CONFIRMED。** 机制在两个真实仓上都起作用（first edit 提前 35% / 44%，pre-edit 轮数各降 38% / 46%），但只有 yq 把它转化成了端到端收益（rounds −33%、tokens −36%、仍 PASS）。ripgrep 省下的 29 轮**原样转移到了 post-edit 实现/验证循环**（+65%），总成本几乎不变 —— 说明 commitment 不是它的约束瓶颈。

## 1. Plan Timing（先补的关键测量）

新增 `first_plan_round`，来源是真实的 `update_plan` **工具调用**，不从文本猜。对照组的值由既有 session 事件日志回算（该指标当时尚不存在）。

| Case | firstRelevant@ | firstPlan@ | firstEdit@ | plan→edit |
| --- | ---: | ---: | ---: | ---: |
| 6× ladder 对照 | 2 | 2-3 | 5-9 | **2-5** |
| yq control | 2 | **3** | 31 | **27** |
| ripgrep control | 3 | **1** | 64 | **62** |

**答案是 §1 的 A 支：relevant 早，plan 也早，edit 很晚。** 计划本身不是瓶颈 —— ripgrep 在**第 1 轮**就有结构化计划，然后 62 轮不动手。这直接把生产修复位置定在"有计划之后的承诺"，而不是"计划生成"。

## 2. Ablation 设计

**触发条件**（只用 Agent 自己可见的事实，绝不碰 `relevant_paths` / `first_relevant_file_round` 等 hidden ground truth）：

```
存在 update_plan  且  自 plan 起连续 N 个 model round 没有任何 edit  且  尚未 edit 过
```

**N = 8，由数据推导**：阶梯对照的健康 plan→edit gap 实测为 **2-5**，取 8 严格高于健康上限（不打扰正常行为），又远低于病态值 27 / 62。

**动作**：一次性、通用的 commitment nudge，经**现有的 mid-turn steering 通道**注入（`SteeringSource::take_pending`，drive loop 每轮开头消费）。不含文件名、不含符号、不含 patch，并明确保留"确有具体阻塞则继续探索"的出口。**不禁用任何工具、不强制 apply_patch。**

**注入缝**：`crates/leveler-cli/src/eval_commitment.rs`（eval-only）+ `run_in_session_bounded` 新增一个 `steering` 参数（该函数唯一调用方就是 eval）。默认 `None`，产品路径一行未变。4 条测试锁住语义：plan 后 N 轮才触发、已编辑则永不触发、无 plan 则永不触发、非正整数环境值不武装。

## 3. 结果表

| Case | Arm | FirstRel | FirstPlan | FirstEdit | Rel→Edit | Plan→Edit | Rounds | Tools | InputTokens | Patches | EditFail | Repair | Acceptance | Outcome |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- |
| **yq** | control | 2 | 3 | 31 | 29 | 27 | 72 | 87 | 3,146,679 | 10 | 0 | 0 | ✅ PASS | Completed |
| **yq** | **treatment** | 2 | 4 | **20** | **18** | **16** | **48** | 59 | **2,017,968** | **8** | 0 | 0 | ✅ PASS | Completed |
| | Δ | | | **−35%** | **−38%** | −41% | **−33%** | −32% | **−36%** | −20% | = | = | 不变 | 不变 |
| **ripgrep** | control | 3 | 1 | 64 | 61 | 62 | 100 | 130 | 7,363,475 | 11 | 0 | 0 | ❌ FAIL | budget_limited |
| **ripgrep** | **treatment** | 6 | 4 | **36** | 30 | **32** | 97 | 112 | 6,695,537 | 12 | 0 | 0 | ❌ FAIL | incomplete |
| | Δ | | | **−44%** | −51% | −48% | **−3%** | −14% | **−9%** | +1 | = | = | 不变 | — |

## 4. Pre-edit vs Post-edit（解释两仓分歧的关键）

| Case | Arm | PRE 轮数 | PRE 调用 | POST 轮数 | POST 调用 |
| --- | --- | ---: | ---: | ---: | ---: |
| yq | control | 29 | 45 | 41 | 42 |
| yq | treatment | **18 (−38%)** | 31 | **28 (−32%)** | 28 |
| ripgrep | control | 63 | 92 | 37 | 38 |
| ripgrep | treatment | **34 (−46%)** | 51 | **61 (+65%)** | 61 |

**yq 两个相位一起缩小；ripgrep 只是把预算从探索搬到了实现。** ripgrep 的 post-edit 里有 24 次 shell_command（大型 Rust 仓的 cargo 构建）外加 13 次 read + 5 次 grep —— 它在实现阶段**继续探索**，这不是 commitment nudge 能管的部分。

## 5. 必答问题

**1. Plan 到底早不早？** **很早**。ripgrep plan@1、yq plan@3，都远早于 first edit（gap 62 / 27）。瓶颈在 plan 之后。

**2. Early commitment 是否让 first edit 提前？** **是，两仓都提前**：yq r31→r20（−35%），ripgrep r64→r36（−44%）。模型没有忽略这个 soft nudge。

**3. 是否降低总 rounds/tokens？** **yq 是**（−33% / −36%，双双超过 20% 门槛）；**ripgrep 否**（−3% / −9%）。

**4. 是否损害 correctness？** **没有**。两仓 edit failures 均为 0，repair 均为 0；yq 的 patch 次数反而**下降**（10→8）且仍 PASS、无 false completion；ripgrep patch 11→12（+1，噪声级），acceptance 在**对照组本身就是 False**，treatment 未使其变差，且 `verification_ran` 由 false→true（至少跑到了验证）。**"更早迭代"没有变成"更早乱改"。**

**5. 两仓效果一致吗？** **机制一致、结果不一致**。两仓的 pre-edit 都大幅下降（−38% / −46%），但 yq 的 post-edit 同步下降 32%，ripgrep 的 post-edit 反而增加 65%。

**6. Context 是否仍是独立问题？** **是。** tokens-per-round 几乎不受 ablation 影响：yq 43.7k→42.0k、ripgrep 73.6k→69.0k。token 总量只跟着轮数走。**commitment 修的是 duration，per-round 上下文成本是另一条独立的线。**

## 6. 关于 helm tie-breaker（如实说明取舍）

§11 规定"yq 改善 / ripgrep 不改善"时用 helm 做 tie-breaker。本轮**未运行 helm**，理由：两仓并不矛盾，分歧已被相位数据**定量解释**（ripgrep 省下的 29 轮精确转移到 post-edit 的 +24 轮）。第三个仓能提供的是"收益是否可复制"的又一个样本，改变不了"commitment 有效但非唯一瓶颈"的判定。helm（1363 文件）已抓取待命，若你要第三点数据可直接补跑。

## 7. Verdict

**LOCALIZATION COMMITMENT HYPOTHESIS: PARTIALLY CONFIRMED**

- 因果成立：一个不含任何答案信息的通用 nudge，稳定地把 first edit 提前 35-44%，并显著削减 pre-edit 勘察（−38% / −46%），**不损害正确性**。
- 但它只在 post-edit 循环本身可控时转化为端到端收益（yq：总成本 −33%/−36%）。在 ripgrep 这类大型仓上，节省被 post-edit 的实现/验证循环吸收（+65%），总成本几乎不变。

**C1.5 PRODUCTION FIX: NOT READY**

按 §12 的判定规则：ripgrep 属于"first edit 提前但总 rounds/tokens 不改善"，因此 **Localization Commitment 不能作为单独的 Top-1 生产修复**。它是一个真实、可验证、低风险的组件，但需要与 post-edit 收敛（实现阶段的重复探索 + 构建/验证循环成本）配套设计才有端到端价值。

建议下一步（不实现）：把 post-edit 阶段作为独立测量对象 —— ripgrep treatment 的 post-edit 61 轮里有 24 次构建命令与 18 次读/搜，值得先弄清"实现期继续探索"是必要还是浪费，再决定 C1.5 的最终形态。

## 8. Regression

`cargo fmt --check` ✅ · `cargo check --workspace --all-targets` ✅ 0 error · `cargo test --workspace --no-fail-fast` ✅ 0 failed（含 4 条 ablation-seam 测试）。

## 9. 复现方式

```bash
# control（默认，无 ablation）
leveler eval run --cases evals/realrepo --model deepseek/deepseek-v4-flash
# treatment
LEVELER_EVAL_COMMITMENT_NUDGE=8 leveler eval run --cases evals/realrepo --model deepseek/deepseek-v4-flash
```
