# Pre-Beta Eval Plan — Comparative Eval / Real Usage

**目的**：为 Beta Readiness Gate 收集最后一轮核心证据。回答的不是"谁分高"，而是：
CodeLeveler 是否可靠完成真实工程任务、是否保真保安全、与其他 Harness 的真实差距在哪、
Harness 质量能否收窄模型差距（Harness thesis）。

本轮**只测量不改造**：不动运行时架构、不做 Continuable SubAgent、不为评测调 prompt/policy、
不发布。测量的是 shipping Harness。

## 1. 冻结基线

| 对象 | 版本 | 说明 |
|---|---|---|
| CodeLeveler | **Eval 冻结 SHA = `3b400357342cef4caa760628531ead3bd9eff333`**（含 roster `3269354f` + 诚实收场 `2b968534`/`3b400357`）。HC-001 用干净二进制 `leveler 0.2.0-beta.1 (3b400357342c)`，不跟随之后的 main。icg-6r 探针尚未在此 SHA 上重验收。 | 见 `docs/evaluations/HC-001-FREEZE.md` |
| AtomCode | **5.0.9 (52ca5e6)**，发布渠道 `atomcode upgrade` 就地升级（2026-08-28；旧 5.0.8 存 `.bak`） | 已装二进制 + **真实用户配置 `~/.atomcode`**（用户指示）；无头：`-p -C -y -v --dev --no-telemetry` |
| DeepSeek Harness | **0.1.2-alpha.1 (`cd5ef8148`)**，master 最新，本地 `pnpm install && pnpm run build` | 无发布二进制，源码运行；headless profile + `--patch` 指 taotoken；`TSX_TSCONFIG_PATH` 垫片 |

三家升级后车道均已单 case 重验（AtomCode 5.0.9: go-normalize-email PASS 35.7s；DSH alpha.1: ts-concurrency-limit PASS 34.9s）。

批次执行期间基线冻结（§47）：发现缺陷 → 停批、另行修复、换新基线重跑受影响批次。

## 2. 阶段

```
Phase A  CodeLeveler Internal Baseline（先跑，有回归就停，不花对照的钱）
Phase B  同模型跨 Harness 对照（10 case × 2 rep × 2-3 Harness）+ Harness thesis 实验
Phase C  Real Usage（3-5 个真实工程任务，非评测夹具）
```

## 3. 模型矩阵（不爆炸）

| 档位 | 模型 | 用途 |
|---|---|---|
| 主力 | `deepseek/deepseek-v4-flash` | 全部对照主车道（三家同 wire 模型、同 taotoken 网关、同 key） |
| 强档 | `deepseek/deepseek-v4-pro` | Harness thesis 的"强模型"侧 |
| 弱档 | `deepseek/glm-5.2` | Harness thesis 的"弱模型"侧（当初就是为此配置的 L1 弱档） |

**Harness thesis 实验（§8）**：同一 10-case 套件上对比
`AtomCode + v4-pro`（强模型 × 别家 Harness）vs `CodeLeveler + glm-5.2`（弱模型 × CL Harness），
辅以 `CL + pro`、`CL + flash`、`AtomCode + flash` 三个支撑臂定位差距来源。
已知混淆：glm-5.2 窗口 200K（其余 1M），在大上下文 case 上是能力混淆而非纯 Harness 变量——按 case 记录。

## 4. 对照 case 套件（§15 类别映射）

复用现有 `EvaluationCase` YAML（无第二 manifest 格式）；全部 `expect` 自包含、可在工作区外部执行 =
跨 Harness 统一判分器。注释性元数据见 `eval/comparative/manifest.yaml`。

| # | 类别 | case | 来源 | 判分 | 超时 | 委派机会 |
|---|---|---|---|---|---|---|
| 1 | A 小确定修复 | rust-first-even | evals/smoke | cargo test（基线红） | 15m | NONE |
| 2 | B 中等仓内 bug | go-normalize-email | evals/core | go test | 15m | NONE |
| 3 | B 中等仓内 bug | ts-concurrency-limit | evals/core | node --test | 15m | NONE |
| 4 | C 跨文件推理 | n3-caller-propagation | evals/navigation | 隐藏测试注入 | 20m | LOW |
| 5 | F 行为保持重构 | rust-area-trait | evals/hard | cargo test | 15m | NONE |
| 6 | E 测试失败诊断 | c4-r2-test-recovery | evals/recovery | 隐藏链 | 20m | LOW |
| 7 | F 移动依赖重构 | c3-e5-move-dependency | evals/edit | 符号迁移+隐藏测试 | 20m | LOW |
| 8 | D 搜索型真实仓 | yq-doc-count | evals/realrepo（yq @v4.44.3，477 文件） | bash 验收脚本 | 30m | MEDIUM |
| 9 | G 长多阶段 | icg-5-long-task | evals/icg（navsvc） | bash 验收脚本 | 30m | MEDIUM |
| 10 | H 多 Agent 机会 | scale-s800 | evals/scale（800 文件真实仓形态） | bash 验收+改动范围围栏 | 30m | HIGH |
| 11 | I Browser（CL-only 能力验证） | 前端行为诊断任务 | 真实前端仓 | 行为脚本 | 30m | LOW |
| 12 | J Restart（CL-only 能力验证） | SIGKILL 连续性 | 真实任务 | 事件/结果核验 | — | — |

case 11/12 是 CodeLeveler 能力验证车道，不进对照均值（对照家无等价能力 → UNSUPPORTED，见 §40 能力矩阵）。

## 5. 公平性（§6/§51）

- 同 case、同冻结 revision、同 prompt（case 的 `task` 原文）、同 wire 模型、同网关同 key、
  干净隔离工作区、同 wall-clock 超时、零人工干预。
- 对照车道三家统一用各自的**无头 CLI**跑（CL 用 `leveler run --auto-approve`；AtomCode 用 `-p -y`），
  统一由适配器落料工作区、注入 prompt、卡超时、跑 `expect` 判分。适配器不改 prompt、不加重试、不后处理答案。
- 权限档：各家自身的"自动批准"档（CL assisted+auto-approve；AtomCode `-y`）。语义差异记入 fairness 文档。
- 已知例外记录于 `docs/evaluations/COMPARATIVE_FAIRNESS.md`。
- 失败标签：PASS / PARTIAL / FAIL / INFRA_FAILURE / UNSUPPORTED，判分前冻结：
  - PASS = expect 退出码 0；FAIL = 正常跑完但 expect 非 0；
  - PARTIAL = expect 非 0 但已产生有意义且方向正确的变更（以 diff+expect 局部通过为证，逐例给证据）；
  - INFRA_FAILURE = provider 故障/适配器故障，绝不折算成 FAIL；
  - FALSE_COMPLETION = harness 自报完成 && expect 失败（CL 有结构化终态；对照家以最终输出的完成声明判，语义差异记 fairness）。

## 6. Phase A — Internal Baseline

冻结二进制 + 原生 eval runner（隔离 `LEVELER_HOME`、去代理），flash：

| lane | cases | rep | 历史锚点 |
|---|---|---|---|
| smoke | 3 | 1 | 3/3 |
| hard | 5 | 2 | C1/C2 期全绿 |
| navigation | 8 | 1 | BV2 24/24（c2-bv2-navigation.json） |
| edit | 8 | 1 | C3 24/24 |
| recovery | 8 | 1 | C4 24/24 + recovery 100% |
| icg | 6 | 1 | ICG 15/15（icg-6r 期望 completed_unverified） |
| extra（对照集预检） | go-normalize-email / ts-concurrency-limit / yq-doc-count / scale-s800 | 1 | — |

**门（§64）**：safety=0、false_completion=0、完成率不低于历史锚点显著幅度、icg-6r 诚实收场正确。
未过 → INTERNAL_REGRESSION，停，不进 Phase B。

附注（2026-08-28 裁决）：首轮 Phase A 于 `366f7ad9` 完成 47 runs/45 pass/正常任务 fc=0；icg-6r
不可能任务探针复现"范围偷换假完成"（2/3，历史率一致），用户裁决 **OPEN_BETA_REQUIRED**：
先修诚实收场（措辞契约 ×2 commit）、icg-6r ×5 全诚实为验收线，再于新基线重跑全 Phase A，
之后 Phase B 才解除 HOLD。

## 7. Phase B — 对照执行

1. 能力矩阵（§40）先行。
2. 主车道：10 case × 2 rep × {CL, AtomCode, DSH?} @ flash。
3. Thesis 车道：10 case × 2 rep × {AtomCode+pro, CL+glm-5.2}（+ 支撑臂按预算裁剪）。
4. 差异分析（§56）以逐 case 轨迹证据为准，不做无证据的架构归因。

## 8. Phase C — Real Usage

3-5 个真实任务（非评测夹具）：候选=CodeLeveler 自身已知小缺陷（在 worktree 副本上做，不动冻结基线）、
dogfood 真实仓（memos/go-task/tailadmin 类）小 issue、一个前端行为修改（兼作 case 11）、
一个 restart 连续性任务（兼作 case 12）。结构化观察记录按 §46。

## 9. 停止条件

§63 全套照抄执行；另：browser 会话归属 flake 若再现，记 occurrence/总数/环境并评估是否升级。

## 10. 产物

- 本计划：`docs/evaluations/PRE_BETA_EVAL_PLAN.md`
- `docs/evaluations/CODELEVELER_INTERNAL_BASELINE.md`
- `docs/evaluations/HARNESS_COMPARATIVE_EVAL.md`
- `docs/evaluations/COMPARATIVE_FAIRNESS.md`
- `docs/evaluations/REAL_USAGE_FINAL.md`
- 机读结果：`eval/comparative/results/*.json`（复用 BaselineDocument 风格的扁平 JSON）

## 11. 卫生项（顺带发现，非本轮 scope）

`~/.leveler/config.toml` 注释中存在两行明文 `sk-` 密钥字面量；eval 运行会把该文件复制进
`eval/runs/*/home/`（该目录已 gitignore，未入库——已核实 `git ls-files eval/runs` 仅 README）。
建议尽快手工删除这两行注释密钥并轮换。
