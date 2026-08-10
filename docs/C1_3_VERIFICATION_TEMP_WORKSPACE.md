# C1.3 — Verification Sandbox Temporary Workspace

日期：2026-08-08。分支 `feat/coding-real-task-completion-c1`。模型 `deepseek/deepseek-v4-flash`。

**结论前置**：审计推翻了 P2 的原假设。验证沙箱**已经**为每个 check 提供了工作区外的专属可写临时空间并导出 `TMPDIR/TMP/TEMP` —— §2 的不变量清单逐条已满足。真正的失败源是 **macOS `/usr/bin/mktemp` 完全忽略 `$TMPDIR`**，硬打到 Darwin 每用户共享临时目录，被 seatbelt（正确地）拒绝。修它的唯一办法正是 §3 明令禁止的宽授权，因此本轮**不改生产代码**，改为：用确定性测试锁住已成立的不变量与该平台限制，并把 R1/R2 从 workspace-local 的权宜写法改回真正的临时空间用法。

## 1. Root Cause

原假设："verify script 无法使用 $TMPDIR"。**实测否定**。

探针（走生产入口 `process_request_for_verify_check` + `CommandRunner`）打印子进程实际环境：

```
TMPDIR=[/private/var/folders/.../home/codeleveler-sandbox-4hmVk9/tmp]      ← 私有 scratch，已正确注入
mktemp: mkdtemp failed on /var/folders/g0/.../T/tmp.IsoOofva6y: Operation not permitted
                          ^^^^^^^^^^^^^^^^^^^^^ 系统共享临时目录，不是 TMPDIR
```

宿主机直接验证（无沙箱）：

| 命令 | 结果 |
| --- | --- |
| `TMPDIR=/tmp/c13probe mktemp -d` | `/var/folders/.../T/tmp.HzxOB7fRul` ← **忽略 TMPDIR** |
| `TMPDIR=/tmp/c13probe mktemp -d -t leveler` | `/var/folders/.../T/leveler.3sIsqLQGjx` ← **-t 也忽略** |
| `TMPDIR=/tmp/c13probe python3 -c 'tempfile.mkdtemp()'` | `/tmp/c13probe/tmp5_5nf96e` ← 尊重 TMPDIR |

即：macOS 的 BSD `mktemp(1)` 用 `confstr(_CS_DARWIN_USER_TEMP_DIR)` 而非环境变量（Linux 的 GNU coreutils 尊重 TMPDIR，此问题不存在）。语言级 temp API、以及带显式模板的 `mktemp -d "$TMPDIR/x.XXXXXX"` 都正常。

**所以这不是"沙箱没给临时空间"，而是"某些工具不走 TMPDIR"。**

## 2. Current Sandbox Model（§1 五问的实读答案）

1. **verify check 允许写哪些目录**：`writable_roots()`（command.rs:318）= workspace_root + per-command private scratch + Leveler 自有 tool cache（cargo/go/npm 等）。closed-by-default，其余一律拒。
2. **为何 workspace 可写而系统 TMPDIR 不可写**：seatbelt 基础策略 `(deny default)`，只有上述 roots 以 `-DWRITABLE_ROOT_i` 参数进 `(allow file-write* (subpath ...))`。系统共享临时目录不在其中——**这是刻意决定**，`writable_roots_exclude_shared_temp_and_host_tool_directories` 与 "shared temp tree must stay read-only" 两条既有测试正锁着它。
3. **ProcessRequest 是否支持 additional writable roots**：支持但非公开旋钮——`prepare_sandbox_paths()` 在 `write_root.is_some()` 时自动创建 scratch，`sandbox_command()` 把它并入 writable roots。无需新增平行机制。
4. **TMPDIR/TMP/TEMP 来自哪里**：`apply_sandbox_environment()`（host_cache.rs:522）在 `apply_common_command_env` **之后**覆写三者为 `<scratch>/tmp`，同时把 GOCACHE/GOPATH/CARGO_HOME 等重定向到 tool cache。
5. **是否已有 per-command temp seam**：**有**。`SandboxPaths.scratch: tempfile::TempDir`，随请求生命周期 drop 即删除；scratch 建在 `LEVELER_HOME`（或 `~/.cache/codeleveler-private`）下的稳定 owner 内，刻意不用 TMPDIR（避免被 workspace 污染）。

## 3. Temp Root Design

§2 要求的不变量与现状逐条对照（全部由本轮新增测试锁定）：

| 不变量 | 现状 | 证据 |
| --- | --- | --- |
| writable | ✅ | `verify_check_gets_a_dedicated_temp_root_outside_the_workspace` |
| 经 TMPDIR/TMP/TEMP 可用 | ✅ 三者同值 | 同上（断言三行一致） |
| 不属于用户 workspace | ✅ | 同上（`!temp_root.starts_with(workspace)`） |
| 不进 modified_files / git diff / mutation evidence | ✅ 在工作区外，快照 diff 看不见 | 同上（检查完毕工作区为空）+ 真跑工作区 `git status` 仅目标源文件 |
| 完成后可清理 | ✅ per-check，用完即删 | `verify_check_scratch_is_per_check_and_removed_afterwards` |

`~/.leveler/` 下确有 2 个 7月27 的残留 scratch —— 来自被 kill 的进程，属 best-effort 清理的预期行为，非正常路径泄漏。

**结论：不需要新建 temp root 机制**；§4 推荐方向已在产品内实现，且比方案更严格（scratch 不放在系统 temp 下，避免被 workspace 反向污染）。

## 4. Security Boundary

`verify_check_cannot_write_outside_its_granted_roots`：工作区与本 check 专属 temp root 之外的路径（含用户目录、系统共享 temp）写入仍被拒且文件不存在。临时空间是**授权**，不是缺口。

**未触碰**：`CommandRunner` sandbox、写约束策略、seatbelt profile、TMPDIR policy、ToolHost 权限模型 —— 一行未改。

## 5. Production Changes

**无。** 本轮生产代码零改动（`git status` 仅 `crates/leveler-execution/src/command.rs` 的 `#[cfg(test)]` 测试模块 + 两个 eval case yaml）。

理由：审计证明设计已正确；唯一残留失败需要 §3 明禁的宽授权。

## 6. Deterministic Tests

| 编号 | 测试 | 覆盖 |
| --- | --- | --- |
| A/B/C/D/E | `verify_check_gets_a_dedicated_temp_root_outside_the_workspace` | 可创建临时目录；TMPDIR/TMP/TEMP 指向同一专属 root；root 在工作区外；用完工作区无痕（→ 不进 diff / modified_files） |
| G/H | `verify_check_scratch_is_per_check_and_removed_afterwards` | check 结束即清理；两次 check 各自独立、不共享脏状态 |
| F | `verify_check_cannot_write_outside_its_granted_roots` | 越权路径仍被拒 |
| — | `macos_mktemp_without_a_template_bypasses_tmpdir` | **锁住平台限制**：裸 `mktemp -d` 越过 TMPDIR 打向共享 temp 并被拒；同一沙箱下 `mktemp -d "$TMPDIR/x.XXXXXX"` 成功 |

I（cancellation/timeout 清理）：`SandboxPaths` 的 `TempDir` 随请求 drop，超时/取消路径同样触发 drop —— 沿用既有所有权语义，未新增代码故未新增测试。

## 7. R1/R2 mktemp Evidence

acceptance 脚本从 workspace-local 的 `.acceptance-work` 改回正常临时空间用法：

```bash
w="$(mktemp -d "${TMPDIR:-/tmp}/leveler-acceptance.XXXXXX")"
trap 'rm -rf "$w"' EXIT
```

（显式模板 + trap 清理：可移植、沙箱内外一致，且修掉了此前失败时残留 scratch 的毛病——那正是 C1.2 里 P1 的燃料。裸 `mktemp -d` 在 macOS 沙箱下不可用，原因见 §1，已由测试钉住。）

真模型结果：

| Case | 结果 | 验证链 |
| --- | --- | --- |
| R1 | ✓ **Completed / Verified** | build PASS → test PASS，**无 `Operation not permitted`** |
| R2 | ✓ **Completed / Verified**，`repair=1(pass)` | seq 44-46 format/build PASS + **test FAIL**（`Format(3661)="61m1s"; want "1h1m1s"`）→ 47 VerificationFailed → 48 **RepairStarted** → repair turn 编辑 → 98-100 三门全 PASS → 101 **passed=true** |

**R2 顺带补上了 C1.2 报告里明确标注"本轮无新证据"的那一项**：`VerificationFailed → RepairStarted → repair → reverify → **Verified**` 端到端首次以 Verified 收尾。

## 8. Workspace Pollution Check

两个 kept workspace 的 `git status --short`：

```
r1: M percent/percent.go        （无其他文件、无残留临时目录）
r2: M duration/format.go        （同上）
```

验证期的临时读写全部发生在工作区外，未进入 diff、未进入 modified_files，也就不会再反过来影响 C1.2 的门禁窄化。

## 9. C1.2 Regression

| Case | 期望 | 实测 |
| --- | --- | --- |
| EDIT1 | 显式 Exact 门禁执行 → Verified | ✓ Completed |
| R2 | 显式门禁原样执行 → Verified | ✓ Completed |
| WV1 | CompletedUnverified | ✓ 不变 |
| BB1 | CompletedUnverified | ✓ 不变 |
| BB2 | 推断窄化 → Verified | ✓ 不变 |

Explicit Exact authority、Inferred narrowing、CompletedUnverified 安全语义三者均未退化。

## 10. Regression

`cargo fmt --check` ✅ · `cargo check --workspace --all-targets` ✅ 0 error · `cargo test --workspace --no-fail-fast` ✅ 0 failed · R1/R2 真模型 ✅ · C1.2 最小集 5/5 ✅

## 11. Result

**C1.3: PASS**

- §2 不变量清单**已全部成立**并被确定性测试锁定（此前无测试覆盖，属"实现正确但无保障"）。
- 真任务验证不再出现任何 `Operation not permitted`，工作区零污染。
- 安全边界未放宽：越权写入仍被拒，生产代码零改动。

**遗留决策（需要你拍板，本轮未做）**：macOS 裸 `mktemp -d` 在验证沙箱内不可用。唯一修法是把 Darwin 每用户共享临时目录（`/var/folders/<hash>/T/`）加入 verify 的可写 roots —— 这正是 §3 禁止项，且会反转既有的两条安全测试。精确影响面：该目录是当前用户所有进程共享的临时区（不含其他用户、不含 DARWIN_USER_CACHE_DIR）；授权后 verify 命令可读写其他应用的临时文件。建议维持现状，并在文档中把"verify 脚本请用 `$TMPDIR` 显式模板或语言级 temp API"作为约定。

不开始 Exploration production fix。
