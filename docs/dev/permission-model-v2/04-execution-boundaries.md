# 04 · 执行边界一致性

日期：2026-11-24
目标：保证「准入层允许的动作，执行层不会因为另一个边界模型而失败」；同时把不能强制执行的边界变成显式的 `ExecutionBoundaryFailure`，而不是假放行或迟到的 EPERM。

## 1. 结论

1. 新 Auto 的普通调用使用 `WriteScope::Unrestricted`：文件工具与命令都不再有工作区写围栏。这是「工作区外普通写入必须真正成功」的唯一自洽实现。
2. 所有权、敏感路径、信任存储、凭据、网络、显式规则仍然独立生效；它们在 `WriteScope` 之前或之外判定，不依赖 OS 写围栏。
3. 第一档保留 `Workspace` 写围栏；本地路径 Git remote 的「策略 Allow + 沙箱 Deny」通过**为该次调用增加精确写根**修复，而不是把 push 改成审批。
4. 显式 `--deny-network` 在所有档位都不可被审批清除；宿主无法强制时返回 `ExecutionBoundaryFailure`，不询问用户。
5. 新 Auto 不是 Full：Full 在准入链首短路，跳过规则、hook、所有权、凭据、定义同意；新 Auto 全部保留。

## 2. 写边界模型

### 2.1 PolicyKind → WriteScope

| PolicyKind | 主 Agent 预设 | 子 Agent 有 claim | 子 Agent 无 claim | 只读角色/overlay |
|---|---|---|---|---|
| `LegacyRestricted` | `Workspace{root}` | `ScopedWorkspace` | `None` | `None` |
| `LegacyAuto` | `Workspace{root}`；Git 元数据写时为 `WorkspaceWithGit` | `ScopedWorkspace` | `None` | `None` |
| `EffectAuto` | `Unrestricted` | `ScopedWorkspace` | `None` | `None` |
| `Full` | `Unrestricted`（短路） | `Unrestricted`（既有契约） | `Unrestricted`（既有契约） | `None`（角色边界仍在） |

实现位置：`crates/leveler-tools/src/tool.rs::ToolPolicy::write_scope`。

**优先级不得改变：**

```text
1. unrestricted_execution()（Full / 精确调用批准） → Unrestricted
2. 已冻结的 ResolvedExecutionPolicy.write
3. read_only / has_zero_write_authority() → None
4. command_write_allowlist / command_foreign_paths → ScopedWorkspace
5. turn 级 grants（unrestricted_fs / repository_git）
6. PolicyKind 预设（仅这里受 EffectAuto 影响）
```

即：**所有权永远先于 Auto 的自主写权限**。新 Auto 取消的是第 6 步的工作区围栏，不是第 3/4 步的 claim/fence。

**独立说明：** `!command` 的用户 shell（`Application::user_shell_execution`）目前直接使用 `PermissionProfile::write_scope`，不是 `ToolPolicy`。新 Auto 是否也取消该路径的工作区围栏是独立决策；本文默认**保持现状**（用户 shell 是用户自己的进程，不是 Agent 调用），避免扩大本轮范围。若要保持一致，Phase 2C 可另立一条产品决策，而不是顺手改。

**`request_permissions` 的退化处理：** 新 Auto 已经默认持有 `filesystem=unrestricted` 与 `network`，模型再请求这两项时直接返回「已持有」，不弹审批；只有 `full_access` 升级仍需真人决定。否则旧提示词习惯会让新 Auto 产生无意义的中断。

### 2.4 系统提示面（`prompt.rs::operating_rules`）

当前 `operating_rules` 只按 `PermissionProfile` 输出文案，V2 必须改为按 `PolicyKind`：

| PolicyKind | 提示要点 |
|---|---|
| `LegacyRestricted` | 沿用现有 RequestApproval 文案：工作区可写、外部写/网络需要批准 |
| `LegacyAuto` | 沿用现有 Assisted 文案：工作区读写/网络自动；不可逆/提权/host opener/push 需批准；`.git` 由沙箱封闭，resolved Git 命令自动解封 |
| `EffectAuto` | 新文案：文件工具可写工作区外；**没有**工作区写沙箱；只有当前调用可直接识别的不可逆破坏才需要批准；`sudo`/私有网络/凭据/定义写入仍会问；`.git` 不再由 OS 写围栏封闭，但文件工具仍拒绝凭据/信任路径；不要为 filesystem/network 调 `request_permissions`（已持有） |
| `Full` | 现有文案不变 |

若只按档位名输出，v2 `request_approval` 会收到「网络需要批准」的错误提示，v2 `assisted` 会收到「不可逆/提权/host opener 需批准」的旧 Auto 提示——模型会据此做出错误的提权或等待行为。这是兼容层的一部分，不是文案润色。

`mode_label` 也应输出 policy-specific 的名字（例如 `legacy-auto` / `effect-auto` / `full-access`），而不是把第一档叫成 `request-approval` 让模型以为每步都要问。用户界面仍然显示三档中文名；模型提示面显示策略名。

### 2.2 文件工具的执行路径

`write_file` / `apply_patch` 的准入与执行都调用 `Workspace::resolve_for_write(path, &context.write_scope())`：

- `Unrestricted` → 任意路径，但仍经过 `check_sensitive`：
  - `.git`、`.ssh`、`.aws` 任一路径段；
  - `.env*`、密钥/证书、`credentials.json` 等敏感文件名；
  - 项目 `.leveler/hooks.yaml`、`.leveler/permissions.yaml`（内容键控的信任闸门）；
  - `.leveler/agents`、`.leveler/skills`；
  - 定义信任存储、运行时控制命名空间（`security_control_write_denied`）。
  - 注意：`.leveler/config.yaml` **不**在写拒绝列表里；普通编辑会让信任摘要失效，不能继承此前的批准。不要把它误写成硬拒绝。
- 这些拒绝发生在工具层，不产生审批；模型得到可读的拒绝原因。
- `execution_workspace()` 对 `EffectAuto` **不会**设置 `with_unrestricted_access(true)`，因为 `unrestricted_execution()` 为 false。因此敏感检查保持生效。这一点是关键：如果把新 Auto 误实现成 `unrestricted_execution() == true`，敏感文件闸门会一起被打开。
- `WorkspaceEditor::replace/create/remove` 通过 `editing_workspace()` 选择 root fd（`crates/leveler-tools/src/workspace/editor.rs`）。新 Auto 必须让 `EffectAuto` 的 `Unrestricted` 写使用文件系统根作为 root，但**不能**设置 `unrestricted_access(true)`；否则 `apply_patch` / `write_file` 会在准入 Allow 之后于 `strip_prefix` 处失败，形成另一种假放行。判定谓词应基于冻结的 `WriteScope`（是否 `Unrestricted`），而不是 `unrestricted_execution()`（后者只代表 Full/精确调用批准）。
- **回滚能力不随写范围扩大。** checkpoint 与 `run_command` 快照只覆盖 workspace；新 Auto 的工作区外写入不在可回滚范围内。这是既有能力边界，不是本轮新增的保证；文档不得暗示「所有写入都可撤销」。

### 2.3 命令的执行路径

`command_execution.rs` 把 `context.write_scope()` 写入 `ProcessRequest.write_scope`：

- `Unrestricted` + 网络允许 → `sandbox_command_with_read_denials` 直接返回原程序（macOS/Linux），Windows 跳过 `leveler-confine.exe`。
  - 例外：宿主声明了读拒绝根（eval/harness 的 sealed roots）时，macOS 仍会加一个 `(allow default)(deny file-read* …)` 的 profile；写仍然不受限。这是读隔离，不是写围栏。
- `Unrestricted` + 显式断网 → 仍构建只断网的 wrapper（macOS seatbelt / Linux `unshare`）。
- 如果宿主没有读拒绝根，也没有断网，命令就是普通 spawn；这是新 Auto 的契约。

## 3. 三个平台的实现路径

### 3.1 macOS（Seatbelt）

| 场景 | 实现 |
|---|---|
| 新 Auto 普通命令 | `scope.confines() == false` 且无断网 → 不加写围栏；若宿主声明读拒绝根（eval/harness），只加读拒绝 profile，写仍不受限 |
| 新 Auto + `--deny-network` | `macos_sandbox_command` 的 `!scope.confines()` 分支：`(version 1)(allow default)(deny network*)` + 读拒绝参数 |
| 第一档普通命令 | `SEATBELT_BASE` + 可写根（workspace、scratch、缓存、`/tmp`）+ 保护路径 deny（`.git`、定义、AGENTS.md、运行时控制） |
| 第一档 + 本地 Git remote | 在 `sandbox_write_roots` 中追加该 remote 的精确路径（见第 5 节） |
| 新 Auto 文件工具 | 无 OS 包装；`Workspace::resolve_for_write(Unrestricted)` 负责敏感/信任拒绝 |

注意：Seatbelt 的 `allow default` 会允许写运行时控制路径。新 Auto 没有 OS 写围栏，这是接受的契约；不要把「文件工具拒绝写 token」误写成「命令也不能写 token」。

### 3.2 Linux（bwrap / unshare）

| 场景 | 实现 |
|---|---|
| 新 Auto 普通命令 | `scope.confines() == false` 且无断网 → 直接 spawn |
| 新 Auto + `--deny-network` | `unshare --user --map-root-user --net <program> <args>`；需要内核允许非特权用户命名空间 |
| 第一档普通命令 | `bwrap --ro-bind / /` + 每个可写根 `--bind` + 保护路径 `--ro-bind`；缺失的保护路径由 `leveler-linux-confine` 的 seccomp fence 处理 |
| 第一档 + 本地 Git remote | 同 macOS：追加精确写根 |

**必须新增的预检：** `probe_sandbox_capabilities().network_deny` 目前在非 Windows 上恒为 `true`，但 `unshare --user --map-root-user --net` 在禁用 user namespace 的发行版上会失败。Phase 2C 要增加一次真实探测（例如执行 `unshare --user --map-root-user --net true`，缓存结果），并在准入层把 `network_deny=false` 用于 `ExecutionBoundaryFailure`。

### 3.3 Windows（MIC / `leveler-confine.exe`）

| 场景 | 实现 |
|---|---|
| 新 Auto 普通命令 | `WriteScope::Unrestricted` → `FilesystemIntent::Unrestricted` → 普通 spawn（Job Object 进程树仍在） |
| 新 Auto + `--deny-network` | **没有后端**：`assert_intent_spawn_allowed` 返回 `NetworkDenyUnsupported`。准入层必须提前返回 `ExecutionBoundaryFailure` |
| 第一档普通命令 | `FilesystemIntent::WorkspaceWrite` → `leveler-confine.exe` + Low integrity 标签；缺少 launcher 时 fail-closed |
| 第一档 + 本地 Git remote | remote 路径不在 Low 标签的可写根内时，需要为该调用扩展写根 lease（与 macOS/Linux 的精确写根对应） |

Windows 没有 per-command network deny，这是既有的、已文档化的能力缺口。V2 不尝试用 AppContainer 绕过它；显式断网 + 命令 = 明确失败。

## 4. 敏感路径、信任与资源的独立边界

新 Auto 取消写围栏，不等于取消以下边界：

| 边界 | 文件工具 | 命令 | 说明 |
|---|---|---|---|
| 凭据文件名（`.env*`、`.ssh`、`.aws`、密钥） | 拒绝 | 参数/脚本字符串命中 → 硬拒绝 | `shell_guard::refuse_sensitive_args` / `refuse_shell_script` |
| 定义信任存储 | 拒绝 | 无 OS 围栏（新 Auto） | 信任是内容键控的；写存储不等于获得信任 |
| 项目 `.leveler/hooks.yaml`、`.leveler/permissions.yaml`、`.leveler/agents`、`.leveler/skills` | 拒绝 | 无 OS 围栏（新 Auto） | 用户可用 `deny` 规则补强；`config.yaml` 可编辑但会失去信任 |
| 运行时控制命名空间 | 拒绝写 | 无 OS 围栏（新 Auto） | **接受的风险**，见 4.1 |
| 定义写入同意 | 必须真人（`save_agent`/`save_skill`） | 不适用 | 不变 |
| 不可信定义使用 | 信任闸门 | 不适用 | 不变 |
| 凭据使用 | Ask（credential 阶段） | 同左 | 不变 |
| 私有网络 | Ask（`web_fetch`） | 不适用于命令 | 不变 |
| 其他执行者所有权 | Deny/Ask | Deny/Ask | 不变 |

### 4.1 运行时控制的诚实说明

旧 Auto 的 workspace 沙箱会在 OS 层隐藏 `<home>/run/sockets` 与 access token。新 Auto 没有 OS 写围栏，因此：

- 文件工具仍然拒绝写这些路径（`security_control_write_denied`）；
- 普通 shell 命令在 OS 权限范围内可以写它们；
- 一个 `python script.py` 本来就可以写；为「直接 `> token`」单独保留 fence 是安全剧场，无法覆盖间接路径。

结论：**新 Auto 的运行时控制保护在工具 API 与内容校验层，不在 OS 层。** 这是高自主 Auto 的明确取舍，必须写进 `SECURITY.md` 与产品文案。需要更强保证的用户应使用第一档或显式 `deny` 规则。

**建议的后续硬化（不作为 V2 发布阻断）：** macOS/Linux 可以做一个 deny-only 的 control-root 沙箱（macOS `(allow default)(deny file-read*/write* control)`；Linux bwrap `--bind / /` + 对 control roots `--ro-bind`），它不影响普通写入；Windows 需要 restricted token / deny ACE 方案，当前 `UNVERIFIED`。V2 暂不纳入，因为产品已接受新 Auto 的脚本拥有用户级文件权限；但 `SECURITY.md` 必须明示「新 Auto 不隐藏运行时控制命名空间」，不得沿用旧 Auto 的表述。

### 4.2 为什么不用「账户写围栏」

备选方案：新 Auto 使用「$HOME + workspace + temp 可写，系统路径不可写」的围栏。已否决：

1. 用户拥有的系统路径（Homebrew、`/usr/local`）会再次出现「策略 Allow、沙箱 Deny」。
2. 要在准入层按路径询问，等于把工作区边界换成账户边界，违背「普通写入不按路径人审」。
3. Windows MIC 表达「多根可写、其余不可写」需要新的 lease 机制，跨平台不一致。
4. 仍然无法覆盖间接写入。

`Unrestricted` 是唯一能保证「策略允许的普通写入一定成功」的模型。

## 5. 本地路径 Git remote

### 5.1 现状

- 第一档 `git push` 被分类为 Allow（旧 Auto 契约）。
- 当 `origin` 是工作区外的文件路径时，push 需要写该路径的临时对象目录；`Workspace` 沙箱只允许 workspace/临时/缓存 → `unable to create temporary object directory`。
- 模型随后 `request_permissions(filesystem=unrestricted)` → 1 次审批 + 1 次失败调用。

### 5.2 目标

1. 在准入阶段解析 Git 的 remote 目标：
   - 已配置 remote：读取仓库配置（宿主解析，不读模型文本）；
   - 显式 URL/路径：解析 argv。
2. 目标是本地路径（`/…`、`./…`、`file://…`、Windows 盘符/UNC）：
   - 解析到规范路径；
   - 若是敏感路径（凭据/信任/运行时控制）或无法确认是 Git 仓库 → 保持 Ask，并在提示中给出目标；
   - 否则把该路径加入本次 `ProcessRequest.sandbox_write_roots`（宿主权威，非模型输入）。
3. 目标是网络 URL → 不追加写根；网络/凭据走现有路径。
4. 新 Auto：写围栏已取消，不需要追加；仍然拒绝敏感路径参数。

### 5.3 为什么不是「把 push 改成 Ask」

- `git push` 在旧 Auto 中是明确允许的普通开发动作；改成 Ask 会改变第一档的授权契约，并让日常流程多一次中断。
- 失败原因是执行边界没有覆盖命令声明的写目标，不是权限策略过宽。
- 精确写根把「Allow」变成「能真正执行」，符合「不得假放行」。

### 5.4 验收

- 第一档：workspace 内 remote、`/tmp` remote、工作区外普通路径 remote、bare repo remote：0 审批、0 失败调用。
- 敏感路径 remote（如 `~/.ssh/...`）：Ask，提示含目标；拒绝后无写入。
- 新 Auto：全部成功；敏感路径仍被参数闸门拦截。

## 6. MCP 边界

### 6.1 执行事实

MCP server 是 CodeLeveler 启动的独立进程，**没有 OS 沙箱**。因此：

- 无法对 MCP 强制写围栏；
- 无法对 MCP 强制网络拒绝；
- 显式 `--deny-network` 下 MCP 必须是 `ExecutionBoundaryFailure`，不能假装允许。
- 新 Auto 下 MCP 子进程仍然使用 **scrubbed env**（`connect_with_authority(unrestricted=false)`）；只有 Full 才继承完整环境。取消写围栏不等于给 MCP 服务器 Full 的环境权威。

### 6.2 类型化 annotations

当前 `McpToolInfo` 只有 `name/description/input_schema`；HTTP transport 把 annotations 拼进 description 文本。这是不可用于判定的（自然语言、不可信）。

Phase 2B 增加：

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct McpAnnotations {
    pub read_only_hint: Option<bool>,
    pub destructive_hint: Option<bool>,
    pub idempotent_hint: Option<bool>,
    pub open_world_hint: Option<bool>,
}
```

- stdio：从 `tools/list` 的 `annotations` 对象解析；
- HTTP：从 `rmcp::model::Tool.annotations` 复制类型化值（不再只拼文本）；
- `Tool` trait 增加默认 `fn mcp_annotations(&self) -> Option<McpAnnotations> { None }`；
- 判定只读类型化字段，不读 description。

### 6.3 授权单元

| 场景 | 第一档 `LegacyAuto` | 第二档 `EffectAuto` |
|---|---|---|
| 普通 MCP | 每次首个调用 Ask；ApproveSession 后同工具本 turn 复用（friction patch） | Allow |
| `destructiveHint=true` | Ask（本来就 Ask） | Ask；**只按精确调用授权**；`decisions()` 不提供 `ApproveAlways`（持久工具规则会跨参数/跨会话放行），只提供 Once / Session / Deny |
| `readOnlyHint=true` | 同普通 MCP | Allow |
| 显式 `--deny-network` | Deny | ExecutionBoundaryFailure |
| 服务器谎报 `destructiveHint=false` | 无影响（所有 MCP 都 Ask） | 无法识别 → Allow（文档化缺口） |

`ApproveAlways` 仍写持久工具规则；对 `destructiveHint=true` 的工具，提示必须明确写「该工具声明为破坏性」。是否禁止对破坏性 MCP 提供 Always 是产品决策，本文默认保留（用户显式选择）。

**开放问题的判断：** 第一档继续沿用 patch 的「同工具本 turn 复用」，这仍然不是按参数绑定资源。V2 不在第一档收紧它，因为那是已验收的 friction 改动，收紧会重新制造 mutating MCP 的连续审批。新 Auto 的普通 MCP 不 Ask，破坏性 MCP 只按精确调用授权，因此新路径不会再扩大这个问题。若未来要求第一档也按参数边界，需要单独产品决策。

## 7. 所有权与后台任务

| 场景 | 行为 |
|---|---|
| 子 Agent 未 claim 的文件写 | Deny 返回模型；`claim_write_scope` 指引 |
| 子 Agent 有 claim 的文件写 | 只允许 claim 范围；越界 Deny |
| 子 Agent 命令 | `ScopedWorkspace`；构建产物不在 claim 内时 EPERM + claim 指引 |
| 子 Agent 调 MCP | Deny（不可约束） |
| 只读角色（Explorer/Reviewer） | 工具集物理只读；写尝试 Deny；Full 也不解除角色边界 |
| 其他执行者拥有的路径 | Ask（人可覆盖的并发边界） |
| 后台任务 | 与前台同一准入/沙箱路径；`background=true` 不产生额外权限 |

新 Auto 不改变所有权协议。唯一变化是主 Agent 的预设写围栏；claim 收窄在预设之前。

### 7.1 `blocked_kind=user_decision` 的错误归类

上一轮记录：子 Agent 未 claim 编辑被结构化拒绝后，模型把 `update_goal(blocked)` 报成 `blocked_kind="user_decision"`，但运行时从未询问用户。

V2 的判断：

- **不是权限模型自然解决的问题。** 归属在领域层的 Goal 语义，不在 Permission 层。
- 现有代码已经有防护：`drive.rs` 只在「本 turn 确实发过 `request_user_input` 且之后没有继续工作」时接受 `user_decision`；否则先 repair、再 degradation，并在工具结果里明确说「该用户决定从未作为问题提出」。
- V2 会**降低触发频率**：新 Auto 不再因工作区边界产生结构性拒绝，剩下的主要是所有权拒绝。
- 需要的独立修复（小改动，不是新框架）：在结构性 Deny 的工具结果/事件里带稳定理由码（`boundary.host_ownership`），让模型和审计者能区分「运行时拒绝」与「用户拒绝」。**不要新增 `blocked_kind` 值**，那会改变 Goal 契约；现有 repair + `external` 已经足够。

验收：`loop_test` 的 `a_user_decision_block_without_asking_is_repaired_once` 保持绿；H4 场景的最终记录不得声称用户做过决定。

## 8. 理由码与可观测性

1. `ApprovalRequest` 新增 `reason_code: PermissionReasonCode`（`#[serde(default)]`）。
2. `EngineEvent::ApprovalRequested` 新增 `reason_code`（持久事件，旧行缺省）。
3. `UiApprovalRequest` 新增 `reason_code` 与结构化 `facts`（路径列表 / Git capabilities / MCP server+tool）。
4. TUI 审批页显示稳定理由码与具体对象；不再只靠自由文本 summary。
5. `doctor` 增加一行：当前会话的「档位 + 契约版本 + 写围栏（workspace / none）」与网络强制能力。

这解决上一轮 P1 的「审批事件缺少稳定原因码」缺口，也为 2D 的审批归因提供机器可读数据。

## 9. 「Policy Allow → Execution Deny」审计表

| # | 场景 | 现状 | V2 处理 |
|---|---|---|---|
| 1 | 第一档本地路径 Git remote | Allow + EPERM | 精确写根（5 节） |
| 2 | 子 Agent scoped 构建写 | Allow + EPERM | 保持边界；claim 指引（patch） |
| 3 | Windows 显式断网 + 命令 | Allow + spawn 拒绝 | 准入 `ExecutionBoundaryFailure` |
| 4 | Linux `Loopback` scope | Allow + `UnsupportedNetworkScope` | 准入 BoundaryFailure（非 macOS） |
| 5 | Linux user namespace 不可用 | Allow + unshare 失败 | 启动探测 + BoundaryFailure |
| 6 | 新 Auto 工作区外文件写 | 旧 Auto Ask/拒绝 | Allow + 真正成功 |
| 7 | 新 Auto 工作区外命令写 | 旧 Auto EPERM | Allow + 真正成功 |
| 8 | 新 Auto 写敏感路径 | — | 准入 ExplicitDeny（工具层），不产生假 Allow |
| 9 | MCP + 显式断网 | Deny | BoundaryFailure（语义更准确） |
| 10 | 定义写入 | Ask | 保持 Ask，不假放行 |

**禁止的实现方式：** 在工具执行前临时把 `WriteScope` 改成 Unrestricted 来「让命令成功」，而不在准入层解释为什么；这会把策略与执行再次拆成两个真源。

## 10. 平台验收矩阵

| 能力 | macOS | Linux | Windows |
|---|---|---|---|
| 新 Auto 普通文件写（工作区外） | 必须实测 | 必须实测 | 必须实测 |
| 新 Auto 普通命令写（工作区外） | 必须实测 | 必须实测 | 必须实测 |
| 第一档工作区写围栏 | 既有 seatbelt 测试 | bwrap / fence 测试 | MIC + launcher |
| 第一档本地 Git remote | 实测 push | 实测 push | 实测 push |
| 显式断网 + 命令 | seatbelt 实测 | unshare/bwrap 实测 | BoundaryFailure 实测 |
| 显式断网 + MCP | Deny 实测 | Deny 实测 | Deny 实测 |
| `Loopback` scope | 支持 | 明确 Unsupported | 明确 Unsupported |
| 子 Agent claim | 既有测试 | 既有测试 | 既有测试 |
| 只读角色 | 既有测试 | 既有测试 | 既有测试 |

本文不把 macOS 的结论外推到 Windows/Linux；2E 必须给出每个平台的实际观测记录。

## 11. 测试计划

| 测试 | 层 | 断言 |
|---|---|---|
| `effect_auto_writes_outside_the_workspace` | tools 集成 | 无审批，文件出现在工作区外 |
| `effect_auto_command_writes_outside_the_workspace` | execution sandbox | `sh -c 'echo > ../outside'` 成功 |
| `legacy_auto_still_confines_workspace_writes` | execution sandbox | 第一档越界写 EPERM/Ask |
| `legacy_git_push_to_local_remote_needs_no_escalation` | execution sandbox | 0 审批、0 失败、remote 更新 |
| `deny_network_command_is_a_boundary_failure_on_windows` | admission（cfg windows） | `ExecutionBoundaryFailure`，无 spawn |
| `deny_network_command_is_enforced_on_macos` | execution sandbox | 网络请求失败 |
| `loopback_scope_is_refused_before_spawn_on_linux` | admission（cfg linux） | BoundaryFailure |
| `mcp_under_explicit_deny_is_a_boundary_failure` | admission | 无执行 |
| `destructive_mcp_asks_with_exact_signature` | admission | 两次不同参数 = 两次 Ask |
| `sensitive_write_is_denied_without_a_prompt` | tools | 无审批、无写入 |
| `sub_agent_unclaimed_still_denied_in_effect_auto` | multi-agent | Deny + claim 指引 |

## 12. 交付时的验证命令

```bash
env -u NODE_OPTIONS cargo test -p leveler-execution --test effect_auto_decision_table
env -u NODE_OPTIONS cargo test -p leveler-execution --test auto_permission_sandbox
env -u NODE_OPTIONS cargo test -p leveler-execution --test auto_permission_decision_table
env -u NODE_OPTIONS cargo test -p leveler-agent --test multi_agent_test
env -u NODE_OPTIONS cargo test -p leveler-tools --lib
env -u NODE_OPTIONS cargo test -p leveler-execution --test git_permission
```

Windows/Linux 的真实平台验证不能用交叉编译代替；在对应宿主机上运行同一组测试并记录环境。
