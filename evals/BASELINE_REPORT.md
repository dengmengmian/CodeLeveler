# CodeLeveler Acceptance Baseline Report

> 记录验收起点环境与已知信号。上一轮草稿见
> [`ACCEPTANCE_BASELINE_REPORT.md`](ACCEPTANCE_BASELINE_REPORT.md)（smoke 100% / daily 未完）。
> 本文件为计划要求的**正式文件名** `BASELINE_REPORT.md`。

## Environment

| 项 | 值 |
|----|----|
| 版本 | 0.1.2（workspace） |
| tip SHA（docs tip） | 以 `git rev-parse HEAD` 为准；文档提交后 tip 前进，见 **Live re-verify** |
| functional re-verify SHA | `9627123fbdb3daf1235684968a57283b143eb336` — 含 `node_status` 修复；`eval_quick.json` 的 `meta.git_sha` 与此对齐 |
| 简述（functional） | Fix orchestrate false-fails on green workspaces; add acceptance docs |
| 系统 | macOS Darwin 25.5.0 arm64 (Apple Silicon) |
| rustc | 1.90.0 |
| 模型 | `deepseek/deepseek-v4-pro` |
| Provider | deepseek，`base_url` = `https://taotoken.net/api/v1`（非 mock、非 localhost） |
| API key | `DEEPSEEK_API_KEY` 已配置 |
| 启动方式 | `./target/release/leveler` + `~/.cargo/bin/leveler`（release 覆盖 PATH）；TUI 经 `cargo test` 客户端路径 |
| 配置 | `~/.leveler/config.toml`：`default_model = "deepseek/deepseek-v4-pro"`；reasoning_effort=max |
| 禁项 | 未直调内部 Agent Loop API；验收 agent 质量不使用 mock 替代模型 |

## Launch surfaces used for acceptance

| 表面 | 命令 | 角色 |
|------|------|------|
| Eval quick | `leveler eval quick --model deepseek/deepseek-v4-pro` | 真实 orchestrated agent + expect |
| Eval daily | `leveler eval daily …` | 更宽 case（耗时长） |
| Regression | `leveler eval run --cases evals/regression` | 失败固化集 |
| TUI soak | `cargo test -p leveler-app --test tui_path_soak` | 客户端路径稳定性（hang=失败） |
| TUI e2e | `cargo test -p leveler-tui --test tui_session_e2e` | 会话/取消/UI 逻辑 |

## Prior measured smoke baseline（history）

来源：`evals/history/baseline-89f2bba.json`（同 commit）

| 指标 | 值 |
|------|----|
| tier | quick（3 smoke cases） |
| success | 3/3 (100%) |
| false completion | 0% |
| loop rate | 0% |
| verification_ran | 100% |
| Quality Score | 100/100 |
| 单例 latency | ~59–107s（主要在模型推理） |

## Prior daily partial（未完，作已知问题信号）

来源：`evals/history/daily-89f2bba.partial.jsonl`（截断）

| case | completed | expect_passed | termination | note |
|------|-----------|---------------|-------------|------|
| go-batch-boundaries | false | true | incomplete | Failed / runtime |
| go-context-worker | true | true | completed | Verified |
| go-copy-map | false | true | incomplete | Failed / runtime |
| go-json-defaults | true | true | completed | Verified |
| go-normalize-email | false | true | failed | Failed / runtime |

模式：多例 **expect 绿但 turn 未 completed** → 门禁失败，但不是 false completion。
指向「收口 / 完成判定 / runtime 中断」类问题，而非单纯改错代码。

## Known issues at baseline time

| 级别 | 问题 | 证据 |
|------|------|------|
| P1 | daily/quick 中 incomplete/failed 而 expect 已通过 | partial jsonl；本轮 r2 quick 0/3 |
| P2 | TTFF / SilentDuration 未进 EvalReport | 代码无字段；runtime 已有 CommandProgress |
| P2 | TUI Stability 未计入 QualityScore | 设计文档诚实 None |
| — | smoke 层历史 100% | baseline JSON |

## Post-fix snapshot（同环境，node_status 修复后）

`leveler eval quick` → **3/3**，false completion 0%，Quality **100**（见 `AGENT_ACCEPTANCE_REPORT.md`）。

## TUI stability at baseline write

| 测试 | 结果 |
|------|------|
| `tui_path_soak` | pass（≥80 轮，hang 硬失败） |
| `tui_session_e2e` | pass（3 tests） |

交互式全键盘（长输出复制、多轮历史）环境限制见验收报告；Cancel 语义：`AgentError::Cancelled` → `TurnCancelled`（非 Blocked）。

## Live re-verify

| 项 | 值 |
|----|----|
| re-verify time | pull/rebuild 后对 **functional** 提交再跑 quick + TUI |
| **tip SHA** | 文档 tip = 当前 `git rev-parse HEAD`（报告/Phase-0 刷新会前进；**不等于** agent 行为 SHA） |
| **functional re-verify SHA** | `9627123fbdb3daf1235684968a57283b143eb336`（`node_status` + regression 落地） |
| eval JSON `meta.git_sha` | 与 functional 对齐（本轮 `eval_quick.json` → `9627123…`） |
| quick 结果 | 3/3 Verified，Quality 100，false completion 0% |
| tip note | docs-only 提交（如 `d9ea0a6` 及后续）不改变 agent 逻辑；agent 质量以 functional SHA 的 re-verify 为准 |
| PATH binary | `~/.cargo/bin/leveler` (release overwrite) |
| release binary | `./target/release/leveler` |

