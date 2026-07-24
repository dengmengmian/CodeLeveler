# Agent Evaluation & Quality Management System — 设计

> 结论先行：这套系统建立在现有 `crates/leveler-eval` + `evals/` + `leveler eval` 之上，**不是新系统**。
> 它把"跑一次评测"升级为"长期质量门禁"：一个代码改动能通过 `leveler eval quick` 立刻知道
> 能力是升是降；跨版本能看 Agent Quality Score 趋势与回归。指标全部**验证驱动**、可 JSON 输出、
> 缺测分量绝不捏造。

## 一、架构（复用现有，未另起 runner）

```
evals/**/*.yaml (EvaluationCase)                         # 声明式 case，含真实仓 repo+base_ref
        │  load_dir（递归）
        ▼
leveler eval quick|daily|release | run | compare | ablate | trend
        │  run_eval → 逐 case × repetition（crates/leveler-cli/src/eval_cmd.rs）
        ▼
run_eval_case: workspace(synthetic|git clone) → 三路径执行 → SignalCollector → expect 独立验收
        ▼
CaseResult{completed,expect_passed,rounds,tokens,latency,tool_calls,loop_guard_trips,verification_ran,…}
        ▼
EvalReport（聚合指标 + QualityScore）→ 打印 + BaselineDocument(JSON)
        ▼
evals/history/*.json ──► leveler eval trend ──► REGRESSION_REPORT.md（版本→分数趋势）
```

## 二、三层门禁（spec §2）

| 命令 | case 集 | 用途 | 目标耗时 |
|------|---------|------|---------|
| `leveler eval quick` | `evals/smoke` | 每次开发前快速验证核心 loop+工具+简单编辑 | <5 min |
| `leveler eval daily` | `evals/core` + `evals/hard` | 每日回归：debug/feature/refactor/multi-file | 中等 |
| `leveler eval release` | 全部 + `evals/scenarios`（真实仓） | 发版前完整能力 | 长（20min+） |

均为薄封装，复用同一 `run_eval`；退出码：全过=0，否则=1（可作 CI gate）。

## 三、Scenario 系统（spec §3）

`evals/scenarios/{feature,debugging,refactor,permission,tui,long_context}/`，与其他套件同 schema。
每个 case 五要素落在 `EvaluationCase` 上：**task**（自然语言，独立于实现）、**setup**（`repo`/`files`）、
**execute**（`max_rounds` 内 agent 运行）、**validator**（`expect` 独立命令，非仅 exit code）、
**metrics**（自动采集，见下）。真实仓由 `scripts/fetch_eval_repos.sh` 按固定 commit 拉取，不入库。

## 四、指标定义（spec §4）

| 指标 | 定义 | 实现 | 状态 |
|------|------|------|------|
| Task Success Rate | passed / total（passed=completed且expect过） | `completion_rate` | ✅ |
| Completion Accuracy | 真完成 / 宣称完成 | `completion_accuracy` | ✅ |
| **False Completion Rate** | 宣称完成但 expect 失败 / total（**最高优先级**） | `false_completion_rate` | ✅ |
| Tool Efficiency | 平均 tool calls / case | `avg_tool_calls`（`CaseResult.tool_calls`） | ✅ |
| Loop Rate | 触发无进展循环守卫的 case 占比 | `loop_rate`（`loop_guard_trips`） | ✅ |
| Validation Rate | 跑过 build/test 的 case 占比 | `validation_rate`（`verification_ran`） | ✅ |
| Regression Rate | 版本间 pass→fail 翻转 | `Comparison` flip 列表 / `TrendReport::regressions` | ✅ |
| Time To Completion | 端到端墙钟 | `CaseResult.latency_ms` | ✅ |
| Token Efficiency | token/task、token/success | `input/output_tokens` | ✅ |
| Recovery Rate | compile/test/tool 失败后恢复率（仅 recovery 场景） | `recovery_rate`（`CaseResult.is_recovery`） | ✅ |
| Tool Efficiency（0..1） | 未耗在循环上的工具调用占比 | `tool_efficiency` | ✅ |
| Cost Efficiency（0..1） | 花在通过 case 上的 token 占比 | `cost_efficiency` | ✅ |
| TUI Stability | crash/freeze/掉帧/输入延迟 | soak 测试已有（另一测试层），未折算进 Score | 🟡 待接 |

所有指标随 `--json-out` 落进 `BaselineDocument`，机器可读。

## 五、Agent Quality Score（spec §7）

单一 0..100 综合分，加权平均，**缺测分量按现有权重归一化，不捏造**（`QualityScore`）：

| 分量 | 权重 | 现状 |
|------|-----:|------|
| Task Success | 40% | ✅ 计入 |
| Completion Accuracy | 20% | ✅ 计入 |
| Tool Efficiency | 15% | ✅ 计入（用工具的 case 存在时） |
| Recovery | 10% | ✅ 计入（有 recovery 场景时） |
| TUI Stability | 10% | 🟡 待接（另一测试层，见 §九） |
| Cost Efficiency | 5% | ✅ 计入（有 token 记录时） |

分量按诚实性可选：无 token（未跑真实模型）→ efficiency 为 None 从分母剔除；无 recovery 场景 → recovery
为 None。**从不以假值凑满权重**。跑真实模型 + release 套件时，除 TUI 外 90% 权重可全部计入。

## 六、结果存储与趋势（spec §5/§6）

- 单次结果：`leveler eval <tier> --json-out evals/history/<version>.json`（`BaselineDocument`，含
  `quality_score` 各分量 + `quality_score_100` + meta：git_sha/引擎版本/模型/case 组成）。
- 趋势：`leveler eval trend --history evals/history --out evals/REGRESSION_REPORT.md` →

  ```
  | Version | Model | Score | Completion | False completion |
  | 0.1.0   | …     | 72    | 72%        | 10%              |
  | 0.2.0   | …     | 89    | 89%        | 0%               |
  | 0.3.0   | …     | 81    | 81%        | 20%              |
  ## Regressions
  - **0.2.0 → 0.3.0**: -8 points
  ```

  回归为软信号（退出 0），CI 自行定策略。`evals/history/`、`REGRESSION_REPORT.md` 已 gitignore。

## 七、CI 集成（spec §8，就绪待接）

```
commit  → leveler eval quick  → 指标/分数变化 → 判断升降 → 合并
release → leveler eval release → quality gate（退出码）
定期    → leveler eval trend  → 更新 REGRESSION_REPORT.md
```

## 八、扩展方式

- **加 case**：在对应套件/scenario 放 YAML；先自检红→绿（见 `evals/README.md`、`scenarios/README.md`）。
- **加真实仓**：在 `scripts/fetch_eval_repos.sh` 的 `REPOS` 追加 `name|url|ref`，写 `repo:` 场景。
- **加指标**：在 `CaseResult`（持久化）+ `EvalReport`（聚合）加字段/方法，同 `false_completion` 做法。
- **接分量进 Score**：给 `QualityScore` 对应 `Option` 分量赋 0..1 值即自动纳入权重。

## 九、未来规划（按价值）

已完成：Recovery 场景 + `recovery_rate`、Tool/Cost Efficiency 接入 Score、权限场景、TUI 门禁登记。剩余：

1. **TUI Stability 折算进 Score（10%）**：`tui_path_soak` 是独立 `cargo test`，不产出 `EvalReport`；
   需从 soak 提取"是否达终态 + 有无 hang"的 0..1 信号喂给 `QualityScore.tui_stability`。属 round 2
   （要么 eval 内驱动 TUI 客户端路径，要么读 soak 产物），当前诚实置 `None` 不伪造。
2. **真实模型跑通 GREEN**：ripgrep/权限/recovery 场景的"agent 实现→通过"，配置 provider 后执行。
3. **tokio/starship/React**：release 层扩 `scripts/fetch_eval_repos.sh` 的 `REPOS`。

## 十、边界与诚实性

- **验证驱动**：`passed = completed && expect_passed`，模型自报"完成"从不单独算通过。
- **不捏造**：未接入的分量/指标 = None，从分母剔除，绝不填 0 或 100 充数。
- **真实仓不入库**：`fixtures/` 整体 gitignored，固定 commit 保证可复现。
- **Phase 8 纪律**：先扩 Eval 不改 Agent；ripgrep 场景已 RED 验证，GREEN 需配置 provider 后跑真实模型。
