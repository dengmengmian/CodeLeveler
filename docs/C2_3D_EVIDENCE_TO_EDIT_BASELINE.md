# C2.3D — Evidence-to-Edit Commitment: Architecture Audit

日期：2026-08-09。分支 `feat/coding-context-efficiency-c2`，baseline product state = C2.3C（`0c664a3`，`base.md` 中 `Code navigation` 计数为 0）。

**纯审计。未改动任何生产代码。** 按 §7 要求，先回答"Harness 现在到底能观察到什么"，再决定是否以及如何实现机制。

---

## 1. Problem Statement

C2.3C 的证据把问题定死在一个很窄的位置：

| 能力 | C2.3C baseline |
| --- | --- |
| Target Recall | **8/8** |
| Required Impact Path Touch Recall | **8/8** |
| Required Impact Path **FullyRead** Recall | **8/8**（19/19 条路径 FULL，PARTIAL 0，MISS 0） |
| MissedImpactPaths | **0** |
| ForbiddenPathsEdited | **0** |
| **通过率** | **6/8** |

**两次失败都不是找不到、找错或找不全。** 它们发生在证据已经充分之后。

---

## 2. N3 Failure Timeline（逐 patch 核实）

| 时点 | 事件 |
| --- | --- |
| r2 | 定位 `internal/report/summary.go` |
| — | 完整读取 `summary.go`(1-52) 与 `aggregate.go`(1-29)，**两条 required impact path 全中且全文读完** |
| r6 | **产出完全正确的补丁**：`Observe` 与 `Distinct` 各加 `if !record.Valid { continue }` —— 正是隐藏验收要的答案 |
| 之后 | **把两处正确修改全部撤销** |
| — | 转而修改 `internal/ingest/reader.go`（错误层级） |
| — | 引擎自身 verification **PASS**（`repair=1(pass)`，终态 `verified`） |
| 终 | 独立隐藏验收 **FAIL** |

导航侧指标：45 次导航调用、信息增量仅 **60%**、**18 次 DUPLICATE**（全 suite 最高）。

**形态：Correct evidence → Correct patch → Revert → Worse final patch。**

## 3. N6 Failure Timeline

| 时点 | 事件 |
| --- | --- |
| r3 | 定位 `internal/ingest/decoder.go` |
| — | **完整读取 1–1024 行，未截断**（文件 30,221 字节 < `MAX_BYTES` 262,144，物理上不可截断） |
| r10 | 首次编辑 —— 但对象是 `cmd/depthcheck/main.go`（自建验证程序） |
| r10–r60 | 持续 scratch / probe：`internal/ingest/scratch_test.go`、`scratch2_test.go`、`verify_depth_test.go` |
| r60 | 预算耗尽。**`decoder.go` 一次都没被编辑** |

导航侧指标：33 次导航调用、信息增量 **94%**（几乎每次都带来新信息）。

**形态：Correct target → Full evidence → 无限有价值的研究 → Target 从未被修改。**

**注意**：94% 的信息增量说明这**不是** `NAV_OVER_EXPLORE`。新信息确实在产生，只是它已经不足以改变主修改决策。

---

## 4. Observable Signals —— §7 A–I 逐条回答

| # | 问题 | 现状 | 依据 |
| --- | --- | --- | --- |
| **A** | Harness 知道 Agent 已形成 plan？ | ✅ **能** | `PlanState.steps`（`leveler-lifecycle/src/plan.rs:39`）在 drive loop 内有内存镜像，并持久化为 `PlanUpdated` 事件 |
| **B** | plan 中是否存在 actionable mutation step？ | ❌ **不能** | `PlanStep { step: String, status: String, .. }` —— 步骤是**自由文本**。判断"这一步是要改文件 X"需要解析散文。**这是首要的 `PLAN_SIGNAL_INSUFFICIENT` 风险** |
| **C** | 某文件刚刚被成功修改？ | ✅ **能（仅 in-loop）** | `collect_modified(&metadata, &mut modified_files)`（`dispatch.rs:24`）—— 每次 dispatch 后从工具 metadata 收集 `modified_files`。`apply_patch` 与 `replace` 都发布该字段 |
| — | ...持久化呢？ | ⚠️ **只有聚合值** | `modified_files` 在事件日志中**只出现在 `TurnFinished`**（`event.rs:155`），是整轮的汇总，不是逐次修改 |
| **D** | 修改前后的 fingerprint / diff？ | ❌ **不能** | `FileStateTracker` 是 `Mutex<HashMap<String, u64>>`，**每路径只存最新指纹，写入即覆盖**。编辑工具在写完后立刻 `record`（`apply_patch.rs:474`、`replace.rs:170`），**编辑前的值就此丢失**。也没有保留任何 diff |
| — | workspace snapshot 能补吗？ | ❌ **不能** | `WorkspaceSnapshotCreated` 只由 `run_command` 与 `task_control` 产生（`run_command.rs:420`、`task_control.rs:154`），**编辑工具不产生 snapshot** |
| **E** | Agent 是否把文件恢复到了 edit 前版本？ | ❌ **不能** | 直接依赖 D。当前无任何数据可以判定 full revert / partial revert / target shift |
| **F** | verification 是 PASS / FAIL / UNKNOWN？ | ✅ **能** | `VerificationStarted` / `VerificationCheck` / `VerificationFinished` 事件齐备；`verify_calls` 集合在信号采集器中已被使用 |
| **G** | verification failure 是否带来新 diagnostic？ | ✅ **能** | 验证证据（program/args/输出）随事件持久化（C1.2 起） |
| **H** | 成功 mutation 之后又发生了哪些 evidence-producing 调用？ | ✅ **能（in-loop）** | 工具调用序列 + `modified_files` 的时序在 drive loop 内完全可见 |
| **I** | 已存在"planned action → actual mutation"的追踪？ | ❌ **不存在** | 没有任何机制把 plan step 与实际发生的 mutation 关联起来 |

### 4.1 能力缺口小结

| 需要 | 现状 |
| --- | --- |
| 知道"已形成修改决策" | **部分** —— 有 plan，但步骤是散文（B） |
| 知道"某次修改发生了" | **有**（C，in-loop） |
| 知道"这次修改被撤销了" | **没有**（D/E） |
| 知道"计划要改的东西一直没被改" | **没有**（B + I） |
| 知道"验证通过了" | **有**（F） |
| 知道"出现了新的矛盾证据" | **有**（G） |

**N3 需要的是 D/E，N6 需要的是 B/I。这是两个不同的缺口。**

---

## 5. Why Prompt-only Is Insufficient

C2.3B 已用真实 A/B 证伪：把 Search First / Progressive Read / Impact Surface / Convergence 四条纪律写进 `base.md` 后，正确性持平，但轮次 +17%、token +28%、yq 信息增量 91%→81%、重复确认 0→6、首次编辑 r19→r29。

**其中收敛那一条恰恰是为 N3/N6 这类问题写的，而且是与搜索纪律同批写入的** —— §34 预警过的失败模式仍然发生了。原因很直接：**"证据是否还在变化"不是模型能可靠自检的东西**。

因此 C2.3D 的机制必须 Harness-observable、Harness-stateful、可测量，而不是再一次让模型自我评估。

---

## 6. Candidate Mechanism（仅候选，待 variance 结果决定是否实现）

按 §44"先找共享的最小抽象"，两个失败共享的骨架是：

```
Edit Hypothesis 形成
      ↓
（N6 断在这里：从未执行）
      ↓
Mutation 发生
      ↓
（N3 断在这里：被无矛盾地撤销）
      ↓
Verification
      ↓
Final Patch
```

共享抽象 = **一个由证据支撑的编辑承诺，其推翻需要新的矛盾证据**（§24）。

### 6.1 最小实现路径（按 Audit 结论）

| 缺口 | 最小补法 | 代价 |
| --- | --- | --- |
| **D/E**（revert 检测） | drive loop 在 dispatch 编辑工具**之前**，为该调用将要触碰的路径记录一份 pre-edit 指纹；成功后记录 post-edit 指纹。二者构成 per-turn 的 `(path → 首次编辑前指纹)`。之后任何一次修改若使该路径指纹回到首次编辑前的值，即为 **full revert** | 小：复用现有 `ContentFingerprint`；只在编辑路径上多一次读取 |
| **B/I**（actionable mutation step） | **不解析散文**。改用可观察的替代信号：plan 存在 + 已发生过针对某路径的成功读取 + 持续的非 mutation 动作。若 Audit 证明不可靠，则退到只基于 "plan 存在但整轮零 mutation" 的更弱信号（§22 允许） | 中：需要谨慎定义，避免变成 round 计数 |

### 6.2 明确不做

- 不新增 `commit_edit_hypothesis` 模型工具（§11：除非现有 plan 完全无法承载）。
- 不禁止 revert（§5）。
- 不强制 `ToolChoice`、不裁剪工具表（§12、§21 —— C2.3A invariant）。
- 不让 Harness 判断补丁的业务正确性（§16）。
- 不使用任何 hidden eval metadata（§6）。

---

## 7. 已知的实验前提风险

| 风险 | 说明 |
| --- | --- |
| **单次 baseline** | C2.3C 每个 case 只跑了一次。N3/N6 的失败是否稳定复现**尚未证实** —— 按 §8，`N3×3` 与 `N6×3` 的 control variance 正在运行，结果出来前不实现任何机制 |
| **PLAN_SIGNAL_INSUFFICIENT** | plan step 是自由文本，B/I 两个缺口可能无法在不解析散文的前提下补上。若如此，N6 方向可能只能拿到很弱的信号 |
| **COMMITMENT_TOO_EARLY** | 机制可能让 Agent 过早锁定错误 hypothesis。§35 要求重点观察 N2/N5/N7/N8；任一 target/impact recall 下降或 distractor edit 增加即 FAIL |
| **会话选择污染** | variance 运行会生成新的 N3/N6 会话目录；离线分析脚本按 mtime 取最新，做对照分析时必须显式钉住会话，否则会读到正在跑的那次 |

---

## 8. 本审计未做的事

- 未改动任何生产代码。
- 未修改任何 eval case、fixture、hidden acceptance。
- 未实现任何 commitment 机制 —— 按 §8，等 variance 结果。
