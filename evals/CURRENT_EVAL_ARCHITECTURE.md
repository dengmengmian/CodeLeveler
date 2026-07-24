# 现有 Eval 架构分析（Phase 1）

> 目的：在动手扩展前，把现有 eval 系统的入口、数据流、指标、扩展点讲清楚，
> 供后续把它升级为"真实用户级 Agent 验收门禁"时对照。**本文档只描述现状，不含改动。**

## 结论先行

现有 `crates/leveler-eval` + `evals/` + `leveler eval` CLI 已经是一套**验证驱动**（非自报完成）的
Agent 评测流水线，覆盖了 8 大目标里的 ~6 个。缺的不是地基，是三块**场景/接线**：
真实大仓 case、权限评分 case、把已存在的 TUI 测试纳入统一门禁。

| # | 目标 | 现状 | 缺口 |
|---|------|------|------|
| 1 | 能否完成真实任务 | ✅ `expect` 命令独立判定 | 缺大仓/长上下文 case |
| 2 | 是否正确用工具 | ✅ `eval_signals` 采集工具调用/是否打开相关文件 | — |
| 3 | 是否正常结束 | ✅ `TerminationClass`（7 类） | — |
| 4 | 是否空转 | ✅ `loop_guard_trips` / `arg_error_streak` | — |
| 5 | 是否错误宣布完成 | ✅ `false_completion_rate`（本轮已加） | — |
| 6 | TUI 交互是否正常 | 🟡 已有 2 套 TUI 测试，但不在 `leveler eval` 门禁内 | 接线 |
| 7 | 权限系统是否可靠 | 🟡 有权限内核+审批，但无评分 case | 加 permission 场景 |
| 8 | 多 Agent 是否稳定 | 🟡 soak 覆盖 goal 模式 | 显式多 Agent 场景 |

## 数据流

```
evals/**/*.yaml                        # case 定义（声明式）
   │  EvaluationCase::load_dir
   ▼
leveler eval run|compare|ablate        # crates/leveler-cli/src/eval_cmd.rs
   │  run_eval() → 逐 case × 逐 repetition
   ▼
run_eval_case()                        # 单 case 执行器
   ├─ 建 workspace：synthetic(空仓+files) 或 repo(git clone --local + files 覆盖)
   ├─ Application::assemble → 跑 orchestrated / direct / no_verify_gate 三种路径
   ├─ SignalCollector 折叠事件流 → TrajectorySignals（工具/循环/错误信号）
   └─ 跑 case.expect 命令 → expect_passed（独立于模型自报的 completed）
   ▼
CaseResult { completed, expect_passed, termination, rounds, tokens, latency, … }
   ▼
EvalReport（聚合指标）→ print_eval_report + BaselineDocument（JSON 落盘）
```

## 关键类型（`crates/leveler-eval/src/lib.rs`）

- **`EvaluationCase`**：`id/name/repo?/base_ref?/files/task/max_rounds/expect`。
  - `repo` + `base_ref`：clone 真实 git 仓到指定 commit，`files` 作为 overlay 注入 bug/失败测试。
    **这是接入 ripgrep 等大仓的现成机制，无需复制仓库。**
  - `files`（无 `repo` 时）：即整个 workspace，用于 synthetic case。
- **`ExpectCommand`**：`program + args`，退出码 0 即通过。验收与"模型说完成"完全解耦。
- **`CaseResult`**：`completed`（模型/门过了）与 `expect_passed`（独立命令过了）分开记。
  `passed() = completed && expect_passed`。
- **`TerminationClass`**：`Completed / BudgetLimited / UsageLimited / Blocked / Incomplete /
  InfrastructureFailed / Failed`——结束边界，正交于对错。
- **`FailureCategory`**（首因）：`Understanding / Localization / Planning / Editing / Tooling /
  Context / Verification / Environment / Runtime`，由 `classify_failure(TrajectorySignals)` 自动归因。

## 指标（`EvalReport` 方法）

| 指标 | 方法 | 说明 |
|------|------|------|
| completion_rate | `completion_rate()` | passed/total |
| **false_completion_rate** | `false_completion_rate()` | `completed && !expect_passed`，头号信号（本轮新增） |
| avg_rounds | `avg_rounds()` | 平均轮数（效率） |
| failure_breakdown | `failure_breakdown()` | 按首因分类计数 |
| unstable_case_ids | `unstable_case_ids()` | 跨 repetition 结果不稳定的 case |
| model_gap / effort_gap | `Comparison::of` | 双模型能力/效率差 |
| ablation delta | `Ablation::of` | 单旋钮开/关的净影响（saved/hurt 列表） |

信号来源 `crates/leveler-cli/src/eval_signals.rs`：`tool_calls`、`loop_guard_trips`
（执行器无进展循环守卫触发次数）、`arg_error_streak`（同工具连续报错）、是否打开 overlay 相关文件。

## CLI 入口（`crates/leveler-cli/src/cli.rs` → `eval_cmd.rs`）

```
leveler eval run     --model M --cases DIR [--direct] [--no-verify-gate] [--repetitions N] [--json-out P]
leveler eval compare M_A M_B --cases DIR [--repetitions N] [--json-out P]
leveler eval ablate  KNOB --model M --cases DIR [--direct] [--repetitions N] [--json-out P]
```

- 三条执行路径：orchestrated（默认全套脚手架）/ `--direct`（裸工具循环）/
  `--no-verify-gate`（去掉验证门+修复轮，量化 verify→repair 的救回率）。
- `--json-out` 落 `BaselineDocument`，带 `BaselineMeta`（git_sha、模型、mode、repetitions、
  引擎版本、case 组成），保证以后可复现对比。
- checkpoint（append-only JSONL）：长跑被中断也能恢复已完成 case。

## 已有的 TUI 测试（现状：在 `cargo test`，不在 `leveler eval`）

1. `crates/leveler-tui/tests/tui_session_e2e.rs`：TestBackend 无头驱动，
   `reduce(Action::Key/Runtime)` 打命令、喂事件、断言屏幕内容。**最接近"启动 TUI 点一圈"**。
2. `crates/leveler-app/tests/tui_path_soak.rs`：按 TUI 完全相同的客户端路径驱动
   `InProcessRuntimeClient`（subscribe → SubmitMessage/RunGoal → 排空 `RuntimeEvent` 到终态），
   scripted mock provider，累计模型轮数 ≥80，**空转/挂起当成功 = 硬失败**（墙钟超时无终态事件）。

## 扩展点（下阶段落地时对号入座）

| 要加的能力 | 用现有哪个扩展点 | 是否需要动核心 |
|-----------|----------------|--------------|
| 真实大仓 case（ripgrep…） | `EvaluationCase.repo + base_ref`，新增 `evals/scenarios/**/*.yaml` | 否（纯 case） |
| 权限评分 case | 新 `expect` 断言 + 权限 profile；可能需要 case 级 permission 字段 | 视断言方式，可能小改 |
| TUI 纳入门禁 | 把上述 2 个测试暴露为 `leveler eval` 可触发的一等场景 | 小接线 |
| 回归集 | `evals/regression/` 目录 + 历史失败 case 固化 | 否（纯 case） |
| Agent 质量指标补全 | `EvalReport` 加方法（同 false_completion 的做法） | 否（纯 metric） |

## 下一步（对应用户分阶段计划）

- Phase 2：`evals/scenarios/{basic_task,debugging,feature,long_context,permission,tui}/` 场景化。
- Phase 3：ripgrep 固定 commit 接入（`repo` 机制），先只上 ripgrep，tokio/starship 克隆编译过重后置。
- Phase 4：把 TUI 两套测试提升为门禁一等公民。
- Phase 6：其余质量指标（verification_rate / recovery_success_rate / permission_violation）。
- Phase 7：`evals/regression/`。
