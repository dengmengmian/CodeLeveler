# 09 · Phase 2C 报告：执行边界一致性

日期：2026-11-24
阶段结论：**PASS（macOS 本地）**
范围：写围栏、网络目的地、敏感路径、显式拒绝、所有权拒绝、Policy Allow 与实际执行一致。

## 1. 写边界

| 策略 | `WriteScope` | OS 沙箱 | 说明 |
|---|---|---|---|
| `LegacyRestricted` / `LegacyAuto` | `Workspace { root }` | 保留 | 第一档继承旧 Auto 的工作区围栏 |
| `EffectAuto` | `Unrestricted` | 不设工作区围栏 | 敏感路径、定义信任文件、凭据仍由 `Workspace::resolve_for_mutation`/`check_sensitive` 拒绝 |
| `Full` | `Unrestricted` | 不设围栏 | 与 V2 改动前相同 |

实现要点：

- `Workspace::resolve_for_mutation` 的 `Unrestricted` 分支继续做敏感路径、`.git`/`.ssh`/`.aws`、定义文件检查；只放宽“是否在 workspace 内”。
- `editing_workspace` 对 `Unrestricted` 选择目标父目录为 descriptor 根，避免 macOS `/var` → `/private/var` 使绝对路径写入失败。
- **修复（2D 验收发现）**：空后缀 `join("")` 产生尾部 `/`，随后 `read_to_string` 得到 `ENOTDIR`。现在空后缀直接使用 canonical 祖先；`unrestricted_scope_allows_writes_outside_the_workspace` 增加 `metadata` 断言锁定该行为。

## 2. 网络边界

| 策略 | `NetworkScope` | 私有/LAN | 显式 `--deny-network` |
|---|---|---|---|
| `LegacyRestricted` | 默认 `Loopback`，网络授权后 `Internet` | ASK | 强制 `None` |
| `LegacyAuto` | `Internet`（保留 SSRF 排除） | ASK | 强制 `None` |
| `EffectAuto` | `Any` | 允许（普通网络） | 强制 `None` |
| `Full` | `Internet`/unrestricted | 允许 | 强制 `None` |

实现要点：

- 新增 `NetworkScope::Any`：验证目的地结构但不排除私有地址；`Internet` 的 SSRF 排除保持不变。
- `ToolPolicy::network_scope()` 对 `EffectAuto` 返回 `Any`；`network_denied()`、host `network_allowed`、命令 `deny_network`、browser/MCP 的 scope 守卫全部把 `Any` 视为允许。
- `host.rs` 的审批冻结保留 `Any`，不会在“删除需要审批”时把目的地类收窄回 `Internet`。
- 显式 `--deny-network` 在 Full 下仍生效；MCP 这类无法约束的进程返回 `BoundaryFailure`，不伪装成 ASK。
- **修复（Linux 实机发现）**：访客 `LegacyRestricted` 的默认 `Loopback` 在 Linux bubblewrap 后端无法表达。此前 admission 把它当成 `UnsupportedNetworkScope` 直接 `BoundaryFailure`，导致 V1 严格档的普通命令全部失败（真实安全回归）。现在 `Loopback` 一律放行到执行层，由 `ProcessRequest::effective_network_scope()` 在不能表达时收窄为 `None`（拒绝所有网络，方向更严）。只有 `LocalLan`/`ConfiguredRemote` 这类没有 fail-closed 等价物的范围才返回边界失败。

## 3. 所有权与审批

- 所有权拒绝发生在审批之前，返回给模型（`Deny`），不转化为用户问题。新增 `effect_auto_does_not_soften_a_delegated_unclaimed_mutation`：V2 Auto 下未 claim 的子 Agent 编辑仍被拒绝，审批不能绕过。
- 定义文件、Hook、权限规则等信任门禁仍由 `read_trusted_project_file` + `check_sensitive` 负责，`EffectAuto` 不因取消写围栏而放开。

## 4. 证据

| 证据 | 命令/报告 | 结果 |
|---|---|---|
| 工作区外写入真实成功 | dogfood `v2_assisted/write_outside` | 审批 0，`file=true` |
| 私有 LAN 真实可达 | dogfood `v2_assisted/private_network` | 审批 0，`served=true` |
| 本地 Git push | dogfood `v2_assisted/git_push_local` | 审批 0，`pushed=true` |
| 审批与执行一致（拒绝即不执行） | dogfood `delete_real_file` / `git_reset_hard` | 拒绝后 `victim_exists=true`、`dirty_survived=true` |
| 显式拒绝不可被批准清除 | `cargo test -p leveler-tools --lib an_admitted_exact_call_keeps_its_frozen_network_scope`、`an_approved_call_under_explicit_denial_still_refuses_the_network` | passed |
| Loopback 不是边界失败 | `cargo test -p leveler-agent --lib loopback_is_never_an_unsupported_command_scope` | passed；Linux dogfood 中 V1 严格档普通命令成功且网络被收窄为拒绝 |
| 所有权拒绝 | `cargo test -p leveler-agent --lib unclaimed_mutation` | 2 passed |
| 敏感路径 | `cargo test -p leveler-tools --lib effect_auto_writes_outside_the_workspace_but_not_a_sensitive_path` | passed |
| 命令沙箱 | `cargo test -p leveler-execution --test auto_permission_sandbox` | macOS 全绿；Linux 结果见 11 |

## 5. 验收

| 标准 | 结果 |
|---|---|
| Policy Allow 与实际执行一致 | PASS（写、网络、Git 均在 dogfood 产生真实副作用） |
| 工作区外写入真实成功 | PASS |
| 硬约束（敏感路径、信任文件、凭据、显式拒绝） | PASS |
| 所有权 deny 不被批准绕过 | PASS |
| Full 行为不变 | PASS（Full 臂 0 审批；删除真实执行） |
