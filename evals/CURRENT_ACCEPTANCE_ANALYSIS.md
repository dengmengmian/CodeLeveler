# CodeLeveler 真实用户验收 · 现状分析（Phase 0）

> 结论先行：验收应**扩展**现有 `crates/leveler-eval` + `evals/` + `leveler eval`，
> 禁止平行 runner。地基已覆盖验证驱动判定、质量指标、三层门禁、TUI 客户端路径测试，
> 以及 **`evals/regression/` 独立回归入口**。剩余缺口主要是 TTFF/SilentDuration
> 度量与 TUI Stability 折进 QualityScore。

**文档刷新**：与仓库 tip 同步（见 `BASELINE_REPORT.md` Live re-verify 的 tip SHA）；
功能验收 SHA 见同文件的 functional re-verify 行。

关联文档（不重复展开）：

| 文档 | 角色 |
|------|------|
| [`CURRENT_EVAL_ARCHITECTURE.md`](CURRENT_EVAL_ARCHITECTURE.md) | Eval 数据流 / 类型 / CLI 权威描述 |
| [`AGENT_EVAL_SYSTEM_DESIGN.md`](AGENT_EVAL_SYSTEM_DESIGN.md) | 质量门禁设计与指标状态 |
| [`CURRENT_EVAL_ANALYSIS.md`](CURRENT_EVAL_ANALYSIS.md) | 验收视角摘要（较早） |
| [`ACCEPTANCE_BASELINE_REPORT.md`](ACCEPTANCE_BASELINE_REPORT.md) | 上一轮验收基线草稿 |
| [`BASELINE_REPORT.md`](BASELINE_REPORT.md) | Phase 1 正式基线 |
| [`AGENT_ACCEPTANCE_REPORT.md`](AGENT_ACCEPTANCE_REPORT.md) | 验收结论与指标 |

---

## 1. Eval 架构（当前能力）

### 1.1 Task 定义

- YAML `EvaluationCase`：`id/name/repo?/base_ref?/files/task/max_rounds/expect/recovery?`
- **synthetic**：无 `repo` 时 `files` 即工作区
- **real-repo**：`repo` + `base_ref` clone 固定 commit，`files` 作 overlay 注入 bug
- **expect**：独立验收命令；`passed = completed && expect_passed`（模型自报不算过）

### 1.2 Scenario / 套件

| 路径 | 用途 | 状态 |
|------|------|------|
| `evals/smoke` | quick 门禁（3 例） | ✅ 在用 |
| `evals/core` / `evals/hard` | daily 主体 | ✅ 在用 |
| `evals/scenarios/debugging` | recovery（compile/test fail） | ✅ daily 已挂 |
| `evals/scenarios/feature` | 真实仓（ripgrep） | ✅ release |
| `evals/scenarios/permission` | 金丝雀密钥保护 | ✅ release |
| `evals/scenarios/tui` | 文档登记 TUI 测试（非 YAML case） | ✅ 登记 |
| `evals/regression/` | 失败固化回归集（`reg-*` id） | ✅ **已建立**；入口 `leveler eval run --cases evals/regression`（独立门禁，不并入 daily 以免与 core 重复跑同一内容） |

### 1.3 Runner

- 入口：`leveler eval run|compare|ablate|quick|daily|release|trend`
- 实现：`crates/leveler-cli/src/eval_cmd.rs` → `run_eval` / `run_eval_case`
- 路径：orchestrated（默认）/ `--direct` / `--no-verify-gate`（消融）
- 工作区：临时 disposable；`SignalCollector` 折叠事件 → 工具/循环/验证信号
- 三层门禁：
  - `quick` → `evals/smoke`
  - `daily` → `evals/core` + `hard` + `scenarios/debugging`
  - `release` → smoke + core + hard + **全部** `scenarios`
- 回归：`leveler eval run --cases evals/regression`（与 quick/daily 同 runner，非平行 harness）

### 1.4 Validator / Result / Report

- Validator：case 级 `expect` + 可选 verification gate（agent 内 build/test）
- Result：`CaseResult`（completed / expect_passed / termination / tool_calls / loop_guard_trips / verification_ran / is_recovery / tokens / latency）
- Report：`EvalReport` 聚合 + `QualityScore`（缺测分量归一化，不捏造）
- 落盘：`--json-out` → `BaselineDocument`；checkpoint JSONL 可断点续跑
- 趋势：`leveler eval trend --history evals/history`

### 1.5 已实现指标

| 指标 | 状态 |
|------|------|
| Task Success Rate | ✅ `completion_rate` |
| False Completion Rate | ✅ |
| Verification / Validation Rate | ✅ |
| Loop Rate | ✅ |
| Tool Efficiency | ✅ |
| Recovery Rate | ✅（recovery case 标记时） |
| Quality Score 0..100 | ✅ |
| TTFF / SilentDuration | ❌ 未入 eval（runtime 有 CommandProgress 事件） |
| TUI Stability 进 Score | 🟡 soak/e2e 绿，未折算进 QualityScore |

---

## 2. Agent 架构（验收相关）

```
User → Terminal → leveler (CLI/TUI)
         → leveler-app (composition)
         → leveler-engine
         → agent loop (leveler-agent) + orchestrator + verifier
         → tools (leveler-tools) → execution (permissions, shell, FS)
         → RuntimeEvent → TUI (leveler-tui)
```

| 子系统 | 关键点 | 验收含义 |
|--------|--------|----------|
| Agent Loop | `leveler-agent` executor/drive | 工具轮次、取消、循环守卫 |
| Tool System | `leveler-tools` | 读写/搜索/shell 选择是否合理 |
| Permission | `leveler-execution` PermissionProfile | Safe/Write/Danger；执行层拦截非 UI |
| Command Executor | `leveler-execution` command/background | 心跳 `CommandProgress`（L2） |
| TUI Event Flow | event_bridge → RuntimeEvent → reducer | Cancelled ≠ Blocked；进度可见 |
| Stop 语义 | `StopReason` → `TurnCancelled` / `TurnIncomplete` | 用户取消必须 Cancelled |
| Graph 收口 | `leveler-engine` `node_status` | CloseoutForced / Incomplete+mutation 可进 verify（`9627123`） |

真实用户路径（本验收采用）：

1. **`leveler eval …`**：正式 binary / `cargo run` 启动的 orchestrated agent（非内部 API 注入）
2. **TUI 客户端路径测试**：`tui_path_soak`、`tui_session_e2e`（与 TUI 同协议路径；scripted provider 仅用于稳定性）

交互式全键盘多小时 session 作为补充证据，不作唯一自动门禁（见 plan Risks）。

---

## 3. 缺失能力（相对 live 树刷新后）

| 缺口 | 影响 | 优先级 | 状态 |
|------|------|--------|------|
| regression 目录 | — | — | ✅ **已落地**：`evals/regression/{README,reg-*.yaml}` + `leveler eval run --cases evals/regression` |
| TTFF / SilentDuration 未度量 | 无法量化「用户多久看到反馈」 | P2 指标 | 仍缺 |
| TUI Stability 未进 QualityScore | 综合分缺 10% 分量（诚实 None） | P2 | 仍缺；soak 本身绿 |
| daily 完整跑通与问题闭环 | 宽集耗时长 | P2 覆盖 | 未本轮强制全跑；quick 门禁绿 |
| 假完成 / incomplete 归因闭环 | 绿代码报 Failed | P1 | ✅ 产品侧 `node_status` 已修；quick re-verify 3/3 |
| 交互 Ctrl+C 全键盘 PTY | 长 session 未全覆盖 | P2 | soak/e2e + 静态 Cancelled 映射 |

---

## 4. 风险

1. **模型/Provider 主导成功率**：无真实 key 时只能证明 harness/TUI 结构，不能宣称 agent 质量绿。
2. **Eval ≠ 全交互 TUI**：`leveler eval` 是真实 agent 编排路径，不是完整键盘 TUI；二者必须**组合**才覆盖用户面。
3. **Incomplete + expect 通过**（历史）：代码可能已对但 turn 未 `completed` → 已用 `node_status` 缓解；仍须盯 false_completion **与** 收口类失败。
4. **长任务 / 20min+**：墙钟与预算限制下可能截断；需诚实记录而非假绿。
5. **「修到零问题」无界**：有限门禁 = P0 清零 + quick/regression 入口绿 + 残留 P2 列明。

---

## 5. 改造方案（复用现有，不平行）

| 步骤 | 动作 | 状态 |
|------|------|------|
| A | 写本分析 + `BASELINE_REPORT.md` | ✅ |
| B | 建 `evals/regression/` + README；`leveler eval run --cases evals/regression` | ✅ |
| C | 真实跑 `leveler eval quick` | ✅（含 re-verify） |
| D | TUI soak + e2e | ✅ |
| E | P0/P1：`node_status` 假失败修复 + 重跑 | ✅（`9627123`） |
| F | `AGENT_ACCEPTANCE_REPORT.md` | ✅ |
| G | TTFF / Score 接 TUI | 后续可选 |

**明确不做**：新建 crate、第二套 benchmark runner、用 mock 代理「agent 质量绿」。
