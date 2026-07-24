# CodeLeveler Acceptance Report

## Environment

| 项 | 值 |
|----|----|
| 版本 | 0.1.2 |
| git commit（基线） | `89f2bba2e7d5a90682f9c51f8ef92ebabcfd879d` |
| 本轮代码改动 | `leveler-engine`：`node_status` / Incomplete-with-mutation 收口（未单独打 tag） |
| 系统 | macOS Darwin 25.5.0 arm64 |
| 模型 | `deepseek/deepseek-v4-pro` |
| Provider | deepseek @ `https://taotoken.net/api/v1`（真实 API，非 mock） |
| 启动方式 | `./target/debug/leveler eval quick` / `cargo test`（TUI 客户端路径） |
| 配置 | `~/.leveler/config.toml` default_model 同上 |

## Test Summary

| 套件 / 路径 | 结果 | 证据 |
|-------------|------|------|
| `leveler eval quick`（修后） | **3/3 通过**，exit 0，Quality **100** | scratch `eval_quick_after_fix.json` / `eval_quick.log` |
| `leveler eval quick`（修前 r2） | **0/3**，全部 expect 绿但 completed 假失败 | `eval_quick_r2.json` |
| `leveler eval quick`（修前 r1） | 1/3 | `eval_quick.json`（首轮） |
| `leveler eval quick`（历史同 commit） | 3/3（baseline-89f2bba） | `evals/history/baseline-89f2bba.json` |
| `tui_path_soak` | pass | `tui_stability.log` |
| `tui_session_e2e` | pass（3 tests） | `tui_stability.log` |
| `cargo test -p leveler-eval` | 41 pass | `leveler_eval_tests.log` |
| `node_status` 单测 | 4 pass | `fix_node_status.log` |
| Regression 入口 | `evals/regression/` + `leveler eval run --cases evals/regression` | `evals/regression/README.md` |
| daily 全量 | 未完整跑完（耗时）；partial 见历史 | `daily-89f2bba.partial.jsonl` |

测试数量（本轮可复现门禁）：**quick 3 + TUI 4 + eval 库测 41**（agent 质量以 quick 实跑为准）。

成功（修后 quick）：**3**  
失败（修后 quick）：**0**

## Metrics

（修后 `leveler eval quick`，`deepseek/deepseek-v4-pro`）

| 指标 | 值 |
|------|----|
| Task Success Rate | **100%** (3/3) |
| False Completion Rate | **0%** |
| Verification Rate | **100%** |
| Loop Rate | **0%** |
| Recovery Rate | n/a（smoke 无 recovery 标记 case） |
| Tool Efficiency | 1.0（avg ~10 tool calls，无 loop 浪费） |
| TTFF / SilentDuration | **未入 EvalReport**（runtime 已有 `CommandProgress`；见 gaps） |
| TUI Stability | soak+e2e **绿**；未折算进 QualityScore |
| Agent Quality Score | **100/100**（measured components） |

修前 r2 对照：success 0%、false completion 0%、**false-incomplete 100%**（expect 全绿但 `TaskOutcome::Failed`）。

## Critical Issues

### P1（已修）：编排图节点误杀导致「代码已绿、任务报 Failed」

| | |
|--|--|
| **问题** | quick/daily 多次出现 `expect_passed=true` 且 `completed=false`（note=`Failed`）。用户工作区已通过独立 expect，但 Agent 报告失败。 |
| **原因** | 1) 后续 **Edit 节点 Answered 且本节点无改动** 时 K15 整图失败，即使先前节点已改文件；2) **`CloseoutForced` 未当成功节点**，与 Direct 可进 verify 不一致；3) **`Incomplete` 有 mutation 时直接失败**，不进 verify。 |
| **修复** | `crates/leveler-engine/src/engine.rs`：`node_status(..., task_has_mutation)`；CloseoutForced→Completed；Incomplete+task mutation→Completed 后走 verify；Direct 的 Incomplete+files 同样进入 verify。 |
| **验证** | `cargo test -p leveler-engine node_status` 4/4；重建 CLI 后 `leveler eval quick` **3/3 Verified**；`cargo test -p leveler-eval` 41/41。 |

### P2（残留）：TTFF / SilentDuration 未度量

| | |
|--|--|
| **问题** | 无法在 eval JSON 中报告首反馈时间与静默时长。 |
| **原因** | 指标未接入 `CaseResult`/`EvalReport`（runtime 已有心跳事件）。 |
| **修复** | 未做（非 P0；见 `CURRENT_ACCEPTANCE_ANALYSIS.md`）。 |
| **验证** | 诚实记入 gaps。 |

### P2（残留）：TUI Stability 未计入 QualityScore

| | |
|--|--|
| **问题** | Score 缺 10% 分量。 |
| **原因** | soak 为独立 `cargo test`，不产 `EvalReport`。 |
| **修复** | 未做；测试本身绿。 |

### P2（残留）：模型方差与 daily 未全量

| | |
|--|--|
| **问题** | 同 commit 历史 100% 与修前 0% 并存，受模型路径影响；daily 28 例未本轮全跑。 |
| **处理** | 固化 `evals/regression/`；门禁以 quick 修后绿 + 结构测试为准。 |

## Agent Score

| 维度 | 分 /10 | 依据 |
|------|--------|------|
| 任务完成 | **9** | 修后 smoke 100%；修前假失败暴露收口缺陷 |
| 工具使用 | **8** | loop 0、avg ~10 calls；无大规模空转 |
| 稳定性 | **7** | TUI soak 绿；模型路径仍有方差；长任务未 20min 实测 |
| 权限 | **7** | 执行层+permission scenario 在位；本轮未 live 跑 release 权限例 |
| TUI | **8** | soak/e2e 绿；Cancel→Cancelled 有协议映射；交互全键盘未 PTY 全覆盖 |

## Final Recommendation

1. **合并本轮 `node_status` 修复**：消除 expect 绿却 Failed 的假失败，是日常可用的关键门槛。  
2. **门禁**：每次改 Agent/Engine 跑 `leveler eval quick` + `cargo test -p leveler-app --test tui_path_soak` + `leveler-tui tui_session_e2e`；失败 case 进 `evals/regression/` 用 `leveler eval run --cases evals/regression` 重放。  
3. **daily/release**：有 provider 预算时跑 `leveler eval daily` / `release`（先 `scripts/fetch_eval_repos.sh`）。  
4. **下一步指标**：把 TTFF 从 `CommandProgress`/首事件时间写入 `CaseResult`；可选把 soak 结果喂给 QualityScore.tui_stability。  

**结论**：在真实用户路径（`leveler eval` orchestrated + TUI 客户端路径测试）上，修后 quick 门禁已恢复 **100% / 假完成 0 / 循环 0**，P0 级「绿代码报失败」已闭合。CodeLeveler 达到可日常使用的 satisficing 条：能完成 smoke 级真实开发任务、不假完成、不空转、权限与 TUI 基础稳定；残留 P2 已列明。

## Artifacts

| 路径 | 内容 |
|------|------|
| `evals/CURRENT_ACCEPTANCE_ANALYSIS.md` | Phase 0 |
| `evals/BASELINE_REPORT.md` | Phase 1 |
| `evals/regression/` | 回归入口 |
| `evals/history/quick-after-node-status-fix.json` | 修后 quick JSON |
| scratch `eval_quick*.log/json`, `tui_stability.log`, `fix_*` | 本机证据 |
