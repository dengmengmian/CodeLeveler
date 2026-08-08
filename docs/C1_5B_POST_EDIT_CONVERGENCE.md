# C1.5B — Post-Edit Convergence Final Diagnosis

日期：2026-08-08。分支 `feat/coding-real-task-completion-c1`。**纯离线轨迹分析**：不跑模型、不改生产代码，全部结论来自引擎自己的持久化事件日志（`scripts/analyze_trajectory.py`，本轮新增，可复现）。

**结论前置：POST_EDIT_CONVERGENCE_DEFECT = NOT_CONFIRMED。** ripgrep treatment 的 60 个 post-edit 轮次里**没有任何 evidence→action 卡顿的签名**：同一诊断连续出现 0 次、mutation 间隔中位数 1 轮。这 60 轮分解为真实实现（~20，止于 r55 首个绿色构建）、工具链/权限摩擦（~16）、**自行加写集成测试**（~21）、收尾（3）。而且 agent **确实调用了 `update_goal{complete}`** —— 它收敛了、也发出了完成信号；最终 Incomplete 是**引擎门禁被 sccache 环境故障打红**。

## 1. Post-edit 指标（treatment 双仓）

| 指标 | ripgrep | yq |
| --- | ---: | ---: |
| PostEditRounds | 60 | 27 |
| PostEditTools | 61 | 28 |
| Mutations | 12 | 8 |
| MedianMutationGap（轮） | **1** | **1** |
| MaxMutationGap（轮） | 26 | 7 |
| ActionsBetweenMutations（中位/最大） | 0 / 25 | 0 / 6 |
| Gating 命令（BUILD/TEST/CHECK） | 4 | 4 |
| RepeatedBuildWithoutMutation | 3 | 2 |
| 重复同一命令 | 1 | 0 |
| **RepeatedSameDiagnostic（连续两次同诊断）** | **0** | **0** |
| Reads/Searches（post-edit） | 18 | 1 |
| 其中重读 pre-edit 已读文件 | 7 | 0 |
| FirstBuildPass | **r55** | r28 |
| FirstTestPass | 未达（命令报错，见 §3） | r32 |
| 首个绿色之后的 read/search | 8 | **0** |
| Acceptance | ❌ FAIL | ✅ PASS |
| Outcome | Incomplete | Completed |

**关键否定证据：两仓的 `RepeatedSameDiagnostic` 都是 0，mutation 间隔中位数都是 1 轮。** "同一个错误 → 反复读/搜 → 迟迟不改" 这一 §5 定义的 stagnation 签名**不存在**。

## 2. ripgrep post-edit 时间线（压缩）

```
r35-r43  7 次 edit（defs.rs → lowargs.rs → hiargs.rs ×3 → main.rs）      实现爆发
r44-r52  9 轮读/搜 printer/{summary,standard,json}.rs                    找"匹配行在哪里被计数"
r53      edit hiargs.rs
r54      BUILD ✗ —— 失败原因是**工具用法**错误（带 && 的命令要用 shell_command），非编译错误
r55      BUILD ✓                                                        ← 首个绿色构建
r56      shell ✗ —— **权限/审批被拒**（"wait for approval, then retry"）
r57-r61  mkdir / 手动跑 ./target/debug/rg --total-count 自验
r62      TEST ✗ `cargo test -p ripgrep --lib` → "no library targets"     命令选错
r64      TEST ✗ `cargo test --bin rg` → "could not compile `log`"        工具链
r65-r71  读 .cargo/config.toml、排查 sccache/RUSTC_WRAPPER
r72-r92  grep SHERLOCK、读 tests/{misc,hay,tests,macros,util}.rs、
         写 tests/feature.rs（rgtest! total_count_*）、跑测试            **任务并未要求加测试**
r93-r95  update_plan → git_diff → update_goal{status:complete}
```

## 3. Shell 命令与诊断进展

4 条 gating 命令（2 BUILD / 2 TEST），其余 24 条是 INSPECTION/手动运行/环境排查。两次 TEST 失败的诊断**各不相同**且都不是代码缺陷：

- `error: no library targets found in package 'ripgrep'` —— 选错命令（ripgrep 无 lib target）。
- `error: could not compile 'log' (lib)` —— 依赖编译失败，随后被定位为 sccache。

**没有一次"同一诊断重复出现"**。这符合 §5 的健康收敛定义，不符合 stagnation。

## 4. Post-edit 探索分类（§7）

18 次 read/search 中：
- **A 类（跟随最新诊断）**：读 `.cargo/config.toml`（跟随 `could not compile log`）—— 合理。
- **C 类（沿新依赖扩展）**：r44-r52 读 printer 层（`--total-count` 必须知道 matched-lines 从哪来）—— 合理。
- **D 类（重回已解决区域）**：7 次重读 pre-edit 已读文件，集中在 printer/summary.rs（r44-r48 连读 4 次同一文件）—— **唯一可疑项**。
- **E/F 类（全仓 broad search / 与当前失败无关）**：r72-r77 的 SHERLOCK + tests/*.rs 属于**自行扩大任务范围去写测试**，不是无目的漫游。

即：真正的 convergence 信号（D/E/F）只占少数，且 E/F 有明确意图（写测试），不是漫游。

## 5. Completion Signal（§9）

事件日志给出确定答案：

```
seq 670  tool_call update_goal {"status":"complete","summary":"--total-count flag implemented, tested, and verified…"}
seq 671  → "Goal resolved."
seq 675  turn_finished  modified_files=[defs.rs, lowargs.rs, hiargs.rs, main.rs, tc_data/*.txt, tests/feature.rs]
seq 677  verification_check  cargo fmt        → passed
seq 679  verification_check  …               → **sccache: error: Failed to create temp dir … No such file or directory**
seq 680  verification_finished passed=false
seq 681  task_finished outcome=failed  stop=completed
```

**不是 C（"有证据却不提交完成"）、不是 D（预算边界）。** Agent 提交了完成（`stop=completed`），是引擎门禁失败把 outcome 变成 failed；而门禁失败的证据字符串是 **sccache 无法创建临时目录** —— 即 C1.3 记录并 DEFERRED 的 P2（验证沙箱与工具链的兼容性）。本机 `~/.cargo/config.toml` 全局设了 `rustc-wrapper = "sccache"`，因此**每一次 cargo 调用**（agent 的、门禁的）都经过 sccache；沙箱外健康（`sccache --show-stats` 正常、主机构建 exit 0），沙箱内失败。

这也解释了 r56-r71 那 16 轮：agent 在正确地诊断一个它无权修复的环境故障。

## 6. Hidden Acceptance（§10）

`expect_passed = false`。**归因限制如实说明**：该运行未保留工作区（未设 `LEVELER_EVAL_KEEP_WORKSPACE`），无法直接检查最终代码。可以确定的两点：

1. **验收路径不是环境故障**：验收在沙箱外、用 `env_clear + scrubbed_environment` 运行。实测 `env -i PATH HOME cargo build -q` 与 `env -i PATH cargo build -q` 在同一 fixture 上均 **exit 0**，即 scrubbed 环境不会打断验收的构建。
2. **实现在形状上是完整的**（从持久化 patch 还原）：`struct TotalCount` + `impl Flag`（`name_long → "total-count"`、`Some("no-total-count")` 否定形式）、`lowargs → hiargs` 贯通、`.stats(self.stats.is_some() || self.total_count)`、`main.rs` 里 `if args.total_count() { … writeln!(wtr, "TOTAL:{}", total_matched_lines) }`，并且 agent 自己手工跑过 `./target/debug/rg --total-count …` 验证。

因此验收失败**更可能是真实的行为细节缺口**（例如 TOTAL 行的位置/计数语义），而非环境或收敛问题 —— 但**无法证明**，如实记为不可归因。同一 case 在 C1.4 的另一次运行中 acceptance 曾 PASS，说明该任务处于"接近完成、run-to-run 波动"的区间。

## 7. yq 对照（§11）

yq treatment 是健康形态的参照：post-edit 27 轮、8 次 mutation、间隔中位 1 最大 7、**首个绿色构建之后 0 次 read/search**、重复诊断 0、重复命令 0、acceptance PASS。

与 ripgrep 的差异不在"是否 thrash"（两者都不 thrash），而在：ripgrep 多了 16 轮工具链摩擦 + 21 轮自加测试。

## 8. Outcome 判定（§13）

| Outcome | 判定 |
| --- | --- |
| A — post-edit evidence-to-action thrash | ❌ 排除：重复诊断 0、mutation 间隔中位 1 轮、绿色后探索仅 8 次且有意图 |
| **B — toolchain feedback loop dominates** | ✅ **主因**：4 条 gating 命令里 2 条因命令选错/依赖编译失败，16 轮花在 sccache/权限排查；门禁最终就是被 sccache 打红 |
| C — code actually incomplete | ⚠️ **部分**：acceptance FAIL 且验收路径已排除环境因素，但工作区已删、不可归因 |
| D — correct but fails to finish | ❌ 排除：agent 已 `update_goal{complete}`，`stop=completed`；不是"不肯收尾" |

## 9. Commitment 结论保留（§12）

C1.5A 的因果结论不变且未被否定：通用 nudge 使 first edit 提前 35-44%、pre-edit 降低 38-46%、零 edit failure、零 repair 增量。本轮只是判定它**不需要**被包进一个更大的 convergence policy —— 因为 convergence defect 不存在。

## 10. FINAL DECISION

**POST_EDIT_CONVERGENCE_DEFECT: NOT_CONFIRMED**

**C1.5 PRODUCTION FIX: Localization Commitment**（按 §0 分支 B 的定位：**小型改善**，不是 convergence 大改 —— 把 C1.5A 已验证的 plan→mutation commitment 收敛成产品策略即可，不要再叠加第二个 loop guard）

**C1 STATUS AFTER C1.5B: READY_FOR_FINAL_GATE**

理由：ripgrep 的 100 轮不是通用收敛缺陷，而是（a）本机全局 `rustc-wrapper = sccache` 与验证沙箱不兼容 —— 即 C1.3 已 DEFERRED 的 P2，直接打红门禁并消耗 16 轮；（b）agent 自行扩大范围写集成测试 21 轮；（c）任务本身接近 100 轮量级。**不应再把 ripgrep 单 case 当作 C1 blocker。**

带入 Final Gate 的两项记录（本轮不实现）：
1. **Verification Toolchain Isolation**（P2 家族）：宿主全局 `rustc-wrapper` 在验证沙箱内失效 → 正确的工作被判失败。这是本轮唯一有**直接证据链**的产品缺陷（`verification_check` 事件原文）。
2. ripgrep 作为 >100 轮的专项 hard case 保留，用于长任务观测，不作为完成率门槛。

## 11. Regression

`cargo fmt --check` ✅ · `cargo check --workspace --all-targets` ✅ 0 error · `cargo test --workspace --no-fail-fast` ✅ 0 failed。本轮无 real-model 运行（纯离线分析）。

## 12. 复现方式

```bash
python3 scripts/analyze_trajectory.py ripgrep-total-count yq-doc-count          # 汇总指标
python3 scripts/analyze_trajectory.py ripgrep-total-count --full                # 完整 post-edit 时间线
```
