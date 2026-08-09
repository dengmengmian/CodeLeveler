# C2 — Completion Evidence: Architecture Audit

日期：2026-08-09。分支 `feat/coding-context-efficiency-c2`。**纯审计，未改动生产代码。**

---

## 1. 追踪结果：谁允许 "generic green" 变成 "task done"

```
Task objective
  ↓
mutation（drive loop 收集 modified_files）
  ↓
gate_plan(spec)                     engine.rs:1592
  ├─ 显式 .leveler/config.yaml verify  → 原样采用（C1.2 权威）
  └─ 否则 discover::plan_for_repo()    → 按仓库类型推断（build / test / lint）
  ↓
Verifier 执行 → VerificationReport
  ↓
finalize_task_outcome(report, ExpectedEvidence)      verifier/outcome.rs:62
  ↓
map_completion_verdict → TaskOutcome::Verified
```

**`finalize_task_outcome` 的全部逻辑：**

```rust
match health.verdict() {
    Verdict::Failed        => CompletionVerdict::Failed,
    Verdict::Unverified(_) => CompletionVerdict::CompletedUnverified,
    Verdict::Verified if expected.needs_mutation && !expected.has_mutation
                           => CompletionVerdict::CompletedUnverified,
    Verdict::Verified      => CompletionVerdict::Verified,
}
```

**"检查全绿"与 "Verified" 之间只隔着一个条件：这个任务看起来是否需要改文件，以及是否真的改了文件。**

`needs_mutation` 来自 `direct_needs_mutation(goal, delivery_gate)` = `delivery_gate || task_looks_like_implementation(goal)` —— 一个基于目标文本的启发式，**与所执行的检查是否与任务有关无关**。

**没有任何一处询问：通过的那些检查，是否与用户要求的行为有关。**

---

## 2. §14 七问逐条回答

| # | 问题 | 答案 |
| --- | --- | --- |
| **A** | 是否区分 `verification succeeded` 与 `requested behavior demonstrated`？ | **不区分。** 系统里只有前者。后者没有对应概念 |
| **B** | verification authority 证明什么？ | **只证明 command success。** `VerificationReport` 记录每条检查的 `CheckStatus`，没有"这条检查覆盖了什么需求"的字段 |
| **C** | 谁选择 verification command？ | 显式 `.leveler/config.yaml`（权威，C1.2）；否则 `discover::plan_for_repo()` 按仓库类型推断（Go → `go build` + `go test ./...`） |
| **D** | 模型能否自选弱验证再自宣完成？ | **不能直接选**（计划由 harness 决定），但**效果等价**：推断出的通用计划对一个**新增行为**天然无法证伪，模型只要让它绿即可 |
| **E** | Harness 是否知道某 command 是 generic 还是 task-targeted？ | **知道类别，不知道针对性。** `CheckKind` 有 `Format/Build/Test/Lint`；`ScopePolicy` 有 `Auto/Exact`。**没有任何一处表达"这条检查验证的是本次任务要求的行为"** |
| **F** | 是否已存在 explicit vs inferred 信息可复用？ | **存在。** C1.2 的 `ScopePolicy::Exact`（用户显式声明）与 `Auto`（推断）是现成的强/弱证据区分 |
| **G** | completion transition 是否只看 verification_status？ | **是**，外加 `needs_mutation ∧ has_mutation`。**不看相关性** |

---

## 3. 最重要的发现：这是一个有实测依据的刻意决定

`verifier/outcome.rs` 的模块文档原文：

> **"Only facts decide, and there is exactly one fact: the project's own gating checks, run against the edited tree."**
>
> "Acceptance criteria are deliberately *not* an input here. They are the model's restatement of the goal plus a check command it invented on the spot — a guess, whichever way that command exits. Two measured runs settled it:
>
> - A correct, fully green turn reported 有改动但缺少系统级验收背书 only because the model never produced a provable criterion.
> - A correct `isRequired()` added to an ESM module passed the project's suite (1377 pass / 0 fail) but was marked 验收未通过：AC2 because the model's own one-liner used CommonJS `require()` on an ESM file and threw.
>
> In both the harness overruled a maintained test suite with a throwaway guess."

**把模型自拟的验收标准接进判定链，已经被实测证伪过一次** —— 它制造了 false negative：正确的工作被一句写错的一次性脚本判死。

而 N3 / N4 是**同一个决定的反方向代价**：项目自身的 gating 检查对一个**新增行为**没有证伪能力，于是 green 就等于 done，产生 false positive。

### 这对 Completion Evidence Gate 的直接约束

**不能简单地把"模型声明的验收标准"重新接回判定链** —— 那条路已经测过并被否决。任何新机制必须结构上不同：

| 已被否决 | 必须改为 |
| --- | --- |
| 判定"模型自拟的标准是否通过" | 判定"是否存在**任何已执行的证据**真的触及了本次改动" |
| 让模型的一次性脚本能**推翻**维护良好的测试套件 | 让缺失针对性证据**阻止完成**，但绝不把绿色检查判成失败 |

即：新机制的方向是 **提高完成门槛**，不是 **降低验证结论**。§25 的措辞"Verification passed, completion evidence insufficient"正是这个区分。

---

## 4. 当前可复用的观察点

| 已有 | 可用于 |
| --- | --- |
| `CheckKind::{Format,Build,Test,Lint}` | 区分 generic 类别 |
| `ScopePolicy::{Exact,Auto}` | 区分显式用户验收（强证据）与推断计划（弱证据） |
| `VerificationReport.checks[].evidence` | 每条检查的实际输出 |
| drive loop 的 `modified_files` + 工具调用时序 | 证据是否发生在改动**之后**（freshness） |
| `tool_call_started/finished` 事件 | 模型自行运行的命令（probe / CLI 调用）及其结果 |

| 缺失 | 影响 |
| --- | --- |
| "这条检查针对本次需求"的表达 | 无法区分 targeted 与 generic |
| 需求 → 证据的映射 | 无法回答 §5 的核心问题 |
| 证据新鲜度判定 | 证据通过后再改代码，无人察觉 |
| `PlanStep.step` 是自由文本 | 无法从计划推出"要证明什么" |

---

## 5. N3 / N4 在这条链路上的确切位置

两者都是：

```
gate_plan → go build + go test ./...   （推断计划，通用）
  ↓ 全绿
Verdict::Verified
  ↓ needs_mutation=true, has_mutation=true
CompletionVerdict::Verified
  ↓
Completed
```

**N4**：任务要求新增 `wrote N records` 输出。仓库里没有任何既有测试覆盖它，`go build` 与 `go test ./...` 对它**没有证伪能力**。Agent 从未实现它，链路照常判 Verified。

**N3**：任务要求 report 层忽略 invalid 记录。既有测试不覆盖 `Distinct`。Agent 第二个补丁把 `Distinct` 的守卫去掉后，`go test ./...` 依然全绿。

**共同点：通过的检查与任务要求的行为之间没有交集，而系统无法察觉这一点。**

---

## 6. 本审计未做的事

- 未改动任何生产代码。
- 未修改 verifier 语义、C1.2 的显式验证权威、或 `CompletionVerdict` 的取值。
- 未实现 Completion Evidence Gate —— 按 PHASE A 的门槛，先等 N3/N4 control variance。
