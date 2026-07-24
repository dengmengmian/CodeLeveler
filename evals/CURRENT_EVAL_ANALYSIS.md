# 现有 Eval 分析（验收 Phase 0）

> 结论先行：无需新建 benchmark/runner。现有 `crates/leveler-eval` + `evals/` + `leveler eval`
> 已能承载真实用户验收：验证驱动判定、11 指标（含 false_completion_rate）、三层门禁、趋势/回归。
> 架构细节见 [`CURRENT_EVAL_ARCHITECTURE.md`](CURRENT_EVAL_ARCHITECTURE.md)（权威）与
> [`AGENT_EVAL_SYSTEM_DESIGN.md`](AGENT_EVAL_SYSTEM_DESIGN.md)。本文只补"验收怎么用它"。

## 当前能力（验收可直接用）

| 验收目标 | 用现有什么 |
|---------|-----------|
| 能否完成真实任务 | `leveler eval quick/daily/release` 真实模型跑 case，`expect` 独立判定 |
| 错误宣布完成 | `false_completion_rate`（completed && !expect_passed） |
| 是否空转 | `loop_rate`（loop_guard_trips）、`avg_tool_calls` |
| 工具是否合理 | `tool_efficiency`、eval_signals（是否打开相关文件） |
| 是否验证结果 | `validation_rate`（跑过 build/test） |
| 失败恢复 | `recovery_rate` + `scenarios/debugging/recovery-*` |
| 权限有效性 | `scenarios/permission/`（金丝雀密钥保护，执行层校验非 UI） |
| TUI 稳定性 | `tui_path_soak`（hang=硬失败）、`tui_session_e2e`（当前绿） |
| 综合分 | `QualityScore`（0..100） |

## 缺失能力（验收会暴露、需补）

- **TTFF / SilentDuration / FalseBlockedRate / UserInterruptAccuracy**：运行透明度指标未入 eval（对应
  runtime 可观测性 L5；L1 假 blocked 用词、L2 命令心跳已提交 89f2bba）。
- **交互式 TUI 断言**（Ctrl+C 后状态、滚动、输出丢失）：靠 soak/e2e 测试覆盖，非 eval case。
- **真实大仓 GREEN**：ripgrep 场景已 RED 验证，GREEN 需真实模型跑（本次用 deepseek-v4-pro 验）。

## 扩展点

- 加真实任务 → `evals/scenarios/**` 或 `evals/{core,hard}`（同 schema）。
- 加运行透明度指标 → 同 false_completion 做法给 `EvalReport` 加方法（L5）。
- 结果趋势 → `--json-out evals/history/<commit>.json` + `leveler eval trend`。

## 本次验收执行方式（受限于环境）

- 真实 Provider：`deepseek/deepseek-v4-pro`（base_url taotoken.net，非 localhost）。
- 真实 agent 路径：`leveler eval quick|daily`（orchestrated，非内部 API mock）。
- 交互 TUI：现有 soak/e2e（PTY 层）。
- 基线：`evals/history/baseline-<commit>.json` → `ACCEPTANCE_BASELINE_REPORT.md`。
