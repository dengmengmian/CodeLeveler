# CodeLeveler 真实用户验收 · 现状分析（Phase 0）

> 结论先行：验收应**扩展**现有 `crates/leveler-eval` + `evals/` + `leveler eval`，
> 禁止平行 runner。地基已覆盖验证驱动判定、质量指标、三层门禁与 TUI 客户端路径测试；
> 缺口在 regression 目录接线、TTFF/SilentDuration、以及把验收产物固化为门禁证据。

关联文档（不重复展开）：

| 文档 | 角色 |
|------|------|
| [`CURRENT_EVAL_ARCHITECTURE.md`](CURRENT_EVAL_ARCHITECTURE.md) | Eval 数据流 / 类型 / CLI 权威描述 |
| [`AGENT_EVAL_SYSTEM_DESIGN.md`](AGENT_EVAL_SYSTEM_DESIGN.md) | 质量门禁设计与指标状态 |
| [`CURRENT_EVAL_ANALYSIS.md`](CURRENT_EVAL_ANALYSIS.md) | 验收视角摘要（较早） |
| [`ACCEPTANCE_BASELINE_REPORT.md`](ACCEPTANCE_BASELINE_REPORT.md) | 上一轮验收基线草稿 |

---

## 1. Eval 架构（当前能力）

### 1.1 Task 定义

- YAML `EvaluationCase`：`id/name/repo?/base_ref?/files/task/max_rounds/expect/recovery?`
- **synthetic**：无 `repo` 时 `files` 即工作区
- **real-repo**：`repo` + `base_ref` clone 固定 commit，`files` 作 overlay 注入 bug
- **expect**：独立验收命令；`passed = completed && expect_passed`（模型自报不算过）

### 1.2 Scenario / 套件

| 路径 | 用途 |
|------|------|
| `evals/smoke` | quick 门禁（3 例） |
| `evals/core` / `evals/hard` | daily 主体 |
| `evals/scenarios/debugging` | recovery（compile/test fail） |
| `evals/scenarios/feature` | 真实仓（ripgrep） |
| `evals/scenarios/permission` | 金丝雀密钥保护 |
| `evals/scenarios/tui` | 文档登记 TUI 测试（非 YAML case） |
| `evals/regression` | **本轮补齐**：失败固化回归集（见改造方案） |

### 1.3 Runner

- 入口：`leveler eval run|compare|ablate|quick|daily|release|trend`
- 实现：`crates/leveler-cli/src/eval_cmd.rs` → `run_eval` / `run_eval_case`
- 路径：orchestrated（默认）/ `--direct` / `--no-verify-gate`（消融）
- 工作区：临时 disposable；`SignalCollector` 折叠事件 → 工具/循环/验证信号
- 三层门禁：
  - `quick` → `evals/smoke`
  - `daily` → `evals/core` + `hard` + `scenarios/debugging`
  - `release` → smoke + core + hard + **全部** `scenarios`

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

真实用户路径（本验收采用）：

1. **`leveler eval …`**：正式 binary / `cargo run` 启动的 orchestrated agent（非内部 API 注入）
2. **TUI 客户端路径测试**：`tui_path_soak`、`tui_session_e2e`（与 TUI 同协议路径；scripted provider 仅用于稳定性）

交互式全键盘多小时 session 作为补充证据，不作唯一自动门禁（见 plan Risks）。

---

## 3. 缺失能力

| 缺口 | 影响 | 优先级 |
|------|------|--------|
| `evals/regression/` 未建立 / 未入门禁 | 失败 case 无固化重跑入口 | P0 基建 |
| TTFF / SilentDuration 未度量 | 无法量化「用户多久看到反馈」 | P1 指标 |
| TUI Stability 未进 QualityScore | 综合分缺 10% 分量（诚实 None） | P2 |
| daily 完整跑通与问题闭环 | 基线草稿 daily 段未完成 | P0 验收 |
| 假完成 / incomplete 归因闭环 | daily 部分结果见 incomplete+expect 绿 | P1 Agent |
| 交互 Ctrl+C 文案回归用例 | 靠 e2e/i18n，缺统一验收日志 | P2 |

---

## 4. 风险

1. **模型/Provider 主导成功率**：无真实 key 时只能证明 harness/TUI 结构，不能宣称 agent 质量绿。
2. **Eval ≠ 全交互 TUI**：`leveler eval` 是真实 agent 编排路径，不是完整键盘 TUI；二者必须**组合**才覆盖用户面。
3. **Incomplete + expect 通过**：代码可能已对但 turn 未 `completed` → 门禁失败；若只盯 false_completion 会漏「不会收口」。
4. **长任务 / 20min+**：墙钟与预算限制下可能截断；需诚实记录而非假绿。
5. **「修到零问题」无界**：有限门禁 = P0 清零 + quick/regression 绿 + 残留 P2 列明。

---

## 5. 改造方案（复用现有，不平行）

| 步骤 | 动作 | 改哪里 |
|------|------|--------|
| A | 写本分析 + `BASELINE_REPORT.md` | `evals/` 文档 |
| B | 建 `evals/regression/` + README；daily/release 可加载或 `leveler eval run --cases evals/regression` | cases + 可选 `run_tier` 接线 |
| C | 真实跑 `leveler eval quick`（及可负担的 daily 样本） | 证据 → scratch + history |
| D | TUI：`cargo test -p leveler-app --test tui_path_soak` + `leveler-tui tui_session_e2e` | 稳定性门禁 |
| E | 失败 case 固化进 regression；P0/P1 修代码 → rebuild → 重跑 | agent/tui/eval |
| F | `AGENT_ACCEPTANCE_REPORT.md` 对齐证据数字 | `evals/` |

**明确不做**：新建 crate、第二套 benchmark runner、用 mock 代理「agent 质量绿」。
