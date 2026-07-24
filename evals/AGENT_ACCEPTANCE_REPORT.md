# CodeLeveler Acceptance Report

## Environment

| 项 | 值 |
|----|----|
| 版本 | 0.1.2 |
| functional re-verify SHA | `9627123fbdb3daf1235684968a57283b143eb336`（`eval_quick` meta.git_sha） |
| tip SHA | 见 `git rev-parse HEAD`（文档刷新可能新于 functional） |
| 简述 | Fix orchestrate false-fails…；Phase-0/报告文档与 live tree 对齐 |
| 系统 | macOS Darwin 25.5.0 arm64 |
| 模型 | `deepseek/deepseek-v4-pro` |
| Provider | deepseek @ `https://taotoken.net/api/v1`（真实 API，非 mock） |
| 启动方式 | `./target/release/leveler eval quick`；PATH `~/.cargo/bin/leveler` 已 release 覆盖 |
| 配置 | `~/.leveler/config.toml` default_model 同上 |

## Test Summary

| 套件 / 路径 | 结果 | 证据 |
|-------------|------|------|
| `leveler eval quick`（**本轮 re-verify**） | **3/3**，exit 0，Quality **100** | scratch `eval_quick.json` / `eval_quick.log` |
| `tui_path_soak` | pass（≥80 轮） | `tui_stability.log` |
| `tui_session_e2e` | pass（3 tests） | `tui_stability.log` |
| `cargo test -p leveler-eval` | 41 pass | `leveler_eval_tests.log` / `tui_stability.log` |
| `node_status` 单测 | 4 pass | 同上 |
| Regression 入口 | `evals/regression/` + `leveler eval run --cases evals/regression` | `evals/regression/README.md` |
| 平行 harness | **无** | `no_parallel_harness.txt` |
| daily 全量 | 未本轮全跑；partial 见历史 | `daily-89f2bba.partial.jsonl` |

测试数量（本轮门禁）：**quick 3 + TUI 4 + eval 库测 41 + node_status 4**。

成功（quick）：**3**  
失败（quick）：**0**

## Metrics

（本轮 `./target/release/leveler eval quick`，`deepseek/deepseek-v4-pro`）

| 指标 | 值 |
|------|----|
| Task Success Rate | **100%** (3/3) |
| False Completion Rate | **0%** |
| Verification Rate | **100%** |
| Loop Rate | **0%** |
| Recovery Rate | n/a（smoke 无 recovery 标记；`evals/regression` 含 `reg-recovery-compile-fail`） |
| Tool Efficiency | ~1.0（avg **8.3** tool calls，无 loop 浪费） |
| TTFF / SilentDuration | **未入 EvalReport**（runtime 有 `CommandProgress`；不编造 0） |
| TUI Stability | soak+e2e **绿**；未折算进 QualityScore |
| Agent Quality Score | **100/100**（measured components） |

历史对照：修复前同环境曾出现 expect 全绿但 `TaskOutcome::Failed`（false-incomplete）；`node_status` 修复后两轮 quick 均 3/3。

## Critical Issues

### P1（已修并 re-verify）：编排图节点误杀 → 绿代码报 Failed

| | |
|--|--|
| **问题** | `expect_passed=true` 且 `completed=false`（note=`Failed`） |
| **原因** | K15 后续 Edit 无改动整图失败；`CloseoutForced`/`Incomplete`+mutation 未进 verify |
| **修复** | `crates/leveler-engine/src/engine.rs`（commit `9627123`） |
| **验证** | 本轮 release binary quick **3/3 Verified**；`node_status` 4/4 |

### P2（残留）：TTFF / SilentDuration 未度量

未接入 `CaseResult`；诚实记 gaps。

### P2（残留）：TUI Stability 未计入 QualityScore

soak 独立 `cargo test`；测试本身绿。

### P2（残留）：daily 未全量

门禁以 quick + regression 入口 + 结构测试为准。

## Agent Score

| 维度 | 分 /10 | 依据 |
|------|--------|------|
| 任务完成 | **9** | re-verify smoke 100% |
| 工具使用 | **8** | loop 0、avg 8.3 calls |
| 稳定性 | **7** | TUI soak 绿；模型方差仍在；20min+ 未本轮强制 |
| 权限 | **7** | 执行层+permission scenario 在位；未 live release 权限全跑 |
| TUI | **8** | soak/e2e 绿；Cancelled≠Blocked 有协议映射 |

## Final Recommendation

1. 保持 `9627123` 的 `node_status` 修复；任何 Agent/Engine 改动先跑 `leveler eval quick`。  
2. 门禁：`leveler eval quick` + `tui_path_soak` + `tui_session_e2e`；失败进 `evals/regression/`。  
3. 有预算时跑 `leveler eval daily` / `release`。  
4. 下一步：TTFF 从首事件/`CommandProgress` 写入 `CaseResult`（可选）。  

**结论**：真实用户路径（release `leveler eval` orchestrated + TUI 客户端路径）上，当前 **quick 100% / 假完成 0 / 循环 0**，P0/P1 假失败已闭合且本轮 re-verify 通过。残留 P2 已列明。

## Phase 7 回归 — core 假阴性案例（决定性验证）

> 缺口修补：上文 quick(smoke) 在**修复前也能 3/3**，并不触发该 bug。真正触发 63% 假阴性的是
> **core 案例**（go-copy-map / go-batch-boundaries / go-normalize-email / rust-dedup-stable）。
> 用**含 `9627123` 修复的重编二进制**对这 4 例各跑 3 次，与修复前对比：

| case | 修复前 假阴性 | 修复后 假阴性 |
|------|--------------|--------------|
| go-batch-boundaries | 1/3 | **0/3** |
| go-copy-map | 2/3 | **0/3** |
| go-normalize-email | 2/3 | **0/3** |
| rust-dedup-stable | 2/2 | **0/3** |
| **合计** | **7/11 = 63%** | **0/12 = 0%** |

修复后：**11/12 完成（92%）、false-negative 0%、completion accuracy 100%、Quality Score 96/100**。
唯一 1 例失败（rust-dedup#2）是**真实**提前放弃（4 步 / planning / expect_passed=False，代码未解出），
非假阴性——属模型变异（同例 #1#3 通过），非确定性 bug。

**结论**：`9627123` 的 `incomplete_with_work` / node_status 修复**在真实失败案例上把假阴性从 63% 降到 0%**，
经真实模型 12 次重复验证。存档 `evals/history/regression-falseneg-9627123.json`。

## Artifacts

| 路径 | 内容 |
|------|------|
| `evals/history/regression-falseneg-9627123.json` | Phase 7 core 回归（修后 11/12，假阴性 0%） |
| `evals/CURRENT_ACCEPTANCE_ANALYSIS.md` | Phase 0 |
| `evals/BASELINE_REPORT.md` | Phase 1（HEAD 刷新） |
| `evals/regression/` | 回归入口 |
| scratch `eval_quick.*`, `tui_stability.log`, `acceptance_docs.list`, `no_parallel_harness.txt` | 本轮证据 |
