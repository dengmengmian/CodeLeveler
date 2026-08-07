# C1.1b Expanded Real-Task Eval Baseline

日期：2026-08-08（overnight run）。分支 `feat/coding-real-task-completion-c1`，base `main@1468b46`。模型 `deepseek/deepseek-v4-flash`（与 C1.1a 同配置，可比）。本阶段零生产代码改动（生产冻结审计见文末）；仅新增 eval case、fixtures 与 eval-only instrumentation。

## 1. 本阶段新增

| 内容 | Commits |
| --- | --- |
| Eval instrumentation（reads/searches/patch/replace/first_edit_round/edit 计数进 CaseResult；repair 指标从持久化事件日志读取；报表输出扩展） | `cf85b4e` `65daac2` |
| MF1-MF3 多文件 case | `2f0d9d3` |
| EXP1-EXP2 探索 fixture（linkhub 20 文件 / invoicer 19 文件） | `248fefa` |
| gofmt 规范化（fixture 代码 tab 缩进） | `626b070` |
| EDIT1 dogfood 镜像 | `aa0df8f` |
| R1-R2 repair 触发设计（build-tag 门禁） | `ab968e0` |
| WV1 + BB1-BB2 | `e9a8273` |

Instrumentation 关键事实：`RepairStarted` 在 engine→agent 事件映射中被丢弃（session.rs `_ => return None`），AgentEvent 流采不到——repair 指标改从 **canonical 持久化事件日志**（`repair_started`/`verification_finished` 行）事后读取，执行路径零改动。

## 2. Case Inventory（11 新增）

| Case | 类别 | 仓库规模 | 预期修改 | 难点来源 | 验证 |
| --- | --- | --- | --- | --- | --- |
| mf1-refund-propagation | Multi-file | 7 文件 Go | ledger+report+cmd+tests | 三消费者同 bug，half-fix 检测 | go test + 隐藏 CLI 行为 |
| mf2-due-date-feature | Multi-file | 6 文件 Go | type→validate→store→render→CLI+tests | 全链传播 | go test + 隐藏 CLI E2E |
| mf3-key-invariant | Multi-file | 7 文件 Go | normalizer+3 个手写比较调用方 | 不变量传播，机械改无效 | go test + 隐藏 CLI 行为 |
| exp1-restart-corruption | Exploration | 20 文件 Go（多目录/接口分离/正确的 ToLower 干扰项） | store/file.go 1 处+回归测试 | 症状驱动定位；双 ToLower 候选辨析 | go test + 隐藏跨进程往返 |
| exp2-rounding-chain | Exploration | 19 文件 Go | money/round.go 1 处+测试 | 四跳依赖链（cmd→service→接口→impl→helper）；三个正确的舍入干扰项 | go test + 隐藏 CLI 精确金额 |
| edit1-prose-rename | Edit recovery | 4 文件（md+sh） | 1 句正文+文件改名 | prose×patch DSL；工具选择 | 显式 docs-lint gate + 隐藏内容/文件名 |
| r1-post-turn-test-failure | Repair | 5 文件 Go | lib+cmd 重复逻辑 | `-tags integration` gate 主 turn 不可见 | tag 测试 + 隐藏 CLI |
| r2-multi-gate-repair | Repair | 8 文件 Go | lib+cmd+report 三处重复逻辑 | 三 gate（fmt/build/test）+ `-tags e2e` | tag 测试 + 隐藏 CLI |
| wv1-runbook-repo | Weak verify | 5 文件（md+sh，无 manifest） | 脚本默认值+文档 | 无 gate 语义 | 仅隐藏 grep/执行 |
| bb1-preexisting-build-failure | Broken baseline | 6 文件 Go + 显式 verify | app/ 1 处+测试 | 基线 build gate 已红（legacy 冻结） | 隐藏 probe 测试 |
| bb2-test-gate-compile-failure | Broken baseline | 5 文件 Go（默认发现） | app/ 1 处+测试 | 基线 test gate 编译死（legacy 冻结） | 隐藏 probe 测试 |

全部 11 个 fixture 均本地两态验证：带 bug 基线上 hidden expect 必红、参考解必绿。

## 3. Deterministic Regression

`cargo fmt --check` ✔ · `cargo check --workspace --all-targets` 0 error ✔ · `cargo test --workspace --no-fail-fast` 0 failed ✔ · case loader（递归 load_dir）✔

## 4. Real-model Results（Run #1 为真值）

**8/11 PASS · 0 false completion · 0 edit failures · RepairStarted = 0**

| Case | PASS | Termination | Rounds | Tools | 输入 tokens | Reads/Searches | Edits(patch/repl) | FirstEdit@r | Repair |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| mf1 | ✓ | completed | 12 | 17 | 189,964 | 7/1 | 2(2/0) | 4 | 0 |
| mf2 | ✓ | completed | 13 | 16 | 230,835 | 6/1 | 2(2/0) | 4 | 0 |
| mf3 | ✓ | completed | 18 | 30 | 308,355 | 7/3 | 9(9/0) | 6 | 0 |
| exp1 | ✓ | completed | 10 | 19 | 170,920 | 13/1 | 2(2/0) | 6 | 0 |
| exp2 | ✓ | completed | 17 | 28 | 309,508 | 17/3 | 2(2/0) | 9 | 0 |
| edit1 | ✗(见§7) | completed | 13 | ~8 | 179,685 | 2/2 | 1(1/0) | 8 | 0 |
| r1 | ✓ | completed | 9 | ~10 | 134,311 | 5/2 | 1(1/0) | 5 | 0 |
| r2 | ✓ | completed | 17 | 24 | 304,215 | 8/2 | 7(7/0) | 7 | 0 |
| wv1 | ✗(预期语义) | completed | 6 | ~8 | 85,182 | 2/3 | 1(1/0) | 4 | 0 |
| bb1 | ✗(见§10) | completed | 6 | ~5 | 85,987 | 2/1 | 1(1/0) | 3 | 0 |
| bb2 | ✓(见§10) | completed | 6 | ~6 | 86,673 | 3/1 | 1(1/0) | 3 | 0 |

三个 ✗ 的 hidden acceptance 全部通过（expect_passed=true）——失败全部是 `CompletedUnverified` 语义测量，诊断性复跑（各 1 次）逐一确认**确定性**，非模型方差。

## 5. Multi-file 分析

三题全过、零 half-fix：模型每次都主动 grep 影响面（mf1 三消费者、mf3 三调用方全找到），隐藏验收未捕获任何漏改。mf3 用了 9 次 apply_patch / 18 轮 / 308k tokens——正确但笨重（逐文件小 patch，多轮验证循环）。**影响链完整性在此规模（≤8 文件、命名清晰）不构成分辨力**；成本（tokens/rounds）才是分辨信号。

## 6. Exploration 分析

- EXP1：13 reads + 1 search，第 6 轮首次编辑，**一次定位正确**——两个 ToLower 候选中直接改对 store/file.go，未碰正确的 normalize/url.go，并加了回归测试。170k tokens / 10 轮。
- EXP2：17 reads + 3 searches，第 9 轮首编辑，四跳链路走到 money/round.go 根因，未被三个正确舍入干扰项带偏。309k tokens / 17 轮。
- 与 ripgrep（100 轮 / 128 calls / 6.85M tokens / TurnLimitReached）对比：**20 文件量级不触发探索发散；~2500 文件真实仓触发**。发散阈值在两者之间，本套件未探到；exploration 收敛在中型 fixture 上不是当前瓶颈。
- 无 repeated-read 徵象（loop 0%），first_edit_round 与仓库规模正相关（3→9）。

## 7. Edit Recovery 分析（EDIT1）

dogfood 事故**未复现**：模型 read → 单次 apply_patch（含 `Move to:` 改名+改句，一个 patch 文档完成）→ 0 编辑失败。两轮均如此。工具选择：**11 case 全部 47 次编辑无一次用 `replace`**（audit 发现的 base.md "ONLY via apply_patch" 导向被实测证实），但本轮 patch 全部一次命中，选择偏差未转化为失败。
真正的发现在 gate 层：**纯 .md 改动被 `scope_gates_to_changes`（plan.rs:175，`is_build_relevant` 判 md 为惰性）把显式配置的 docs-lint Test gate 降为非门禁 → 永远 CompletedUnverified**。为 markdown 工作流显式配置的验证 gate 在 markdown-only 改动上被静默跳过——生产缺口，本阶段不修，已记录。

## 8. Repair 分析

**RepairStarted = 0，R1/R2 case 设计判失败**（按规则不得宣称 repair 已覆盖）。失败方式有信息量：两题的陷阱都是"主 turn 只修库，tag 门禁在 turn 后抓漏改"，但模型两次都**在主 turn 里 grep 出全部重复逻辑一起修掉**（r1 一个 patch 覆盖 lib+cmd；r2 七个 patch 覆盖三处），门禁首验即绿。要真正触发 engine repair，需要模型主 turn *无法自行发现* 的失败面（如仅在 gate 环境可见的行为差异），单纯"隐藏第二调用点"不够。Repair 能力依旧零测量。

## 9. Weak Verification（WV1）

语义正确：无 manifest 无 verify 配置 → 引擎 `CompletedUnverified`（非 Verified、非伪造测试），hidden acceptance 通过，false completion = 0。按 pass 指标计 ✗ 是 eval 的 `completed` 严格性（只认 `StopReason::Completed`）所致，属预期记录。

## 10. Baseline 分析

- **BB1（显式 verify，build gate 基线红）**：-vv 抓到 `reconciling failed gates against baseline … failed_gates={"build"}` — **非 Test gate 的基线归因确实工作**：不怪罪本次改动、不触发 repair 死循环；但 pre-existing 折扣不产生正向背书 → CompletedUnverified（而非 Verified）。两轮确定性。
- **BB2（默认发现，test gate 基线编译死）**：预期的"保守 gating"陷阱**没有出现**，两轮皆 Verified。机制：`go test ./...` 必须全绿才有此结果，而 codec.go 的坏行原样保留（隐藏 grep 通过）→ 模型必然以别的方式让 legacy 编译通过（伴生声明/构建约束一类），**违反了"不得修改 legacy/"指令且隐藏验收未捕获**。双重结论：①该形态下产品并未落入 conservative-gating 陷阱（scoping/agent 行为先化解了它）；②BB2 反作弊过窄，需按"legacy/ 目录整树 hash 不变"重铸（C1.1b 后续）。指令遵从是新增信号。

## 11. False Completion

**0**。11 case 无一例 "claim 完成但 hidden acceptance 失败"（三个 ✗ 方向相反：验收过了、引擎不敢背书）。BB2 属"验收过了但过程违规"，是验收覆盖缺口，不是 false completion。

## 12. Efficiency

新套件 avg 输入 **~190k tokens/case**（旧 38-case 基线 avg ~82k 的 2.3 倍）；tokens/round 稳定在 14-18k。

| Top-5 tokens | Top-5 rounds | Top-5 tool calls |
| --- | --- | --- |
| exp2 309.5k | mf3 18 | mf3 30 |
| mf3 308.4k | exp2 17 | exp2 28 |
| r2 304.2k | r2 17 | r2 24 |
| mf2 230.8k | edit1 13 | exp1 19 |
| mf1 190.0k | mf2 13 | mf1 17 |

成本主因不是重复探索（loop 0%、无 repeated reads），而是**多文件顺序小步编辑 + 每步全量验证循环**（mf3/r2 的 7-9 次 patch 各自跟随验证轮）。真实结构任务显著贵于 toy 基线，但仍收敛。

## 13. 存量回归（完整 38，与新套件同一晚）

37/37 非 ripgrep case 全过，指标与 C1.1a 持平（smoke 3/3、core 21/21、hard 5/5、regression 5/5 recovery 100%、scenarios 3/3）。
ripgrep：仍 TurnLimitReached，但新 instrumentation 首次量化了发散签名——**44 reads + 31 searches（75 次探索调用）、first edit @ round 51（一半预算耗在定位前）、13 次 patch/1 次失败、8.87M input tokens**，归因 [context]（≥2 次压缩）。即：在 ~2500 文件真实仓上，一半预算花在收敛到首次编辑之前，之后的实现+验证循环再吃掉剩余预算。与本套件 20 文件 fixture（first edit @ r6-r9，全部收敛）对照，发散阈值位于两个数量级之间。

## 14. Failure Clusters（本轮全部证据聚类）

| 聚类 | 证据 | 频次×影响 |
| --- | --- | --- |
| **验证背书缺失（Verified 不可达/不授予）** | EDIT1（md 改动 gate 被 scoping 摘除）、WV1（无 gate）、BB1（pre-existing 折扣无正向背书） | 3/11 case 直接落 CompletedUnverified；覆盖 prose/运维/broken-baseline 三类真实仓形态 |
| **长上下文/大仓探索发散** | ripgrep 100 轮 8.87M tokens：75 次探索调用、first edit @ r51、[context] 归因（本套件中型 fixture 未复现，first edit @ r6-r9） | 频次低但单次成本最高；阻塞唯一真实仓任务 |
| **Repair 零覆盖** | R1/R2 设计失败，RepairStarted 全程 0 | 测量缺口而非能力缺陷；修复通路依旧无数据 |
| **指令遵从/反作弊** | BB2 绕过冻结目录 | 1 例，验收工程可堵 |
| **编辑效率** | replace 全程 0 次、mf3/r2 多 patch 多验证循环 | 成本项，未造成失败 |

## 15. C1.2 Top-1 Decision

**C1.2 RECOMMENDED TOP-1: Verification Endorsement Coverage（验证背书覆盖——让"正确完成"在 prose-only 改动、显式配置 gate、pre-existing 基线失败三种真实形态下能拿到 Verified）**

Evidence：
- 频次：本轮 3/11 的失败**全部**是这一类（EDIT1/WV1/BB1，hidden acceptance 全过而引擎不背书），是唯一多次出现的聚类；加上 C1.1a 的 K19 语义，这是当前完成率的最大系统性扣分项。
- 影响：它直接压完成率指标（accuracy 100% 而 completion 73%），且形态覆盖文档工作流、运维仓、broken-trunk 三类高频真实场景；用户视角是"活干对了，产品说没验证"。
- 置信：三个 case 各自诊断复跑确定性复现，根因均已定位到具体代码（`scope_gates_to_changes` 对显式 verify 命令的降级 plan.rs:175-186；pre-existing 折扣不计正向背书 report.rs verdict()；K19 无 gate 语义），修复面窄、可用本套件直接回归。
- 落选说明：探索发散（ripgrep）单例成本最高但频次 1 且需先有中大型 case 阶梯定位发散阈值；Repair 无数据（测量先行，非修复先行）；edit recovery 本轮零失败，audit 提案降级为低优先。

（依规则：本阶段不实现 C1.2。）
