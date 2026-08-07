# 阶段 5：产品启发式删除决策（eval ablation）

> 结论先行：**不删除任何产品启发式代码**。全部保留为可开关的
> `TurnPolicy` / work-profile 策略；**当前默认保持不变**。
> 本文件是 `core-runtime-convergence-plan.md` 阶段 5 工作项
> 「用真实复杂任务数据决定是否删除旧策略」的验收证据。

## 方法

- 命令：`leveler eval ablate <KNOB> --model deepseek/deepseek-v4-flash --cases <DIR>`
- 单旋钮：control = 生产默认，ablated = 关掉该旋钮；其余完全相同。
- 模型：`deepseek/deepseek-v4-flash`（真实 API，非 mock）。
- 代码侧先修了 ablation 接线，否则对照是假的：
  - `explicit_plan` 原先 control/ablated 都是 ON（无差异）；现 ablated = OFF。
  - `repeated_read_guard` / `progress_guards` 现同时关掉 tools 层读守卫与
    agent 层 `progress_guards`（loop guard + observe/closeout thrash）。
  - `completion_evidence` 现通过 `with_delivery_gate(false)` 只能**压低**
    Delivery 证据门，不能抬高 work profile 默认。

本地 JSON（git 忽略，可复跑）：

| 文件 | 旋钮 | suite |
| --- | --- | --- |
| `evals/baselines/ablate-progress_guards-hard.json` | progress_guards | hard (5) |
| `evals/baselines/ablate-progress_guards-core.json` | progress_guards | core (21) |
| `evals/baselines/ablate-explicit_plan-hard.json` | explicit_plan | hard (5) |
| `evals/baselines/ablate-completion_evidence-hard.json` | completion_evidence | hard (5) |

运行日：2026-08-07；基线 tip（接线提交前）：`775f89d`。

## 结果

| 旋钮 | suite | control | ablated | rate Δ | rounds control → ablated |
| --- | --- | --- | --- | --- | --- |
| progress_guards | hard | 100% (5/5) | 100% (5/5) | +0.0pp | 5.6 → 6.0 (+0.4) |
| progress_guards | core | 100% (21/21) | 100% (21/21) | +0.0pp | 5.9 → 6.1 (+0.2) |
| explicit_plan | hard | 100% (5/5) | 100% (5/5) | +0.0pp | 5.6 → 5.8 (+0.2) |
| completion_evidence | hard | 100% (5/5) | 100% (5/5) | +0.0pp | 5.6 → 5.8 (+0.2) |

- **saved by knob / hurt by knob：全部为空**（没有任何 case 因旋钮翻转而通→挂或挂→通）。
- loop rate 在本批 run 中均为 0%——这些 case 本身很少触发「同参同果」拒绝，
  因此 progress_guards 的通过率差异被**天花板效应**压扁；轮次上 control
  略优，符合「守卫在时少做一点空转」的弱信号。

## 决策表

| 机制 | 默认 | 决策 | 理由 |
| --- | --- | --- | --- |
| plan-before-write（`require_explicit_plan` / resolve `explicit_plan`） | 解析默认 ON，复杂任务才真正门禁 | **保留代码 + 保持默认** | hard 关掉不掉通过率，也没有证据说明它在拖累；多步任务仍需要可选 plan 门，删代码会拆掉 `TurnPolicy` 面 |
| 连续搜索预算（`max_search_calls_per_step`） | 0 = 关 | **保留代码 + 保持默认关** | 默认已是「不生效」；无需删除，profile/实验可再开 |
| progress_guards（loop guard + round verdict thrash） | ON | **保留代码 + 保持默认 ON** | core 21 + hard 5 关掉不掉通过率；关掉略增均轮次； thrash 防护在长任务/真实仓库上仍有产品价值，且 `minimal()` 已可关 |
| tools 层 repeated-read guard | ON | **同上**（与 progress_guards 共 seam） | 同 ablation |
| Delivery 证据门（`delivery_gate` / ablate `completion_evidence`） | Balanced 关；Delivery 开 | **保留代码 + 保持 work-profile 默认** | Balanced eval 路径上关 delivery 是 no-op（本来就关）；Delivery 模式仍依赖该门，无数据支持整段删除 |
| goal todo 门（`goal_todo_gate`） | goal 模式 ON | **保留代码 + 保持默认** | 本批 eval 走 chat/direct，不进 goal 终止路径；删了会拆 goal complete 语义，且无对照证据 |

## 明确不做的事

1. **不**因「本批 100% 无差异」删除门禁实现——计划禁止「先删再看回归」，
   且天花板效应不能证明「永远无用」。
2. **不**把默认改成 `TurnPolicy::minimal()`——默认行为保持阶段 0 能力基线。
3. **不**把 goal/Delivery 专用门因 Balanced 路径无差异而删掉。

## 局限（诚实写清）

- 模型只测了 `deepseek-v4-flash`；更强/更弱模型可能翻转结果。
- case 是合成小仓单点修复，不是多文件长 horizon 产品任务；plan 门与 thrash
  守卫在这类 case 上本来就不常触发。
- `completion_evidence` 在 Balanced 上几乎测不到（delivery 默认关）；Delivery
  profile 的专用对照仍可后续补 `leveler eval ablate completion_evidence`
  并强制 Delivery work profile。
- 未做 repetitions≥3 方差估计；单次 100% 对照不能排除噪声。

## 复跑

```sh
leveler eval ablate progress_guards \
  --model deepseek/deepseek-v4-flash \
  --cases evals/core \
  --json-out evals/baselines/ablate-progress_guards-core.json

leveler eval ablate explicit_plan \
  --model deepseek/deepseek-v4-flash \
  --cases evals/hard \
  --json-out evals/baselines/ablate-explicit_plan-hard.json
```
