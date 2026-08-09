# C2.3A — Plan Navigation Interruption Baseline (Pre-Change Audit)

日期：2026-08-09。分支 `feat/coding-context-efficiency-c2`。  
**本文件是改动前的审计记录，不含 production 实现。**

---

## 1. Current Forced-Plan Architecture

### Trigger

| 条件 | 来源 |
| --- | --- |
| `policy.require_explicit_plan` | `TurnPolicy` / engine `explicit_plan` |
| `task_needs_structured_plan(task)` | 任务文本含 ≥2 个编号项或 ≥3 个 bullet（`dispatch.rs`） |
| 两者同时为真 | `structured_plan_required`（`drive.rs`） |

### Explore accounting

```text
const PLAN_EXPLORE_ROUNDS: u32 = 2;   // drive.rs
plan_explore_rounds_used              // per drive, starts 0
```

每轮模型回合结束后，若仍 `structured_plan_required && !structured_plan_started`：

```text
plan_explore_rounds_used = min(used + 1, PLAN_EXPLORE_ROUNDS)
```

### Path when threshold reached

```text
Normal navigation (read/grep/… allowed for first 2 rounds)
    ↓
plan_explore_rounds_used >= 2  &&  no plan yet
    ↓
A) REQUEST LAYER (drive.rs ~459–469)
   tools := filter(name == "update_plan")   // 导航工具从表上消失
   tool_choice := Tool("update_plan")       // 强制调用计划工具
    ↓
B) EXECUTION LAYER (gates::plan_gate)
   非豁免工具一律 Refuse("Explore budget used. Call update_plan…")
   含 read_file / grep / find_symbol / find_references
    ↓
C) EMPTY-RESPONSE REPAIR (drive.rs ~703–712)
   若模型只回 prose 且 plan_repair_required：
   注入 User 反馈，continue 下一轮，仍强制 update_plan
    ↓
D) RESTORE
   structured_plan_started = true（成功 update_plan）
   下一轮 full tools + ToolChoice::Auto
```

### Who does what

| 字母 | 行为 | 位置 |
| --- | --- | --- |
| **A** | 移除 navigation tools + forced ToolChoice | `drive.rs` `plan_repair_required` 分支 |
| **B** | 拒绝导航工具执行 | `gates::plan_gate` + `explore_rounds` |
| **C** | 纯文本不得结束，必须重试 plan | `drive.rs` empty calls + plan_repair |
| **D** | plan 成功后恢复 | `structured_plan_started` 置位 |

### What is still allowed before plan (today)

| 工具类 | 前 2 轮 | 阈值用尽后 |
| --- | --- | --- |
| explore（read/grep/list/find_*/symbol/refs/web/skill） | ✅ | ❌ 被 A+B 双重切断 |
| `update_plan` / user input / request_permissions | ✅ | ✅ exempt |
| mutation（apply_patch / replace / …） | ❌ 被 plan_gate | ❌ |

### Original intent (from code comments / tests)

1. **多步任务必须有 machine-readable plan**，避免散文 checklist 冒充结构。  
2. **允许少量只读探索** 再要求 plan（W2-04 / `PLAN_EXPLORE_ROUNDS`）。  
3. **Mutation 不得在 plan 之前发生**（`complex_task_must_register_a_structured_plan_before_tools_run`）。  
4. **模型装死写 prose 时** 用 forced tool_choice 把 turn 拉回 `update_plan`。

问题不在「要 plan」，而在 **用剥夺导航工具 + forced choice 当警察**，把 (2) 的探索预算做成了硬墙。

---

## 2. Evidence this interrupts real navigation

From `docs/C2_3_NAVIGATION_BASELINE.md`（改动前实测）：

| Run | plan_gate / explore_budget 拒绝 | 被拒示例 |
| --- | ---: | --- |
| yq | 有 | `read_file(cmd/constant.go)` 等真实导航读 |
| ripgrep | 有 | `read_file(crates/core/flags/defs.rs)` |

签名：**first relevant 极早，first edit 极晚** — 模型在闸门后只能带着薄证据写 plan，再补探索。

---

## 3. C2.3A target invariant (for the change)

```text
NoPlan  →  NavigationAllowed  →  PlanMayBeSuggested (soft)
NoPlan  ↛  NavigationDenied
NoPlan  →  Mutation still gated (plan before edit for multi-step)  // keep
```

- **禁止**：`tools = [update_plan]` + `ToolChoice::Tool("update_plan")` 作为正常导航路径。  
- **禁止**：仅因未 plan 而拒绝 grep/read/symbol/refs。  
- **允许**：soft advisory nudge；mutation 前仍可要求 plan。  
- **禁止**：`PLAN_EXPLORE_ROUNDS = 2 → 20` 式搬家。  
- **单变量**：不改 read_file / navigation prompt / compaction / verifier / runtime。

---

## 4. Files that will change in C2.3A

| 文件 | 角色 |
| --- | --- |
| `crates/leveler-agent/src/executor/gates.rs` | plan_gate：explore 永不因缺 plan 拒绝 |
| `crates/leveler-agent/src/executor/drive.rs` | 去掉 tool 裁剪 / forced choice / text repair 硬循环；改 soft nudge |
| `crates/leveler-agent/tests/loop_test.rs` | 改写依赖旧行为的测试 + 新契约测试 |
| `crates/leveler-agent/src/executor/gates.rs` tests | 更新 unit 期望 |

**不改**：`read_file` 实现、`prompts/base.md` 导航段、compaction、verifier、provider、runtime。

---

## 5. Pre-change test anchors (behavior to invert or keep)

| 测试 | 旧期望 | C2.3A |
| --- | --- | --- |
| `exhausted_plan_explore_budget_restricts_the_next_request…` | 第 3 轮起 tools 仅 update_plan | **改写**：导航工具仍在，ToolChoice Auto |
| `plan_repair_text_only_response_is_retried…` | 纯文本强制再 plan | **改写**：不强制 |
| `reading_is_allowed_while_explore_budget…` | used=2 拒绝 read | **改写**：read 始终 Allow |
| `complex_task_must_register_a_structured_plan_before_tools_run` | patch 在 plan 前被拒 | **保留** |
| 简单任务 / loop guard / search budget | — | **不退化** |

---

## 6. Causal ablation plan (post-change)

| Arm | 配置 |
| --- | --- |
| Control | 本 commit 之前的 forced-plan 行为 |
| Treatment | 仅 C2.3A 生产改动 |

Cases：yq-doc-count、ripgrep-total-count、MF/EXP 子集。  
指标：见任务说明 §10（first relevant/plan/edit、impact recall、acceptance…）。  
**本 baseline 不跑模型**；结果写入 `docs/C2_3A_PLAN_NAVIGATION_INTERRUPTION_REPORT.md`。
