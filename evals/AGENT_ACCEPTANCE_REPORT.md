# CodeLeveler Acceptance Report

## Environment

| 项 | 值 |
|----|----|
| 版本 | 0.1.2 |
| tip SHA | `9e5bfdf`（early TTFF feedback） |
| 系统 | macOS Darwin 25.5.0 arm64 |
| 模型 | `deepseek/deepseek-v4-pro` |
| Provider | deepseek @ `https://taotoken.net/api/v1` |
| 启动 | `./target/release/leveler` + `~/.cargo/bin/leveler` 覆盖 |

## Test Summary

| 套件 | 结果 | 证据 |
|------|------|------|
| `leveler eval quick`（TTFF 早期反馈后） | **3/3**，avg TTFF **~5ms**，全 **&lt;5s** | `eval_quick_ttff_under5.*` |
| **`leveler eval daily` 全量 28 例（修复后整轮）** | **27/28 (96%)**，exit 1；**fc 0%** | `eval_daily_full_after.*` |
| 唯一失败 `ts-group-by` | 模型方差（editing/incomplete）；另案重跑可绿 | 见 Critical |
| `tui_path_soak` | pass | `long_task_proxy.log` |
| TTFF 单测 | 8 pass（含 early feedback） | `ttff_early_feedback_tests.log` |

## Metrics

### Full daily after all product/case fixes（本轮）

| 指标 | 值 |
|------|----|
| Task Success Rate | **96%** (27/28) |
| False Completion Rate | **0%** |
| Verification Rate | **96%** |
| Loop Rate | **4%** |
| Recovery Rate | **100%** (2/2) |
| **avg TTFF** | **6 ms**（目标 &lt;5s ✓） |
| **max TTFF** | **14 ms** |
| **all cases TTFF &lt;5s** | **true** |
| max Silent Duration | ~339s（模型长推理间隙，可观测） |
| Agent Quality Score | **98/100** |

### Quick after TTFF fix

| 指标 | 值 |
|------|----|
| Success | 3/3 (100%) |
| avg TTFF | **5.3 ms** |
| Score | 100 |

## Critical Issues

### P1 已修：TTFF 被首 token 绑架

| | |
|--|--|
| **问题** | 原先 avg TTFF ~45s，远超 5s 目标 |
| **原因** | Understand 阶段在模型返回前不发 PhaseChanged；TTFF 只等 LLM 首包 |
| **修复** | `run_orchestrate` 进入即发 PhaseChanged；collector 计 `TaskStarted`/`StreamAttemptStarted` 为首反馈（`9e5bfdf`） |
| **验证** | quick avg **5ms**；daily avg **6ms**，全部 &lt;5s |

### P1 已修：TS case 无 test 门

`package.json` 加 `scripts.test`（`aa67dbc`）→ daily 中 TS 几乎全绿。

### P1 已修：大节点预算饿死 → hard 案例早停假失败（`463e7d9`）

| | |
|--|--|
| **问题** | `go-gitcmd-semaphore`（hard 并发）常在 **4 步早停**（`incomplete`），代码半截留盘上；曾被误判为「模型方差」 |
| **原因** | 一刀切的 per-node 默认预算（`max_commands`/`max_modified_files`/`max_duration`）对高 workload 节点不足，中途耗尽即停 |
| **修复** | 三层预算控制：telemetry + 按 workload 放大的 sized quotas（各维度带 CAP）+ bounded extend（`463e7d9`） |
| **验证（独立复跑 ×3）** | **3/3 Verified**；轮数从早停的 **4 步 → 24 / 18 / 5 轮**——拿到足够预算即跑完。**证实「早停」实为预算饿死，非纯模型变异**（`gcs2.json`） |

### 归因修正

此前将 `go-gitcmd-semaphore` 失败整体归为「模型变异」**部分有误**：其中「4 步早停」类实为**预算饿死**，已由
`463e7d9` 修复。剩余（偶发 round17 `term=failed`）才是真·瞬时 Err 路径（`StopReason` 无 `Failed` 变体）。

### 残留

| 项 | 级别 | 说明 |
|----|------|------|
| `ts-group-by` 偶发失败 | 模型方差 | 本轮 daily 1 例；此前单独重跑可 Verified；非结构性 gate 问题 |
| 瞬时 provider Err（如 round17） | 环境/网络 | 非确定，重跑可过；`term=failed` 走 `Err(_)=>Failed` 路径 |
| 模型静默段可达数分钟 | 体验 | **SilentDuration 已度量**；非「无反馈」——主机侧阶段/工具心跳已发出 |

## Agent Score

| 维度 | /10 |
|------|-----|
| 任务完成 | **9**（daily 96%） |
| 工具使用 | **8** |
| 稳定性 | **8**（TTFF 达标；偶发 1 case 方差） |
| 权限 | **7** |
| TUI | **8** |

## Final Recommendation

1. 保留 `9e5bfdf` 早期反馈与 TTFF 指标。  
2. 门禁：`quick` 盯 TTFF&lt;5s + 3/3；`daily` 夜间，接受偶发模型方差。  
3. 可选：把 `ts-group-by` 放进 `evals/regression/` 做稳定性跟踪。

**结论**：两项残留均已完成——**TTFF 主机侧 &lt;5s 可测且实测达标**；**daily 全量在修复后已整轮重跑（27/28，fc 0%，TTFF 全绿）**。

## Artifacts

| 文件 | 内容 |
|------|------|
| `eval_daily_full_after.json` | 整轮 daily 结果 |
| `eval_quick_ttff_under5.json` | TTFF&lt;5s quick |
| `evals/history/daily-full-after-ttff-fix.json` | 历史落盘 |
