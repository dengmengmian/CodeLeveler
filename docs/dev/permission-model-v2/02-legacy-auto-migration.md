# 02 · 旧 Auto 基线与第一档迁移

日期：2026-11-24
基线：`5800dd8a` + `docs/dev/auto-permission-friction/evidence/auto-permission-friction.patch`
目标：把「旧 Auto」完整迁移为 V2 的第一档「请求批准」，不复制两套权限实现，不恢复历史缺陷。

## 1. 基线的准确定义

旧 Auto 不是一个名字，而是一组可核对的源码行为：

| 项 | 值 |
|---|---|
| 源码 HEAD | `5800dd8aad887137e86806e10d02ec84f5860c5d` |
| 版本 | `v1.0.28-19-g5800dd8a` |
| 未提交 patch | `docs/dev/auto-permission-friction/evidence/auto-permission-friction.patch` |
| 行为证据 | `docs/dev/auto-permission-friction/01..05`、`evidence/metrics-pairs.*` |
| 关键实现 | `PermissionProfile::Assisted` + `ApprovalPolicy::evaluate` 的 Assisted 分支 + `WriteScope::Workspace` |
| 实测日常路径 | 普通开发、15 条复合 Shell、`cargo add + build`、完整 Git 流程：**0 审批**（见 `03-friction-baseline.md`） |

**版本串注意：** friction lab 的 baseline 二进制自报 `1.0.29 (a2d8134efdc5)`，与源码 HEAD `5800dd8a` 不同。这是 lab 构建/版本嵌入的产物，不是行为来源；证据以 `git archive 5800dd8a` 的源码树和 `evidence/clean-tree-status.txt` 为准。不要把 `a2d8134e` 当成基线 commit。

**重要：** patch 当前存在于工作区但**未提交**，同树还有并发会话的 web-latency / web-search / doctor / memory 改动。Phase 2A 开始时必须先把 friction patch 单独落成一个可复核的提交（需要单独授权），否则后续 checkout / 并发提交可能丢失第一档的基线。不得把并发改动一起打包。

### 1.1 patch 已改变、第一档必须继承的行为

| 行为 | 修改前 | patch 后（要继承） | 源码 |
|---|---|---|---|
| 子 Agent 未 claim / 越界编辑 | Ask（批准后 `ApprovedOnce` 提升为 `Unrestricted`，绕过 claim） | Deny 返回模型，理由含 `claim_write_scope` | `host.rs::resolve_policy` 的 `refuse_unscoped_mutation` |
| 子 Agent 调 MCP | Ask | Deny（结构性不可约束） | `refuse_unboundable_delegated_tool` |
| 主 Agent 静态 allowlist 越界编辑 | Ask | 仍 Ask（host-owned Node 没有 claim 协议） | 同函数的分支 |
| MCP "this turn" | 精确参数签名 | 同一 MCP 工具本 turn 复用 | `mcp_session_key` / `Executor::ask` |
| 子 Agent scoped 命令被 EPERM | 只说 "writes were confined" | 追加 `claim_write_scope(paths)` 指引 | `leveler-tools/src/recoverable.rs`、`command_execution.rs` |

这些是**已修复的所有权/安全缺陷**。第一档不得回退。

## 2. 旧 Auto 的完整行为清单

以下按准入链顺序列出。第一档必须逐条保持一致；Phase 2A 的验收以「同一输入、同一输出」为准。

### 2.1 Profile 级

| 事实 | 值 | 源码 |
|---|---|---|
| 网络默认 | 开启（`network_by_default = true`） | `risk.rs` |
| 风险许可 | 所有 RiskLevel 都 permit（高风险的 Ask 由策略层处理，不做 registry 硬拒绝） | `risk.rs::permits` |
| 写围栏 | `Workspace{root}`；Git 写元数据时升级为 `WorkspaceWithGit{root}` | `risk.rs::write_scope`、`host.rs::resolve_policy` |
| 只读 overlay | `read_only` turn 的非 Safe 工具 Ask | `host.rs` |

### 2.2 命令分类（`ApprovalPolicy::evaluate` 的 Assisted 分支）

| 命令形态 | 旧 Auto | 依据 |
|---|---|---|
| 任意 Git（add/commit/push/reset/clean/rebase/branch -D/tag -d/stash drop/filter-branch…） | Allow | `git_command_class` 只在「Git 会执行本地程序」或「remote 凭据不可绑定」时 Dangerous |
| `git -c core.hooksPath=…`、`--exec-path`、`GIT_SSH_COMMAND=…` | Ask | `git_argv_executes_local_program` / `git_remote_credential_unbrokered` |
| `rm` / `rmdir`，所有目标都是「工作目录内的字面相对路径」 | Allow | `deletion_targets_stay_in_workdir` |
| `rm` 绝对路径、`..`、`~`、glob、变量、无目标 | Ask | 同上 |
| `dd`、`mkfs`、`shutdown`、`reboot`、`halt`、`poweroff` | Ask | `DESTRUCTIVE` |
| `sudo` / `su` / `doas` | Ask | `PRIVILEGED` |
| `open` / `xdg-open` / `gio open` / Windows `start` | Ask | host escape |
| `pwsh` / `powershell` / `fish` | Ask | 没有语法，fail closed |
| 凭据库读取（`security find-generic-password -w`、`secret-tool lookup`、`pass show`、`git credential fill`） | Ask | `is_credential_store_read` |
| 含 `$()` / 反引号且 AST 解析失败的脚本 | Ask | 字符串 fallback fail closed |
| `cmd /C` 中的 `%NAME%` 目标 / 程序字 | Ask | 不可读词按危险处理 |
| 重定向到工作区外（非 `/tmp`） | Ask | `fallback_redirect_is_dangerous` |
| 其余普通程序（cargo/npm/python/node/make/…） | Allow | 默认 Safe |

### 2.3 工具 admission

| 场景 | 旧 Auto | 源码 |
|---|---|---|
| `write_file` / `apply_patch` 写工作区外 | Ask（提示写范围扩大） | `workspace::path_approval_reason` |
| 写凭据文件名 / 信任存储 / 运行时控制路径 | 文件工具拒绝（不是审批） | `security_control_write_denied`、`Workspace::check_sensitive` |
| `run_command` cwd 在工作区外 | Ask | `cwd_approval_reason` |
| `run_command` 参数指向凭据文件 | Ask，且执行层硬拒绝 | `shell_guard::refuse_sensitive_args` |
| `shell_command` 含 `&`/`nohup`、凭据路径、吞命令的 `#` 注释 | 硬拒绝 | `shell_guard::refuse_shell_script` |
| `web_fetch` 非 loopback 私有/局域网 | Ask | `web_fetch::admission_reason` |
| `native_action`（邮件发送） | Ask | 工具自己的 `approval_reason` + `RiskLevel::Destructive` |
| `computer_act` | Ask | 同上 |
| `task_control` 操作其他执行者的任务 | Ask | `task_admission_reason` |
| `save_agent` / `delete_agent` / `save_skill` / `delete_skill` | Ask，且必须真人，Once/Session/Always 均不可复用 | `needs_human_consent` |
| MCP `mcp__*` | Ask（无条件） | `ApprovalPolicy::evaluate` |

### 2.4 规则、资源、凭据、网络

| 机制 | 旧 Auto | 源码 |
|---|---|---|
| 权限规则 | `deny > ask > allow`；规则先于 profile；文件不可解析则全部 Ask | `permission_rules.rs`、`host.rs` |
| pre-hook | Deny 直接拒绝 | `host.rs` |
| Session/Project 授权 | 精确调用签名 + 资源绑定（Session/Project scope） | `host.rs::ask`、`resource_grants` |
| `ApproveAlways` | 持久项目规则；定义写入不可用 | `remember_always`、`always_rules_for` |
| 凭据使用 | 命令会使用已存凭据 → Ask；资源解析失败且可能用凭据 → Ask | `host.rs` 的 credential 阶段 |
| 私有网络 | `web_fetch` Ask；loopback 允许 | `web_fetch.rs`、`network.rs` |
| 显式 `--deny-network` | 命令/网络工具无网络；MCP 直接 Deny；Full 也保留 | `host.rs`、`ToolPolicy::network_scope` |
| 子 Agent 所有权 | 未 claim/越界 = Deny 给模型；重叠 claim 拒绝；角色只读 | patch + `ownership.rs` |
| Full | 最前面短路，无审批/规则/hook/所有权 | `host.rs` |

## 3. 历史修复：不得恢复

| commit | 修复 | 第一档的要求 |
|---|---|---|
| `598d4a83` | Auto 允许所有 Git 与工作区内字面删除 | 保留；不得回到「Git 一律 Ask」 |
| `416ed869` | `cmd` 重定向目标的转义/引号语义 | 保留分类器 |
| `86e1935e` | `%NAME%` 展开的目标/程序按不可读处理 | 保留 |
| `5d4893d6` | 模式切换时 supersede 待审批问题 | 保留；新增契约升级也要 supersede |
| `14c92cc9` | 写范围扩大必须披露；批准只为可写动作解除围栏；显式断网穿透 Full；模型预算并发；子 Agent 禁止调度工具；工具输出配额；硬链接限制 | 全部保留 |
| friction patch | 子 Agent 结构性拒绝回给模型；MCP 工具级 turn；claim 指引 | 全部保留 |

**不得恢复的旧缺陷：**

1. 批准子 Agent 未 claim 编辑 → `ApprovedOnce` 提升为 `Unrestricted` → 绕过 claim。
2. 子 Agent MCP 调用人审 → 批准后越过「MCP 不可约束」。
3. 写工作区外但批准后不真正解除写围栏（导致「批准了还是失败」）。
4. `--deny-network` 在 Full 下被审批清除。
5. 规则文件不可解析时静默回落到 profile 默认放行。

## 4. 旧「请求批准」的严格策略（v1 专用）

现有 `PermissionProfile::RequestApproval` 与旧 Auto 不同，必须保留给 v1 历史数据：

| 维度 | 旧 `request_approval`（`LegacyRestricted`） | 旧 Auto（`LegacyAuto`） |
|---|---|---|
| 网络默认 | 关闭（`network_by_default = false`） | 开启 |
| `permits()` | 拒绝 `Destructive` / `Privileged` 工具 | 全部允许 |
| 命令风险 | 危险分类 Ask；命令本身不因「可能联网」逐条 Ask，网络由 OS 沙箱默认拒绝 | 危险分类 Ask；网络默认开启 |
| 写围栏 | Workspace | Workspace |

它不再作为用户可选档位出现，只由 v1 `request_approval` Session 与旧配置触发。实现上保留为一个 `PolicyKind` 分支，而不是删掉。

## 5. 第一档的实现方式

### 5.1 不复制框架

```text
ToolHost::resolve_policy
  现有主链（hook → admission → role → ownership → read-only → profile → rules → network/credential → ask）
  ↑
  ├─ LegacyAuto：复用现有 Assisted 分支（ApprovalPolicy::evaluate），
  │              写围栏 Workspace / WorkspaceWithGit
  ├─ LegacyRestricted：复用现有 RequestApproval 分支
  ├─ EffectAuto：同一主链，替换「profile 判定」为不可逆检测器，
  │              写围栏 Unrestricted
  └─ Full：链首短路，原样
```

落地方式：

1. `leveler-execution/src/risk.rs` 新增：
   ```rust
   pub enum PermissionContract { V1, V2 }
   pub enum PolicyKind { LegacyRestricted, LegacyAuto, EffectAuto, Full }
   impl PolicyKind {
       pub fn of(contract: PermissionContract, profile: PermissionProfile) -> Self;
       pub fn permits(self, risk: RiskLevel) -> bool;
       pub fn write_scope(self, root: &Path) -> WriteScope;
       pub fn network_by_default(self) -> bool;
   }
   ```
2. `ApprovalPolicy::evaluate` 增加 `PolicyKind` 参数（或改为 `PolicyKind::evaluate`），把现有 `RequestApproval`/`Assisted` 分支原样绑定到 `LegacyRestricted`/`LegacyAuto`。
3. `PermissionProfile::permits/write_scope/network_by_default` 保留为「档位预设」：`write_scope()` 继续供用户 shell 使用，`permits()` 只服务旧测试/文档，**不再作为准入真源**。准入路径改为读取 `PolicyKind` 的同名方法，两处答案由同一个 `PolicyKind` 派生，避免双真源。
4. 第一档的 `EffectAuto` 检测器**不启用**；`LegacyAuto` 不读取 `irreversible`。
5. 只读 overlay 的现有判断 `ctx.policy.mode() == PermissionProfile::Assisted` 必须改为按 `PolicyKind` 判断：`LegacyAuto` / `EffectAuto` → Ask，`LegacyRestricted` → Deny，Full → 短路。否则 v2 `request_approval`（第一档）会在 plan/只读 turn 里被误判成 Deny。
6. `ToolPolicy::network_scope()` 目前用 `self.mode().network_by_default()`；必须改为 `policy_kind.network_by_default()`。否则 v2 `request_approval` 会沿用旧 RequestApproval 的「网络默认关闭」，第一档就不是旧 Auto 了。

### 5.2 第一档与第二档的差异边界

| 维度 | 第一档 `LegacyAuto` | 第二档 `EffectAuto` |
|---|---|---|
| 写围栏 | Workspace | Unrestricted（所有权优先收窄） |
| 工作区外文件写 | Ask | Allow |
| `rm` 工作区内字面相对路径 | Allow（沙箱兜底） | Ask（真实数据） |
| `git reset --hard` / `clean -fd` / `push --force` | Allow | Ask |
| 脚本间接副作用 | 沙箱吞掉越界写 | 按用户 OS 权限发生 |
| MCP | Ask | Allow / destructiveHint Ask |
| 判定依据 | 命令分类 + 工具 admission | 不可逆检测 + 独立边界 |

### 5.3 必须保留的「旧 Auto 授权单元」

- Session 级：精确调用签名（非 MCP）；MCP 工具键（patch 行为）。
- Project 级：资源绑定 `GrantRequest`（文件路径、Git remote、凭据绑定）。
- Always：持久 `.leveler/permissions.yaml` 规则。
- 这些单元在 V2 中不改变语义；新 Auto 的自动放行不产生授权记录（`AuthorizationEvidence::Policy`）。

## 6. 第一档的验收标准

| # | 验收 | 方法 |
|---|---|---|
| 1 | 旧 Auto 决策等价 | 用 `5800dd8a + patch` 与 Phase 2A 候选跑同一决策表（命令/工具/规则），逐项断言 `Allow/Ask/Deny` 相同 |
| 2 | 日常零审批 | 复用 friction lab 的 A/B/C/E 场景，审批数 0 |
| 3 | 边界审批不变 | D（工作区外写）/ F（本地 remote）/ I（私有网络）/ G/J（MCP）审批数与基线一致或更少 |
| 4 | 所有权不回退 | H4 未 claim 编辑仍 Deny、0 审批、文件未变 |
| 5 | 历史修复不丢 | 上述 commit 对应的回归测试全绿 |
| 6 | 与第二档可区分 | 同一命令在第一档/第二档得到预期的不同判定（如 `rm -rf src`、`git reset --hard`） |
| 7 | 不新增模型调用 | 旧 Auto 路径的模型调用数不变 |

**阻断项：** 若 Phase 2A 无法证明「第一档与旧 Auto 决策等价」，不得进入 2B。第一档是兼容性锚点，锚点漂移会让 2D 的对比失去意义。
