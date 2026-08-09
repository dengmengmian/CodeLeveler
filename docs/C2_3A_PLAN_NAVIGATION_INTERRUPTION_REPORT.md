# C2.3A PLAN NAVIGATION INTERRUPTION REPORT

日期：2026-08-09。分支 `feat/coding-context-efficiency-c2`。  
模型 treatment：`deepseek/deepseek-v4-flash`。  
**单变量**：只解除 forced-plan 对导航的打断；未改 read_file、导航 Prompt、compaction、verifier、runtime、provider。

---

## Final Verdict

# **C2.3A: PASS**

| 门槛 | 结果 |
| --- | --- |
| forced-plan 不再剥夺 navigation tools | ✅ 确定性测试 + 生产代码 |
| 正常导航路径无 `ToolChoice=update_plan` | ✅ |
| Plan 仍可用；mutation 前仍可要求 plan | ✅ |
| False Completion | **0** |
| yq hidden acceptance | ✅ PASS |
| C1 代表 case（MF/EXP/EDIT/R2/WV/BB） | **10/10** 安全语义正确 |
| ripgrep | TurnLimitReached（与历史同类），**expect PASS**，无 provider/false-completion 新问题 |
| Deterministic workspace | **122 suites / 2,539 tests / 0 failed** |

**NEXT: C2.3B — Navigation Discipline**（本阶段未实现）

---

## Current Forced-Plan Architecture（改前）

见审计稿 `docs/C2_3A_PLAN_INTERRUPTION_BASELINE.md`。摘要：

```text
structured_plan_required && !plan_started && explore_rounds >= 2
  → A) tools := [update_plan] ; ToolChoice := Tool("update_plan")
  → B) plan_gate 拒绝一切非豁免工具（含 read/grep/symbol/refs）
  → C) 纯文本响应强制重试 update_plan
  → D) update_plan 成功后恢复 full tools + Auto
```

常量：`PLAN_EXPLORE_ROUNDS = 2`（`drive.rs`）。

**原意**：多步任务要有结构化 plan；允许少量只读探索；mutation 不得先于 plan。  
**副作用**：把「探索预算」做成了**剥夺导航工具的硬墙**。

---

## Root Cause

强制 plan 打断的是 **Locate → Read → Impact Expand** 的连续证据链，不是「模型不愿 plan」：

1. 陌生真实仓里，2 轮只读探索往往不够形成可执行计划。  
2. 阈值到达后模型**物理上**无法继续 grep/read/symbol/refs。  
3. 被迫在薄证据上 `update_plan`，之后再补探索 → **first relevant 很早、first edit 很晚** 的签名。  
4. 被拒绝的导航调用还会污染事件日志（C2.2 已记录「假整文件读」假象）。

---

## Production Change

| 组件 | 改动 |
| --- | --- |
| `gates::plan_gate` | **Explore/导航工具永不因缺 plan 被拒**；mutation 在 multi-step + require_explicit_plan 下仍要求 plan |
| `drive.rs` request 层 | **删除** `tools=[update_plan]` 与 `ToolChoice::Tool("update_plan")`；始终 full tools + `Auto` |
| `drive.rs` empty-response | **删除** 纯文本强制 plan-repair 循环 |
| Soft nudge | 无 plan 且 explore 轮数 ≥ 2 时，**一次性**注入 advisory User 消息；**不**改工具表、**不**强制 ToolChoice |
| 计数 | `plan_explore_rounds_used` 仅用于 nudge 时机，不再 cap 导航 |

### 新 invariant

```text
NoPlan  →  NavigationAllowed  →  PlanMayBeSuggested (soft, once)
NoPlan  →  MutationStillGated (multi-step + explicit plan policy)
NoPlan  ↛  NavigationDenied
NoPlan  ↛  ToolChoice forced to update_plan
```

### What Was NOT Changed

| 项 | 状态 |
| --- | --- |
| `read_file` 实现 / 256KiB 上限 | 未改 |
| `prompts/base.md` 导航段 | 未改（C2.3B） |
| Compaction / token 阈值 | 未改 |
| Verifier / runtime / provider | 未改 |
| Round budget / loop guard / search budget | 保留 |
| `PLAN_EXPLORE_ROUNDS = 2 → 20` | **未做** |

---

## Deterministic Tests

| ID | 测试 | 结果 |
| --- | --- | --- |
| A–D | `past_explore_threshold_navigation_tools_stay_available_with_soft_plan_nudge`：阈值后 read/grep/symbol/refs 仍在工具表；ToolChoice=Auto；无 only-update_plan | ✅ |
| E–F | 同上：soft nudge 出现；阈值后 r3/r4 导航成功 | ✅ |
| G–H | 自主 `update_plan` → `PlanUpdated`；plan 后流程正常 | ✅ |
| I | `complex_task_must_register_a_structured_plan_before_tools_run`：mutation 仍被 plan_gate 挡住 | ✅ |
| J | `text_only_after_explore_does_not_force_plan_repair`：不进入 forced plan 死循环 | ✅ |
| K | `navigation_stays_open_without_a_plan`（gates unit） | ✅ |
| L | 全工具表断言（len>1 + read/grep/find_symbol/find_references/update_plan/apply_patch） | ✅ |
| 回归 | 其余 plan / loop_guard / seeded plan 相关 loop_test | ✅ |

### Workspace gate

```text
cargo fmt --all -- --check     ✅
cargo check --workspace --all-targets  ✅ (既有 unused_mut 警告，与本改动无关)
cargo test --workspace --no-fail-fast  ✅
  122 suites · 2,539 tests · 0 failed
```

---

## Control vs Treatment

### 方法

| Arm | 定义 |
| --- | --- |
| **Control** | 改前行为：C1 Final + C2.3 Navigation Baseline 已记录的同模型运行（`deepseek/deepseek-v4-flash`） |
| **Treatment** | 仅本 C2.3A 生产改动；`LEVELER_EVAL_COMMITMENT_NUDGE` 未设；fixture 未改 |

Treatment 产物：`evals/baselines/c23a-treatment.json`（gitignored）。

### C1 representative gate（Treatment）

| Case | 终态 | expect | rounds | rel@ | plan@ | edit@ | 对照 C1 Final |
| --- | --- | --- | ---: | ---: | ---: | ---: | --- |
| MF1 | Completed | ✅ | 13 | 2 | 4 | 5 | C1: 16 / plan@5 / edit@7 |
| MF2 | Completed | ✅ | 13 | 2 | 3 | 4 | C1: 16 / plan@3 / edit@4 |
| MF3 | Completed | ✅ | 14 | 2 | 4 | 5 | C1: 9 / — / edit@4 |
| EXP1 | Completed | ✅ | 15 | 2 | — | 8 | C1: 11 / — / edit@7 |
| EXP2 | Completed | ✅ | 14 | 1 | — | 6 | C1: 13 / — / edit@7 |
| EDIT1 | Completed | ✅ | 8 | 2 | 3 | 4 | C1: 9 / plan@3 / edit@4 |
| R2 | Completed | ✅ | 7 | 2 | — | 4 | C1: 7 / — / edit@3 |
| WV1 | **CompletedUnverified** | ✅ | 6 | 2 | — | 3 | 语义保持 |
| BB1 | **CompletedUnverified** | ✅ | 6 | 2 | — | 3 | 语义保持 |
| BB2 | **Completed** | ✅ | 5 | 2 | — | 3 | 语义保持 |

- **False Completion = 0**  
- **Half-fix**：MF/EXP/EDIT/R2 隐藏验收全部 PASS（独立 expect）  
- 简单/中等任务无「必须先 plan」退化；多步任务仍能自发 `update_plan`

### 真实仓

| Case | Arm | rounds | rel@ | plan@ | edit@ | rel→edit | expect | 终态 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- | --- |
| **yq** | Control (C1 Final) | 50 | 2 | 4 | 23 | 21 | ✅ | Completed |
| **yq** | Control (C2.3 baseline) | — | 2 | — | 23 | 21 | ✅ | — |
| **yq** | **Treatment** | **33** | **2** | **17** | **21** | **19** | ✅ | Completed |
| **ripgrep** | Control (C2.3 baseline) | 100 | 3 | — | 45 | 42 | 依 run | budget / 诊断 |
| **ripgrep** | Control (C1.4) | 100 | 3 | — | 64 | 61 | ✅ accept | budget_limited |
| **ripgrep** | **Treatment** | **100** | **5** | **2** | **85** | **80** | ✅ accept | TurnLimitReached |

### Navigation Delta（解读）

**yq（主证据）**

- 仍 early relevant（r2）。  
- **plan 延后到 r17**（不再在探索中段被强制写 plan）——符合「Plan = Execution Aid」。  
- rounds **50 → 33（−34%）**，tokens **2.13M → 1.56M**。  
- first edit 仍在 ~r21，略优或持平；**correctness 不退化**。  
- 支持因果：解除强制打断后，可连续导航更久再 plan，整体更短路径完成。

**ripgrep（非阻塞诊断）**

- 仍在 round ceiling 结束；**hidden acceptance 仍 PASS**（实现形状对，预算内未收敛）。  
- first edit **未因 C2.3A 提前**（r85）——说明 plan 墙不是它唯一瓶颈；C2.3 baseline 已指出 post-edit / 实现循环与导航纪律问题，属 **C2.3B**。  
- plan@2：模型仍可**自愿**早 plan（base prompt 仍鼓励 multi-step plan）；C2.3A 只保证**不能剥夺导航**，不禁止早 plan。  
- 无新的 provider protocol / false completion / verifier 语义破坏。

**合成任务**

- C1 代表集 **10/10** 正确性与安全终态保持。  
- MF 的 plan→edit 间隔健康（约 1 轮），无无限探索。

---

## Correctness Regression

| 检查 | 结果 |
| --- | --- |
| False completion | **0 / 12** |
| WV1 / BB1 Unverified | ✅ 未升格 |
| BB2 完成且 expect PASS | ✅ |
| yq accept | ✅ |
| ripgrep accept（独立 expect） | ✅（终态 budget） |
| loop_guard_trips（treatment） | 全 0 |
| 无限探索（合成） | 未观察到；round budget 仍生效 |

---

## C1 Representative Gate

**PASS**（见上表 10/10 + 安全语义）。  
ripgrep 仍为非阻塞长任务诊断，与 C1 Final 立场一致。

---

## 风险与后续

1. **base prompt 仍写**「optional explore 后 first substantive action 须 update_plan」——与 C2.3A harness 解耦；C2.3B 再统一 Navigation Discipline 文案。  
2. **软 nudge 仅一次**；不会因未 plan 反复强制。探索收敛（信息增量）留给后续，**不用**固定 20/26 轮。  
3. ripgrep 证明：**只拆 plan 墙不够**；需要 C2.3B 的 Search→Read→Expand→Edit 纪律与 C2 context 回收。

---

## 变更文件清单

| 路径 | 角色 |
| --- | --- |
| `crates/leveler-agent/src/executor/gates.rs` | plan_gate：导航永不因缺 plan 拒绝 |
| `crates/leveler-agent/src/executor/drive.rs` | 去 forced repair；soft nudge |
| `crates/leveler-agent/tests/loop_test.rs` | 契约测试改写 |
| `CHANGELOG.md` | Unreleased Changed |
| `docs/C2_3A_PLAN_INTERRUPTION_BASELINE.md` | 改前审计 |
| `docs/C2_3A_PLAN_NAVIGATION_INTERRUPTION_REPORT.md` | 本报告 |

---

## Final Verdict（复述）

**C2.3A: PASS**

Plan 不再是获取代码证据的许可证；导航在缺 plan 时保持开放；mutation 门控与 C1 正确性保持。  
**NEXT: C2.3B — Navigation Discipline**（未在本阶段实现）。
