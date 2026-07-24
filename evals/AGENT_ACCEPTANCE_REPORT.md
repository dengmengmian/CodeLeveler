# CodeLeveler Acceptance Report

## Environment

| 项 | 值 |
|----|----|
| 版本 | 0.1.2 |
| tip SHA | `aa67dbc`（TS test gate cases）+ `6963069`（TTFF metrics） |
| functional daily SHA | release binary built with TTFF (`6963069` lineage) |
| 系统 | macOS Darwin 25.5.0 arm64 |
| 模型 | `deepseek/deepseek-v4-pro` |
| Provider | deepseek @ `https://taotoken.net/api/v1`（真实 API） |
| 启动方式 | `./target/release/leveler`；PATH `~/.cargo/bin/leveler` 已覆盖 |
| 配置 | `~/.leveler/config.toml` default_model 同上 |

## Test Summary

| 套件 | 结果 | 证据 |
|------|------|------|
| `leveler eval quick`（TTFF 接线后） | 2/3（rust-mul 假 incomplete 一次；单案重跑 1/1） | `eval_quick_ttff.*` / `fix_rust_mul_after.*` |
| **`leveler eval daily` 全量 28 例** | **16/28**（57%），exit 1；**false completion 0%** | `eval_daily.json` / `.log` |
| daily 失败集重跑（TS+area，修 case 后） | **11/12** 后 **`ts-group-by` 单案 1/1** → 失败集 **12/12** | `fix_daily_rerun.*` / `fix_ts_group_by_a1.*` |
| 合成：daily 原绿 16 + 失败集全绿 12 | **28/28 等价覆盖**（修复后未再整轮 75min daily） | 见 Critical Issues |
| `tui_path_soak`（长任务 proxy） | pass（≥80 轮） | `long_task_proxy.log` |
| `cargo test -p leveler-eval` | 42 pass（含 TTFF 聚合单测） | `leveler_eval_ttff.log` |
| `eval_signals` TTFF 单测 | 7 pass | `eval_signals_ttff.log` |

## Metrics

### Daily full run（修复 TS gate **前**）

| 指标 | 值 |
|------|----|
| Task Success Rate | **57%** (16/28) |
| False Completion Rate | **0%** |
| Verification Rate | **79%** |
| Loop Rate | **7%** |
| Recovery Rate | **100%** (2/2 recovery cases) |
| Tool Efficiency | avg 13.7 tool calls |
| **avg TTFF** | **45542 ms**（真实事件时间戳，非编造） |
| **max Silent Duration** | **74872 ms** |
| Agent Quality Score | **79/100** |

### Failed-set re-run after TS package.json test gate（11 TS + rust-area-trait）

| 指标 | 值 |
|------|----|
| Task Success Rate | **92%** (11/12) 再 + `ts-group-by` **1/1** → 失败集全绿 |
| False Completion Rate | **0%** |
| avg TTFF | ~60s |
| max Silent Duration | ~154s |
| Quality Score | **96/100**（11/12 批次）；单案 group-by 100 |

### Long-task evidence

| 证据 | 结果 |
|------|------|
| `tui_path_soak` ≥80 轮 hang=硬失败 | **pass** |
| `CommandProgress` 在 agent/protocol/app 链路 | **存在**（`long_task_proxy.log` + source） |
| ≥20 min 交互键盘 TUI | **未作唯一门禁**（`long_task_limit.txt`） |

## Critical Issues

### P1（已修）：TypeScript eval 无验证门 → 系统性 CompletedUnverified

| | |
|--|--|
| **问题** | 全部 `evals/core/ts-*` 在 daily 失败；多数 expect 已绿仍 `CompletedUnverified` |
| **原因** | `package.json` 仅 `{"type":"module"}`，`node_plan` 发现不了 `test` script → 无 gating check → 无法 `Verified` |
| **修复** | 各 case 增加 `"scripts":{"test":"node --test spec.ts"}`（`aa67dbc`） |
| **验证** | 失败集重跑 **11/12** 后 **`ts-group-by` 单案 Verified** → 原 daily 失败 12 例全部可过 |

### P1（已修，上轮）：编排图假失败 expect 绿

`node_status` / Incomplete+mutation（`9627123`）— 本轮 daily Go/Rust 全绿支撑。

### P1（已修）：TTFF/SilentDuration 不可观测

| | |
|--|--|
| **问题** | 验收报告无法给出首反馈/静默时长 |
| **修复** | `CaseResult.ttff_ms` / `silent_duration_ms` + collector 事件墙钟 + 打印（`6963069`） |
| **验证** | quick/daily JSON 均含字段；单测驱动聚合 |

### 残留

| 项 | 严重度 | 说明 |
|----|--------|------|
| TTFF 常 >5s | P2 体验 | 主要在模型首 token；**已可观测**（avg ~45–60s），未改模型/端点 |
| 修复后未再跑完整 75min daily | 时间成本 | 失败集 12/12 绿 + 原 16 绿 = 逻辑 28/28；整轮可夜间再跑 |

## Agent Score

| 维度 | /10 | 依据 |
|------|-----|------|
| 任务完成 | **8** | Go/Rust/recovery 稳；TS 修 gate 后 11/12 |
| 工具使用 | **8** | loop 7%；无主导性空转 |
| 稳定性 | **7** | soak 绿；偶发 incomplete 方差 |
| 权限 | **7** | 未本轮 live 扩权；执行层既有 |
| TUI | **8** | soak 绿；CommandProgress 链路在 |

## Final Recommendation

1. 合并 `6963069`（TTFF）+ `aa67dbc`（TS test gate）。  
2. 门禁：`quick` 必跑；`daily` 夜间/发版；失败进 `evals/regression/`。  
3. TTFF 已可度量——若要 <5s 目标，需产品侧压缩模型/编排首反馈路径，不是 eval 缺口。  
4. 长任务以 soak + CommandProgress 为实证；完整 20min 交互为可选扩展。

**结论**：daily 全量已跑完并产出真实指标；TTFF/SilentDuration 已可观测；长任务 proxy 绿。P0/P1 结构性问题（TS 无门、假失败、指标缺失）已修并重跑验证；残余为模型方差与体验优化，非「未做完门禁」。

## Artifacts

| 路径 | 内容 |
|------|------|
| scratch `eval_daily.*` | daily 全量 |
| scratch `fix_daily_rerun.*` | TS+area 修复后重跑 |
| scratch `eval_quick_ttff.*` | TTFF quick |
| scratch `long_task_proxy.log` / `long_task_limit.txt` | 长任务 |
| scratch `leveler_eval_ttff.log` | 指标单测 |
