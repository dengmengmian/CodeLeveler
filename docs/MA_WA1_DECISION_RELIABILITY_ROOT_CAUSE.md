# MA-WA1 — Delegation decision reliability: root cause（取证结论）

日期：2026-08-18。证据：`codeleveler-dogfood-control/ma-wa1-frozen-main-reproducibility/`
（12/12 eligible 全量 EventLog 回放 + 每步 KEEP/DELEGATE 原文）。
本文回答一个问题：**offered → KEEP 之间到底发生了什么**。修复设计只允许引用这里的事实。

## 决策时间线（全 12 run）

offer 100% 由 plan 注册触发（EB-2 是 mutation_fallback，EB-1 从未触发），
与 plan 同轮；首编辑紧随其后；而 run 本体跑到 100+ 轮：

| RUN | OFFERED_ROUND | FIRST_EDIT | TOTAL_ROUNDS | DECISION | 依据（模型原话摘要） |
| --- | ---: | ---: | ---: | --- | --- |
| EA-1 | 16 | 27 | 116 | KEEP×6 | 注册文件共享 + serde 助手共用 + cargo target 争用 |
| EA-2 | 28 | 29 | 100 | （未回应）| 无逐步答复，直接开写 → 静默 kept |
| EA-3 | 32 | 33 | 100 | KEEP×5 | “全部经 Cargo.toml/util.rs/main.rs/tests.rs 耦合” |
| EA-4 | 39 | 40 | 100 | KEEP×6+**DELEGATE dedup** | dedup.rs+test 是新文件、与我的工作不相交 |
| EB-1 | — | — | 100 | 未触发 | 纯探索 run，无 plan 无 mutation |
| EB-2 | ~40(fallback) | 95 | 100 | KEEP | “三个 verb 小而紧耦合”（run 本身退化） |
| EB-3 | 34 | 35 | 100 | KEEP×4 | 2-4 步全部“依赖 step 1 的产物/二进制” |
| EB-4 | 52 | 53 | 100 | KEEP×4+**DELEGATE tests,docs** | “先用建好的二进制算出期望输出再交给 worker” |
| EC-1 | 13 | 14 | 100 | KEEP×4 | 序列化语义一致性耦合 + mod.rs 单一归属 |
| EC-2 | 27 | 28 | 132 | KEEP×5 | “schema 决策在我脑子里；~150 行” |
| EC-3 | 43 | 44 | 100 | KEEP×4 | 同上；“1 个测试，写比交接便宜” |
| EC-4 | 19 | 21 | 100 | KEEP×6 | 六步共享 serde 视图结构/TOML null 限制 |

## 分类计数（每 run 取主导原因）

| 类别 | 数 | Run |
| --- | ---: | --- |
| RATIONAL_COUPLING_KEEP | 4 | EC-1, EC-2, EC-3, EC-4（一致性耦合论证成立，EC-3 附带 overhead 论证） |
| UNDER_SPECIFIED_CANDIDATE_KEEP | 3 | EA-1, EA-3（**捆绑步**）, EB-2（fallback 无 step 的泛化形态） |
| MISSED_RECONSIDERATION | 1 主导 + 5 成分 | EB-3 主导；EA-1/EA-2/EC-1/EC-2 的 tests/goldens 尾巴同因 |
| MODEL_IGNORED_CLEAR_CANDIDATE | 1 | EA-2（对 disposition 请求零回应，mutation 静默落 kept） |
| OPPORTUNITY_NEVER_FIRED | 1 | EB-1（无 plan 无 mutation，两个触发器都够不着；run 级退化） |
| DELEGATED（对照） | 2 | EA-4, EB-4 |

## 三个硬事实（修复设计的地基）

### 事实 1：候选边界 = 模型自己的 step 边界；捆绑步杀死候选

EA-1/EA-3 的 plan 把「`dedup.rs` 新文件」和「`main.rs`/`mod.rs` 注册胶水」写进**同一个
step**，整步被正确判为共享文件耦合 → KEEP。EA-4 的 plan 恰好把注册拆成独立 step，
dedup 当场被 DELEGATE 且 verifier PASS。同一任务、同一 offer 机制，结果只取决于
step 怎么切。offer 逐 step 询问但从不提示"可以把一步拆开处置"。

### 事实 2：offer 时机 = 依赖最稠密的时刻；解除后没有第二次机会

offer 与 plan 注册同轮（r13–52），此时 tests/goldens/docs/fixtures 全部被
未完成的核心实现阻塞——EB-3 四个 KEEP 理由**全部**是依赖论证，当时完全理性。
run 跑到 100+ 轮，核心在中段落地，尾巴变成有界、独立、可验证的工作项，
但 `DelegationDecisionPoint` 是 one-shot（"you will not be asked again"），
`note_plan_registered` 之后的 plan 进度全部被丢弃。
EB-4 是活证：模型自己预告"先算出期望输出再委派 tests"，并在 r~80 真的补了
委派（kept@489 之后 delegated@898）——**延迟委派是这套 runtime 支持且有价值的，
只是没有任何机制在依赖解除时把这个选项重新摆上桌**。

### 事实 3：EC 簇是理性 KEEP，不是缺陷

hyperfine 三个 exporter 的一致性耦合论证（浮点格式对齐 JSON、escaping 语义、
共享 serde 视图）在 4/4 run 中独立复现。修复不应以扭转 EC 为目标——
门槛的"≥2 distinct tasks"应来自 A（拆捆绑）与 B（依赖解除后重考虑）。

## 主根因

**DECISION_TIMING + MISSED_RECONSIDERATION**（事实 2），
次因 **UNDER_SPECIFIED_CANDIDATE（捆绑步无拆分提示）**（事实 1）。
不是 capability visibility，不是 opportunity detection（10/12 触发），
不是 worker execution/safety（2/2 委派 PASS），大体不是 prompt 措辞。

## 对修复的直接约束

1. 保留现有 plan 注册 offer 与 KEEP 判据原样（6/6 KEEP 对照健康，不许扰动）。
2. 加**事件驱动的一次性 reconsideration**：kept 之后，当 plan 完成数增长且
   剩余 open steps ≥2 且指纹与 offer 时不同 → 再给一次逐步处置机会，
   附带父已编辑文件清单（`EvidenceLedger.mutations`，权威事实）作独立性判断依据。
   同一指纹永不重复；每 epoch 至多一次；无周期性提醒。
3. offer 文案加一条通用拆分指引（自包含新文件 + 共享胶水的捆绑步可拆开处置），
   并修正"不会再问"的表述为条件式。
4. 不做：关键词/任务名规则、强制委派、第二调度器、EB-1 类探索退化 run 的抢救
  （那是 run 级失败，不在本窄修范围）。
