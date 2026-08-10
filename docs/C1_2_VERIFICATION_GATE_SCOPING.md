# C1.2 — Verification Gate Scoping Integrity

日期：2026-08-08。分支 `feat/coding-real-task-completion-c1`。模型 `deepseek/deepseek-v4-flash`。
修复范围严格限制在 verification planning/scoping；repair、Verified 判定语义、sandbox 策略均未改动。

## 1. Root Cause

审计结论与 prompt 假设一致，并补出**第三处**改写点：

| # | 位置 | 行为 |
| --- | --- | --- |
| 1 | `VerificationCommand`（plan.rs:24） | 无 provenance 字段。`plan_from_verify`（discover.rs:47，读 `.leveler/config.yaml`）与 `VerificationCommand::new`（`for_languages` / `node_plan` / `nested_rust_plan` 的推断计划）产出**完全同构**的结构，下游无从区分 |
| 2 | `Verifier::run_check`（verifier.rs） | 对所有 command 无条件 `scope_args(...)` → 显式 `go build ./...` 被改写成 `go build ./.acceptance-work/...` |
| 3 | `Verifier::run_check` | 对所有 command 无条件 `with_no_fail_fast(...)` → **prompt 未列出的第二处 args 改写**：显式 `cargo test` 会被插入 `--no-fail-fast` |
| 4 | `VerificationPlan::scope_gates_to_changes`（plan.rs:175） | 对所有 command 按 `is_build_relevant(modified_files)` 把 Build/Test/Lint 降为 `gating=false` → 显式 docs-lint 在 markdown-only 改动上被取消 |

P1 的燃料（`modified_files` 语义，审计确认）：drive.rs:1631 / 1928 是**仅追加的并集**，来源为编辑工具 metadata + `run_command` 的 `WorkspaceSnapshot::changed_since` git 快照差异；语义是 **"ever touched"，不是 final diff，且从不剔除已消失的路径**。因此窄化目标可以指向：
- A. 运行期 create→delete 的临时目录（无持久改动）
- B. 真实删除的源码/包（有持久改动）
- C. 现存改动文件

A 与 B 都会产生"当前不存在"的包目标，但语义完全不同 —— 不能用 `!path.exists()` 一刀切过滤。

## 2. Invariants

```
EXPLICIT VERIFICATION IS AUTHORITATIVE.
```

- **Explicit（用户在 `.leveler/config.yaml` 声明）→ Exact**：`program` + `args` 原样执行，不窄化、不加 flag；声明的 gating 语义不被启发式取消。
- **Inferred（系统从 manifest 推断）→ Auto**：仍可按真实 change scope 做 targeted → package → full 的窄化与降级（BB2 证明这有价值），但**窄化后的目标必须真实存在**；无法安全窄化时回退到更宽的有效命令，绝不跳过验证。

安全优先级固定为：**valid broader gate > invalid narrowed gate > (从不) skip**。

## 3. Production Changes（逐文件）

| 文件 | 改动 |
| --- | --- |
| `crates/leveler-verifier/src/plan.rs` | 新增 `ScopePolicy { Auto, Exact }`（`Default = Auto`）；`VerificationCommand` 增 `scope_policy` 字段（`#[serde(default)]`）；`VerificationCommand::new`（推断构造器，唯一入口）标 `Auto`；`scope_gates_to_changes` 跳过 `Exact` 命令 |
| `crates/leveler-verifier/src/discover.rs` | `plan_from_verify`（`verify:` section 的唯一读取点）标 `Exact` |
| `crates/leveler-verifier/src/verifier.rs` | 新增 `effective_args(command, modified_files, root)`：`Exact` 直接返回原 args；`Auto` 才走 `scope_args` + `with_no_fail_fast`。`run_check` 改为调用它。`scope_args` 增 `workspace_root` 参数，**任一候选目录不存在即整体回退到原 args** |
| `crates/leveler-engine/{baseline,engine}.rs`、`tests/direct_test.rs` | 仅测试字面量补字段（`Default::default()`），无行为改动 |

provenance **只在构造点记录**：不按命令名猜、不按 program 字符串猜、不按语言/provider 硬编码。

## 4. Deterministic Tests（边界全覆盖）

| 编号 | 测试 | 断言 |
| --- | --- | --- |
| A | `explicit_args_are_never_rewritten` | Exact `go test ./...` + 改 `app/foo.go` → 仍 `./...` |
| A' | `explicit_cargo_test_keeps_its_exact_arguments` | Exact `cargo test --workspace` → **不注入 `--no-fail-fast`** |
| B | `inferred_args_are_narrowed_to_the_changed_package` | Auto `go test ./...` + 改 `app/foo.go`（app/ 存在）→ `./app/...` |
| C | `explicit_gate_survives_an_inert_change` | Exact Test gate + 仅改 `README.md` → 仍 `gating=true` |
| D | `inferred_gate_is_still_downgraded_on_an_inert_change` | 推断 Rust plan + 仅改 `README.md` → 降级（既有安全优化保留） |
| E | `a_vanished_target_falls_back_to_the_broader_command` | Auto + `.acceptance-work/duration/format.go`（目录已消失）→ 回退 `./...`，不生成 `./.acceptance-work/...` |
| F | `scope_falls_back_to_full_on_root_change` | 根级改动 → 不窄化 |
| G | `source_deletion_still_verifies` | 删包内文件（包仍在）→ 正常窄化；删整个包 → **加宽而非跳过** |
| — | `a_command_without_scope_policy_defaults_to_auto` | 旧序列化命令反序列化后仍是 Auto（向后兼容） |
| — | `an_explicit_verify_section_is_authoritative` | `plan_for_repo` 读到 `verify:` → `Exact` + program/args/gating 原样 |
| — | `an_inferred_plan_stays_scopeable` | 推断计划全部 `Auto` |

`cargo test -p leveler-verifier --lib`：**78 passed / 0 failed**。

## 5. Real-task Before / After

| Case | Before | After | 证据 |
| --- | --- | --- | --- |
| **EDIT1** | hidden acceptance PASS，显式 docs gate 被降级 → **CompletedUnverified** | 显式 gate **真实执行且 gating** → `verification_check name=test status=passed` → **Completed / Verified** ✅ | 事件 seq 97-98；工作区 `docs/release-checklist.md` 已改名到位 |
| **R2** | repair 成功（test 转绿）但 build 撞 `./.acceptance-work/...` 死路径 → **CompletedUnverified** | 三个显式 gate（format/build/test）**全部原样执行并通过**，无 lstat 失败 → **Completed / Verified** ✅ | 事件 seq 64-67（首跑）、68-71（复跑） |
| **WV1** | CompletedUnverified，PASS | **不变**：CompletedUnverified，PASS ✅ | — |
| **BB1** | CompletedUnverified，PASS | **不变**：CompletedUnverified，PASS ✅ | — |
| **BB2** | Verified，PASS（推断计划窄化到 `./app/...`） | **不变**：Verified，PASS ✅ | 推断窄化能力未被误伤 |

## 6. Repair Integrity

修复前 R2 已实测完整链：`test FAIL(Format(3661)="61m1s") → RepairStarted attempt=1 → repair edit → reverify test PASS`，但同一次 reverify 的 build 假失败把终态压成 CompletedUnverified —— 即**"repair 成功后的验证被 scoping fake failure 毁掉"**，正是 C1.2 要消除的那一环。

C1.2 后两次 R2 真模型运行**均未触发 repair**：模型在主 turn 内自行运行了 acceptance 脚本、自己发现并修掉隐藏契约，门禁首验即全绿（8 / 9 步）。诊断复跑额度已用尽。因此：

- **可断言（有证据）**：destroy 那一环已消除 —— 显式命令不再被改写（A/A' 测试 + 两次真跑事件），推断命令不会再生成死路径目标（E/G 测试）。
- **不可断言（本轮无新证据）**："repair 成功 → Verified" 的**端到端真模型**复现。repair 主链代码零改动（`DIRECT_REPAIR_ATTEMPTS`、repair prompt/context/engine flow 均未触碰），修复面与 repair 逻辑不相交。

## 7. Safety Semantics

未放宽任何判定：`VerificationReport::verdict()` 一字未动；无 gate 仍 `CompletedUnverified`（WV1）；pre-existing red 仍不升级（BB1）。C1.2 的效果是**"让本该运行的 gate 跑正确的命令和范围"**，不是"让更多任务变 Verified"—— EDIT1 转 Verified 是因为它的 gate 这次真的跑了并且真的通过了。

## 8. P2 — Verification Sandbox TMPDIR Policy：**DEFERRED**

审计确认与 scoping **不是同一根因**：P2 出自 `process_request_for_verify_check` 的写约束策略（verify 检查无法在 `$TMPDIR` 下 `mkdir`），与 provenance / 窄化逻辑无交集。本轮未触碰 `CommandRunner` sandbox、写约束、TMPDIR 策略。后续单独决策。

## 9. Regression

| 项 | 结果 |
| --- | --- |
| `cargo fmt --check` | ✅ |
| `cargo check --workspace --all-targets` | ✅ 0 error |
| `cargo test --workspace --no-fail-fast` | ✅ 0 failed |
| Real-model：EDIT1 / R2 / WV1 / BB1 / BB2 | ✅ 5/5 PASS |
| smoke | ✅ 3/3 |
| regression/recovery | ✅ 5/5，recovery rate 100% |

未重跑 ripgrep 与 full 49（按 §13 不需要）。

## 10. Result

**C1.2: PASS**

- Explicit authority 建立并被测试锁定；Inferred 窄化能力完整保留。
- 两个 product failure（P1 死路径窄化、P3 显式 gate 降级）在真任务上双双转 Verified。
- 三条安全语义（WV1 / BB1 / BB2）逐项不变。
- P2 明确 DEFERRED。

不开始下一个 production capability。
