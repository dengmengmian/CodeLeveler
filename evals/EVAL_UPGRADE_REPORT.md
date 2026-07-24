# Eval 升级报告

> 结论先行：本轮把现有 `leveler-eval` 从"能力对比工具"推进为"**Agent 质量门禁**"的地基：
> 补齐了以 `false_completion_rate` 为首的质量指标家族、落地了真实大仓场景机制（ripgrep 固定
> commit，已 RED 验证）、并把行为/工程指标持久化进 JSON baseline。**未新建平行 benchmark 系统**。

## 一、本轮新增能力（已落地 + 测试）

| 能力 | 位置 | 状态 |
|------|------|------|
| **false_completion_rate**（最高优先级：宣称完成但验收失败） | `EvalReport::false_completion_rate/_count/_case_ids` | ✅ 测试+打印+JSON |
| **completion_accuracy**（宣称完成里真完成的比例） | `EvalReport::completion_accuracy` | ✅ 测试+打印 |
| **Tool Efficiency**（avg tool calls / case） | `CaseResult::tool_calls` + `EvalReport::avg_tool_calls` | ✅ 持久化 |
| **Loop Rate**（触发无进展循环守卫的 case 比例） | `CaseResult::loop_guard_trips` + `EvalReport::loop_rate` | ✅ 持久化 |
| **Validation Rate**（跑过 build/test 的 case 比例） | `CaseResult::verification_ran` + `EvalReport::validation_rate` | ✅ 持久化 |
| **Scenario 系统** | `evals/scenarios/{feature,debugging,…}/` + `README.md` | ✅ 结构+约定 |
| **真实大仓机制** | `scripts/fetch_eval_repos.sh`（ripgrep@14.1.1）+ `feature/ripgrep-total-count.yaml` | ✅ RED 已验证 |
| 架构分析 | `evals/CURRENT_EVAL_ARCHITECTURE.md` | ✅ |

所有改动：`cargo test -p leveler-eval` 32 通过，`leveler-cli` 构建 + clippy 干净。

## 二、既有能力（复用，未重造）

- 验证驱动判定：`passed() = completed && expect_passed`，验收命令独立于模型自报。
- 三条执行路径：orchestrated / `--direct` / `--no-verify-gate`（ablation）。
- 失败首因归因：`classify_failure` 9 类；`TerminationClass` 7 类结束边界。
- 模型对比与旋钮消融：`Comparison`（model_gap/effort_gap + flip 列表 = **Regression Rate** 来源）、`Ablation`。
- 持久化 baseline：`--json-out` → `BaselineDocument` + `BaselineMeta`（git_sha/模型/mode/引擎版本/case 组成）。
- 长跑 checkpoint（append-only JSONL），中断可恢复。
- 时间/token：`latency_ms`（Time To Completion）、`input/output_tokens`（Token Efficiency）。
- TUI 测试两套：`tui_session_e2e.rs`（TestBackend 断屏）、`tui_path_soak.rs`（客户端路径，hang=硬失败）。

## 三、用户要求的 11 指标 —— 覆盖情况

| 指标 | 状态 | 说明 |
|------|------|------|
| Task Success Rate | ✅ | `completion_rate` |
| Completion Accuracy | ✅ 新 | `completion_accuracy` |
| False Completion Rate | ✅ 新 | 最高优先级，已单列打印 |
| Tool Efficiency | ✅ 新 | `avg_tool_calls`（已持久化进 JSON） |
| Loop Rate | ✅ 新 | `loop_rate` |
| Recovery Rate | ⬜ | 需 recovery 场景（compile/test/tool 失败后恢复），见下一步 |
| Validation Rate | ✅ 新 | `validation_rate` |
| Regression Rate | ✅ | `Comparison` 的 a_pass→b_fail flip 列表 |
| TUI Stability | 🟡 | soak 测试已存在，尚未纳入 `leveler eval` 报告 |
| Time To Completion | ✅ | `latency_ms` |
| Token Efficiency | ✅ | `input/output_tokens`（token/success 可派生） |

## 四、运行方式

```sh
# 拉真实仓（固定 commit，只需一次；不入库）
scripts/fetch_eval_repos.sh ripgrep

# 跑场景（换成你配置的模型）
leveler eval run --cases evals/scenarios/feature --model <provider/model> \
  --json-out evals/baselines/scenarios-feature.json

# 现有套件仍可用
leveler eval run --cases evals/smoke --model <provider/model>
leveler eval compare <model_a> <model_b> --cases evals/hard   # 含 Regression Rate
```

打印摘要现在含：completion / completion accuracy / **false completion** / avg tool calls / loop rate / validation rate / 失败首因分布 / 跨 repetition 不稳定 case。

## 五、后续进展（本文写于首轮，以下多项已在后续轮完成）

完整当前状态见 **`AGENT_EVAL_SYSTEM_DESIGN.md`**（本系统的权威设计与现状）。截至目前已追加：

- ✅ **三层门禁** `leveler eval quick|daily|release`（复用 `run_eval`，纯 CLI 接线）。
- ✅ **Agent Quality Score**（加权综合分，缺测分量归一化不捏造）。
- ✅ **趋势/回归** `leveler eval trend` → `REGRESSION_REPORT.md`（版本→分数表 + 回归标记）。
- ✅ **权限场景** `scenarios/permission/`（金丝雀密钥保护 + 正当任务，expect 已红/绿验证）。
- ✅ **TUI 稳定性门禁** 登记（`tui_path_soak` + `tui_session_e2e`，当前绿；见 `scenarios/tui/README.md`）。

仍待做（按价值）：

1. **Recovery 场景 + `recovery_rate`**：`scenarios/debugging/` 注入 compile/test/tool 失败起点。
2. **Tool/Cost Efficiency 归一** 接入 Quality Score 的 15%+5% 分量（需参考基线）。
3. **TUI Stability 折算** 成 Score 的 10% 分量（round 2，需从 soak 提取信号）。
4. tokio/starship/React：release 层再上，按需扩 `fetch_eval_repos.sh` 的 `REPOS`。

## 六、执行原则遵循（Phase 8）

本轮属"第一轮：只扩展 Eval，不改 Agent"。ripgrep 场景已做 **RED** 验证（干净 14.1.1 上
`--total-count` 不存在 → `expect` 失败），**GREEN**（真实模型实现该 flag → `expect` 通过）属"第二轮
运行 Eval / 第三轮修 Agent"，需配置 provider 后执行，不在本轮。
