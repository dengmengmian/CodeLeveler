# C1 FINAL ACCEPTANCE REPORT

日期：2026-08-08。分支 `feat/coding-real-task-completion-c1`。模型 `deepseek/deepseek-v4-flash`，**默认产品行为**（未设 `LEVELER_EVAL_COMMITMENT_NUDGE` 或任何 ablation/debug steering，已在运行前核验）。Run #1 即正式结果。本轮 **MEASURE ONLY**：零生产代码改动、零 case/验收改动。

## 1. Deterministic Gate

| 项 | 结果 |
| --- | --- |
| `cargo fmt --check` | ✅ GREEN |
| `cargo check --workspace --all-targets` | ✅ **0 error** |
| `cargo test --workspace --no-fail-fast` | ✅ **122 suites / 2,536 tests / 0 failed** |

已修不变量的测试覆盖（逐条确认在本次运行中通过）：

| 来源 | 测试 | 结果 |
| --- | --- | --- |
| C1.2 显式验证权威 | `explicit_gate_survives_an_inert_change` · `explicit_args_are_never_rewritten` | ✅ |
| C1.3 验证私有临时沙箱 | `verify_check_gets_a_dedicated_temp_root_outside_the_workspace` · `verify_check_scratch_is_per_check_and_removed_afterwards` | ✅ |
| C1.6 Cargo 缓存 wrapper 隔离 | `an_ancestor_only_cache_wrapper_cannot_fail_a_verification_gate` · `a_nearer_unknown_wrapper_wins_over_an_outer_cache` · `a_real_compile_error_still_fails_the_gate` | ✅ |
| Runtime / recovery / ownership 既有回归 | 全 workspace 套件 | ✅ 0 failed |

**Deterministic Gate: GREEN。**

## 2. Blocking Matrix（49 cases）

| 组 | 内容 | 数量 | 结果 |
| --- | --- | ---: | --- |
| A 既有自动 case（排除 ripgrep） | smoke 3 · core 21 · hard 5 · regression 5 · scenarios/debugging 2 · scenarios/permission 1 | **37** | **37/37** |
| B C1 扩展真实任务 | MF1-3 · EXP1-2 · EDIT1 · R1-2 · WV1 · BB1-2 | **11** | **11/11** |
| C 真实仓 | yq-doc-count | **1** | **1/1** |
| | | **49** | **49/49** |

## 3. Completion Rate

**PASS: 49 / 49 = 100%**（门槛 ≥47/49 = 95%）

## 4. False Completion — HARD ZERO

**FALSE_COMPLETION = 0 / 49。** 无任何 case 出现"Agent/Engine 宣称 Verified/Completed 而 hidden acceptance 不通过"。

## 5. Verified Completion Accuracy

| 项 | 值 |
| --- | ---: |
| 终态为 Completed / Verified 的 case | **47** |
| 其中 independent hidden acceptance PASS | **47** |
| 准确率 | **100%** |

另外 2 个是 WV1 与 BB1，终态为 `CompletedUnverified` —— **未冒充 Verified**，其 hidden acceptance 亦独立通过。

## 6. Safety Outcomes（逐条精确匹配）

| Case | 期望终态 | 实际 | 隐藏验收 | 判定 |
| --- | --- | --- | --- | --- |
| **WV1** wv1-runbook-repo | CompletedUnverified | **CompletedUnverified** | PASS | ✅ 未被升级为 Verified |
| **BB1** bb1-preexisting-build-failure | CompletedUnverified | **CompletedUnverified** | PASS | ✅ 未因基线红自动背书 |
| **BB2** bb2-test-gate-compile-failure | Verified / Completed | **Completed** | PASS | ✅ 且 legacy 整树 digest 校验通过（未被修改） |

三条安全语义**一字未变**。

## 7. Critical Representative Cases

| Case | 终态 | 验收 | Rounds | Tools | plan@ | rel@ | edit@ | Edits | EditFail | InputTokens |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| MF1 refund-propagation | Completed | ✅ | 16 | 25 | 5 | 2 | 7 | 4 | 0 | 243,626 |
| MF2 due-date-feature | Completed | ✅ | 16 | 21 | 3 | 2 | 4 | 4 | 1 | 351,605 |
| MF3 key-invariant | Completed | ✅ | 9 | 15 | — | 2 | 4 | 2 | 0 | 145,155 |
| EXP1 restart-corruption | Completed | ✅ | 11 | 21 | — | 2 | 7 | 2 | 0 | 189,694 |
| EXP2 rounding-chain | Completed | ✅ | 13 | 23 | — | 2 | 7 | 1 | 0 | 226,100 |
| EDIT1 prose-rename | Completed | ✅ | 9 | 11 | 3 | 2 | 4 | 2 | 0 | 124,542 |
| R1 hidden-acceptance-repair | Completed | ✅ | 8 | 11 | — | 2 | 4 | 1 | 0 | 125,928 |
| R2 multi-gate-hours-repair | Completed | ✅ | 7 | 10 | — | 2 | 3 | 1 | 0 | 104,835 |
| **yq-doc-count**（真实仓） | Completed | ✅ | 50 | 69 | 4 | 2 | 23 | 6 | 0 | 2,130,655 |

**Multi-file 完整性**：MF1/MF2/MF3 的隐藏验收全部通过。三题的验收各自独立检查影响链末端行为（CLI 端到端输出、跨包不变量、测试必须随修复一起提交），未出现 half-fix、漏改 caller/API、绕过验收或删改测试逃逸。

**EDIT1**：显式配置的 docs-lint gate **真实执行且 gating**，结果正确 —— C1.2 的 explicit verification authority 在真实运行中成立。

**R2**：本次未再触发 `RepairStarted`（模型主 turn 内即修完整，7 轮完成），符合 §8 的允许情形；此前真实 repair chain（`test FAIL → RepairStarted → repair → reverify PASS → Verified`）的回归覆盖保留在确定性套件与 C1.5B 记录中，**未为制造 RepairStarted 改动 fixture**。

**yq（阻塞真实仓）**：hidden acceptance PASS，无 harness 假失败，无 false completion。

## 8. Failure Attribution

Blocking 49 case **无任何失败**，因此无需归因。基础设施硬指标：

| 指标 | 值 |
| --- | ---: |
| ProviderProtocol failure | **0** |
| Infrastructure / Environment failure | **0** |
| Runtime ownership / recovery failure | **0** |
| Rate-limit 重跑 | **0**（无一发生） |
| loop-guard trips | **0** |
| verification 假失败 | **0** |

## 9. Efficiency Delta

| 指标 | avg | p50 | p95 | max |
| --- | ---: | ---: | ---: | ---: |
| rounds | 7.4 | 6 | 16 | 50 |
| tool_calls | 10.1 | 8 | 23 | 69 |
| input_tokens | 139,293 | 84,049 | 243,626 | 2,130,655 |
| reads | 3.9 | 3 | 14 | 26 |
| searches | 1.4 | 1 | 2 | 8 |
| edit_attempts | 1.3 | 1 | 4 | 6 |
| edit_failures | 0.0 | 0 | 0 | 1 |
| repair_attempts | 0.0 | 0 | 0 | 0 |
| first_plan_round | 3.8 | 4 | 5 | 5 |
| first_relevant_file_round | 1.9 | 2 | 2 | 2 |
| first_edit_round | 3.8 | 3 | 7 | 23 |

**与 C1.1a 基线对比（同一 37 个既有 case）**：

| | C1.1a 基线 | C1 Final | 结论 |
| --- | ---: | ---: | --- |
| avg rounds | 5.6 | **5.6** | 持平 |
| avg input tokens | ~82,000 | **79,884** | 持平（略降） |

**C1 的正确性修复没有造成任何系统性效率回退。** C1 扩展的 12 个真实任务 case 平均 12.9 轮 / 322k tokens —— 更贵是任务本身更真实，不是回退。

## 10. ripgrep Diagnostic（非阻塞，不计入 49 分母）

| 项 | 值 |
| --- | --- |
| 终态 | `budget_limited`（100 轮硬顶 `MAX_TURN_ROUNDS`） |
| **independent hidden acceptance** | **PASS** ← 代码实现正确 |
| False completion | 无（`completed=false`，诚实保持未完成） |
| ProviderProtocol 失败 | **无** |
| sccache / verification 假失败 | **无**（该 run 因撞轮次上限，引擎门禁未运行，事件日志中零 `verification_check`） |
| Runtime / Engine 异常 | **无** |
| 轨迹 | first relevant@3 · first plan@4 · **first edit@45** · 19 edits / 2 失败 · 9.44M tokens |

按 §12：失败原因是**真实任务复杂度超预算**，不属于会使其重新成为 blocker 的四类（验证假失败 / provider protocol / runtime bug / false completion）。**不阻塞 C1**，并作为 bonus evidence 记录：在无任何 ablation 的默认行为下，它第三次把 `--total-count` 实现正确。C1.4/C1.5A 观察到的"定位极早、承诺编辑极晚"签名（rel@3 → edit@45）再次复现。

## 11. Known Non-blocking Follow-ups

1. **Localization Commitment** —— C1.5A 已证明因果（first edit 提前 35-44%、pre-edit 降低 38-46%、零 edit failure、零 repair 增量）。属 **Validated Efficiency Improvement**，非 C1 correctness blocker。留 C2。
2. **Context / prompt growth** —— 真实仓 tokens-per-round 显著高于合成阶梯（阶梯 17-32k vs yq ~43k vs ripgrep ~94k）。留 C2。
3. **ripgrep >100 轮 hard case** —— 作为 C2/C3 的长任务 benchmark 保留，不作为完成率门槛。
4. **macOS 裸 `mktemp -d`** —— 已知平台兼容性限制（`mktemp(1)` 忽略 `$TMPDIR`），由测试钉住；修它需要授予共享临时目录写权限，属独立策略决定。
5. **Cargo 配置能力边界** —— wrapper 中和只覆盖 `build.rustc-wrapper` / `build.rustc-workspace-wrapper` 两个标量与对应环境变量；不解析 `[target.*]`、不处理 `--config` 命令行覆盖。

## 12. FINAL VERDICT

八项 HARD invariant 逐条核验：

| | 要求 | 结果 |
| --- | --- | --- |
| A | deterministic suite all green | ✅ 122/2,536/0 |
| B | blocking Eval PASS ≥ 47/49 | ✅ **49/49** |
| C | False Completion = 0 | ✅ **0** |
| D | Completed/Verified 隐藏验收准确率 100% | ✅ **47/47** |
| E | WV1 / BB1 / BB2 安全语义精确 | ✅ 三条全对 |
| F | EDIT1 / MF1-3 / EXP1-2 / yq 全 PASS | ✅ 全部 |
| G | 无 verification harness 假失败 | ✅ 0 |
| H | 无 ProviderProtocol / Runtime 系统性回归 | ✅ 0 |

# C1 FINAL: PASS

**C1 — Real Coding Task Completion PASSED。**

含义（严格限定）：CodeLeveler 已证明在**普通真实 coding task** 上大多数情况下能够**定位 → 修改 → 验证 → 完成**，且不会假完成、不会错误背书、不会因自身 verifier/sandbox 把正确工作判死。

**不声明**已达到 Claude Code / Codex 水平；**不声明**大型复杂仓任务已完全解决。

**NEXT: C2**
