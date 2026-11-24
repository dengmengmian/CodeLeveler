# 01 · 现状与目标模型

日期：2026-11-24
阶段：Permission Model V2 / Phase 1（仅设计，不改业务代码）
审计基线：`main` @ `5800dd8aad887137e86806e10d02ec84f5860c5d`（v1.0.28-19-g5800dd8a）+ 工作区未提交的 Auto Permission Friction patch
平台：macOS Darwin 25.6.0 arm64

> 行号会随工作区并发改动漂移。本文用「文件 + 函数/类型」定位，必要时给 commit；实现阶段以函数为准，不以行号为准。

## 1. 结论先行

1. **三档的用户可见名称不变，内部语义重排。** 第一档「请求批准」采用旧 Auto（`assisted`）的能力；第二档「替我审批」变成新的高自主 Auto；第三档「完全访问」完全冻结。
2. **内部需要区分「档位」与「权限契约版本」。** 旧 Session 的 `request_approval` / `assisted` 不能按新标识直接解释，否则会静默提权。契约版本是兼容层的核心，不是第四档。
3. **三档 × 两个契约版本只折叠出四种策略实现**：`LegacyRestricted`、`LegacyAuto`、`EffectAuto`、`Full`。不需要维护两套权限框架。
4. **新 Auto 的判定依据是「当前调用可直接识别的不可逆后果」**，不是工具名、命令复杂度、工作区边界或抽象 RiskLevel。`python script.py` 这类间接副作用不追踪、不审批，这是产品明确接受的风险。
5. **新 Auto 取消普通工作的文件系统写围栏。** 否则会出现「策略 Allow、沙箱必然 Deny」的假放行。取消写围栏不等于 Full：显式规则、凭据/信任边界、所有权、不可逆检测、网络/OS 权限仍然独立生效。
6. **第一档保留旧 Auto 的工作区写沙箱**，这是两档 Auto 最根本的结构差异，也是兼容层必须同时承载两套执行边界的原因。

## 2. 审计范围与仓库状态

| 项 | CodeLeveler | Grok Build（仅参考） |
|---|---|---|
| 路径 | `/Users/mengmian/Develop/app/dengmengmian/CodeLeveler` | `/Users/mengmian/Develop/app/other/grok-build` |
| 分支 | `main` | `main` |
| HEAD | `5800dd8a`（2026-11-24 14:31 +0800） | `2bdd1d6a`（2026-09-29） |
| 工作区 | 有未提交改动：上一轮 friction patch + 并发 web-latency / web-search / doctor / memory 改动 | clean |
| 版本 | `v1.0.28-19-g5800dd8a`；最新 tag `v1.0.28` | 源码 `SOURCE_REV=559751fd…`（≠ git HEAD）；已安装二进制 `1.0.92 (b06756fd6256)`；三者不一致，行为证据只能来自实际运行的二进制 |
| 用途 | 被改造对象 | 只作为「确定性快路径 + 结构化授权单元」的参考，不复制其权限模型 |

**基线边界：** 本文所说「旧 Auto」= `5800dd8a` 的 `PermissionProfile::Assisted` 行为 **加上** 工作区未提交的 friction patch（`docs/dev/auto-permission-friction/evidence/auto-permission-friction.patch`）。该 patch 已把子 Agent 未 claim 编辑/子 Agent MCP 改为返回模型的 Deny、把 MCP 的 "this turn" 授权改为工具级。第一档必须继承 patch 后的行为，不能回退到 patch 前。

Grok Build 的参考结论来自 `docs/dev/auto-permission-friction/02-grok-build-comparison.md`，本轮只核对了仓库状态与关键路径。Grok 的 Auto 依赖 heuristic + LLM 分类（`xai-grok-workspace/src/permission/auto_mode/mod.rs`），本轮明确**不引入**动作级 LLM 审查。

## 3. 现状：三档与真实判定链

### 3.1 现有档位的真实语义

| 档位 | 当前内部标识 | 真实行为（源码） |
|---|---|---|
| 请求批准 | `request_approval` | 写围栏 = workspace；网络默认关闭（OS 沙箱拒绝，直到用户授予）；`permits()` 直接拒绝 `Destructive`/`Privileged` 工具；命令按危险分类 ASK；网络/破坏性/特权风险 ASK |
| 替我审批 | `assisted` | 写围栏 = workspace；网络默认开启；`permits()` 全放行；危险命令（rm 非工作区字面路径、sudo、host opener、凭据读取、不可绑定 Git remote）ASK；MCP 全 ASK；工作区外文件写 ASK；私有网络 ASK；凭据使用 ASK；定义写入需真人 |
| 完全访问 | `full_access` | 在 `resolve_policy` 最前面短路：跳过 hook、规则、审批、定义同意、凭据检查、所有权；唯一保留是显式 `--deny-network` |

关键源码：

- `crates/leveler-execution/src/risk.rs`：`PermissionProfile`、`RiskLevel`、`WriteScope`、`network_by_default()`、`write_scope()`。
- `crates/leveler-execution/src/approval.rs`：`ApprovalPolicy::evaluate()`、`classify_command()`、`needs_human_consent()`。
- `crates/leveler-agent/src/executor/host.rs`：`resolve_policy()`（hook → admission → 角色 → 所有权 → 只读 overlay → profile → 规则 → 网络/凭据 → 审批）。
- `crates/leveler-tools/src/tool.rs`：`ToolPolicy::write_scope()`、`ToolContext::execution_workspace()`、`write_path_denied()`。
- `crates/leveler-execution/src/command.rs`：`ProcessRequest.write_scope`、`sandbox_command_with_read_denials()`、macOS seatbelt / Linux bwrap / Windows MIC。
- `crates/leveler-execution/src/permission_rules.rs`：`deny > ask > allow`，无匹配则 `NoMatch`。

### 3.2 一个动作被三层判定

```text
模型工具调用
  → 准入策略：Allow / Ask / Deny（看 profile、命令、规则、资源、所有权）
  → 冻结执行策略：WriteScope + NetworkScope + AuthorizationEvidence
  → OS 沙箱：seatbelt / bwrap / leveler-confine.exe 再判定
```

三层资源模型不同，历史上出现了两类缺陷：

1. 策略 Allow + 沙箱 Deny（本地路径 Git remote、子 Agent scoped 构建写）；
2. 结构性拒绝走人审，批准后绕过原边界（子 Agent 未 claim 编辑、子 Agent MCP）。

第 2 类已由 friction patch 修复；第 1 类在新 Auto 中通过取消写围栏自然消失，在第一档中需要独立修复（见 04）。

## 4. 目标产品契约

| 档位 | 内部标识 | 目标行为 | 权限契约 |
|---|---|---|---|
| 请求批准 | `request_approval` | **继承旧 Auto**：常规开发自动执行；保留工作区写沙箱；工作区外文件写 ASK；危险命令 / MCP / 私有网络 / 凭据 / 定义写入按旧 Auto 策略；保留 Session/Project 授权与所有权 | v2 |
| 替我审批 | `assisted` | **新 Auto**：默认自主执行；只对当前调用可直接识别的不可逆破坏 ASK；普通文件读写、工作区外普通写入、Shell/脚本、构建测试、依赖安装、普通 Git、普通网络、普通 MCP、子 Agent/后台任务自动执行 | v2 |
| 完全访问 | `full_access` | 现有契约完全不变：无 CodeLeveler 审批与沙箱限制；显式 `--deny-network` 仍生效；不自动获得 root/Administrator | v1 / v2 等价 |

### 4.1 新 Auto 的明确承诺

自动执行，且**不因以下理由**要求审批：

- 工具名称看起来危险；
- 命令复杂（管道、循环、变量、`$()`、`&&`）；
- 目标在工作区之外；
- 抽象 RiskLevel 为 `Destructive` / `Privileged`；
- 无法证明脚本内部没有副作用；
- 当前信息不足以证明完全安全。

必须审批的只有：**当前调用本身可被结构化识别的、不可恢复的破坏行为**（见 03）。

### 4.2 新 Auto 的明确不承诺

- 不保证发现所有不可逆行为。`python script.py`、`make clean`、`npm run postinstall` 的内部行为不追踪。
- 不把「未知」当「危险」。无法解析的脚本、未知程序直接运行，除非其中直接可见一个已识别的破坏动作。
- 不是「绝对安全」的执行环境，文档不得这样宣传。

## 5. 内部策略解析：契约版本 × 档位

```text
PermissionContract = V1 | V2
PermissionProfile = request_approval | assisted | full_access

policy_kind(contract, profile):
  (_,     full_access)      -> Full
  (V2,    request_approval) -> LegacyAuto      // 第一档 = 旧 Auto
  (V2,    assisted)         -> EffectAuto      // 第二档 = 新 Auto
  (V1,    assisted)         -> LegacyAuto      // 历史「替我审批」= 旧 Auto
  (V1,    request_approval) -> LegacyRestricted // 历史「请求批准」= 旧的更严策略
```

| policy kind | 写围栏 | 网络默认 | 审批依据 | 谁使用 |
|---|---|---|---|---|
| `LegacyRestricted` | workspace | 关闭 | 旧 RequestApproval：Network/Destructive/Privileged ASK；Destructive/Privileged 工具直接拒绝 | 仅 v1 `request_approval` 历史 Session/配置 |
| `LegacyAuto` | workspace | 开启 | 旧 Auto：危险命令/MCP/外部写/私有网络/凭据 ASK | v1 `assisted` 历史 Session；v2 `request_approval` 当前第一档 |
| `EffectAuto` | **无写围栏** | 开启 | 不可逆后果 + 独立边界 | v2 `assisted` 新 Auto |
| `Full` | 无 | 开启 | 无 CodeLeveler 审批 | 任意契约的 `full_access` |

**为什么不是两套实现：** `LegacyAuto` 直接复用现有 `ApprovalPolicy` 的 Assisted 分支、`WriteScope::Workspace`、MCP/凭据/定义写入路径；`EffectAuto` 只替换「判定依据」和「写围栏预设」，复用同一条 `resolve_policy` 主链。`Full` 继续在链首短路。

**为什么需要 v1：** 历史 `request_approval` 是更严的策略，历史 `assisted` 是旧 Auto。V2 把第一档改成旧 Auto、第二档改成新 Auto，两者对旧数据都是「扩大」。没有契约版本，就无法阻止旧 Session 在恢复时静默提权。

> 注意：UI 永远只有三档。契约版本只作为 Session 的内部事实和兼容标记，不出现第四档选择。

## 6. 权限决策矩阵

表中「基础策略」是准入层的 Allow / Ask / Deny；「执行资源」是冻结后的 `WriteScope`/`NetworkScope`；「OS 结果」是真实执行边界；「独立约束」是不由 Auto 自主性撤销的机制。

### 6.1 第一档 · 请求批准（v2，旧 Auto）

| 操作 | 基础策略 | 执行资源能力 | OS 结果 | 独立约束 |
|---|---|---|---|---|
| 读取文件 | Allow | 读不受限（凭据文件名除外） | 成功 | 凭据/信任路径文件工具拒绝 |
| 普通文件修改（工作区内） | Allow | `Workspace{root}` | 成功 | 所有权 claim 仍收窄 |
| 工作区外普通写入 | Ask（工具 admission） | 批准后 `Unrestricted` | 批准成功；拒绝无副作用 | 凭据/信任路径仍拒绝；提示明示扩大写范围 |
| `python script.py` | Allow | `Workspace{root}` | 进程运行；脚本写工作区/临时/缓存之外 → EPERM | 不追踪脚本内部 |
| `npm run build` | Allow | `Workspace{root}` + 缓存/临时 | 成功 | — |
| 复杂 Shell | 逐段分类；有危险段则 Ask | `Workspace{root}` | 安全段成功；工作区/临时/缓存之外的写入 EPERM | `sudo`/host opener/凭据读取仍 Ask |
| Cargo 构建/测试 | Allow | `Workspace{root}` + 缓存 | 成功 | 子 Agent 需 claim 构建产物，否则有 claim 指引 |
| 普通 Git Commit | Allow | `WorkspaceWithGit{root}` | 成功 | — |
| 普通 Git Push | Allow | `WorkspaceWithGit{root}`；网络开启 | 网络 remote 成功；**本地路径 remote 可能 EPERM（已知缺陷，2C 修复）** | 凭据 remote 绑定；不可绑定 remote Ask |
| `git reset --hard` | Allow | `WorkspaceWithGit{root}` | 成功（旧 Auto 语义） | 无 |
| `git clean -fd` | Allow | `WorkspaceWithGit{root}` | 成功（旧 Auto 语义） | 无 |
| `rm -rf` 真实数据 | 相对字面路径 Allow；绝对/`..`/展开 Ask | `Workspace{root}` | 相对路径删除成功；越界 Ask 后成功 | 无（这是第一档与第二档的差异） |
| 清理隔离构建缓存 | `rm -rf target` Allow；`/tmp/...` Ask | `Workspace{root}` | 成功 / 批准后成功 | — |
| 普通 MCP | Ask（首次）；ApproveSession 后同工具本 turn 复用 | MCP 进程无 OS 沙箱 | 成功 | 显式 `--deny-network` → Deny |
| 不可逆 MCP 删除 | Ask | 同上 | 批准后执行 | 无结构化识别；所有 MCP 都 Ask |
| 普通网络访问 | Allow | `NetworkScope::Internet` | 成功 | 私有/局域网 `web_fetch` Ask |
| 子 Agent 未 Claim | Deny（返回模型） | `WriteScope::None`/`ScopedWorkspace` | 无副作用 | 所有权协议；`claim_write_scope` 恢复 |
| 用户显式 Deny | Deny | — | 无副作用 | 规则先于 profile |
| 不可信 Skill/定义 | 使用受信任闸门；定义写入需真人同意 | — | 按信任结果 | 信任存储独立 |

### 6.2 第二档 · 替我审批（v2，新 Auto）

| 操作 | 基础策略 | 执行资源能力 | OS 结果 | 独立约束 |
|---|---|---|---|---|
| 读取文件 | Allow | 读不受限；凭据文件名文件工具拒绝 | 成功 | 凭据/信任路径独立 |
| 普通文件修改（工作区内/外） | Allow | `Unrestricted`（所有权收窄优先） | 成功 | 凭据/信任路径文件工具拒绝 |
| 工作区外普通写入 | Allow | `Unrestricted` | 成功 | 同上；不再有写边界提示 |
| `python script.py` | Allow | `Unrestricted` | 进程运行；脚本副作用按用户 OS 权限发生 | 不追踪内部；这是接受的风险 |
| `npm run build` | Allow | `Unrestricted` | 成功 | — |
| 复杂 Shell | Allow（除非识别到不可逆动作） | `Unrestricted` | 成功 | 显式规则/凭据/所有权仍生效 |
| Cargo 构建/测试 | Allow | `Unrestricted` + 缓存 | 成功 | 子 Agent claim 仍收窄 |
| 普通 Git Commit | Allow | `Unrestricted` | 成功 | — |
| 普通 Git Push | Allow | `Unrestricted` + `Internet` | 成功（含本地路径 remote） | 凭据绑定 |
| `git reset --hard` | **Ask**（`irreversible`） | 批准后 `Unrestricted` | 批准执行；拒绝无副作用 | 显式规则可覆盖 |
| `git clean -fd` | **Ask**（`irreversible`） | 同上 | 同上 | 同上 |
| `rm -rf` 真实数据 | **Ask**（非可重建目标） | 同上 | 同上 | 同上 |
| 清理隔离构建缓存 | Allow（可重建目标） | `Unrestricted` | 成功 | 目标判定见 03 |
| 普通 MCP | Allow | MCP 进程无 OS 沙箱 | 成功 | 显式 `--deny-network` → BoundaryFailure |
| 不可逆 MCP 删除 | **Ask**（`destructiveHint=true`） | 精确调用授权 | 批准执行；拒绝无副作用 | 服务器未声明则无法识别（文档化缺口） |
| 普通网络访问 | Allow | `Internet` | 成功 | 私有/局域网 Ask（独立资源边界） |
| 子 Agent 未 Claim | Deny（返回模型） | `None`/`ScopedWorkspace` | 无副作用 | 所有权协议 |
| 用户显式 Deny | Deny | — | 无副作用 | 规则先于 profile |
| 不可信 Skill/定义 | 信任闸门；定义写入需真人 | — | 按信任结果 | 信任存储独立 |

### 6.3 第三档 · 完全访问（不变）

| 操作 | 基础策略 | 执行资源能力 | OS 结果 | 独立约束 |
|---|---|---|---|---|
| 全部已实现能力 | Allow | `Unrestricted` + `Internet` | 按 OS 权限执行 | 仅保留：显式 `--deny-network`（无法强制则拒绝）、Reviewer/Explorer 只读角色边界、MCP 在显式断网下 Deny、取消/超时/资源回收 |
| 用户显式 Deny rule | **不生效**（Full 在规则前短路） | — | 按命令执行 | 既有契约，V2 不改变 |
| 不可信定义 | 按 Full 的基线采纳规则处理 | — | — | 既有契约 |

> **冲突披露：** 现有 Full 会绕过用户写的 `deny` 规则（`host.rs::resolve_policy` 的 Full 短路在规则评估之前）。因此矩阵中「用户显式 Deny」在 Full 下不是 Deny。这是既有产品契约，本轮要求「不得改变 FullAccess」，所以不修。若产品希望 Full 也尊重 `deny`，必须作为独立决策，不能夹带在本轮。

## 7. 新 Auto 与 Full 的边界

| 机制 | 新 Auto | Full |
|---|---|---|
| 不可逆动作检测（rm/磁盘/Git 历史/MCP destructive） | Ask | 不 Ask |
| 权限规则 `deny` / `ask` / `allow` | 生效 | **不生效** |
| pre-hook Deny | 生效 | **不生效**（Full 短路在 hook 前） |
| 凭据使用 / 凭据库读取 | Ask / 工具层拒绝 | 不 Ask |
| 私有/局域网网络 | Ask | 不 Ask |
| 定义写入真人同意 | 生效 | 不生效 |
| 子 Agent 所有权（未 claim / 越界） | Deny 给模型 | 不生效 |
| 只读角色（Reviewer/Explorer） | 生效 | 生效（角色边界在 Full 之前） |
| 显式 `--deny-network` | 生效；无法强制则 BoundaryFailure | 生效；无法强制则拒绝 |
| 文件系统写围栏 | 无 | 无 |
| 敏感文件名（`.env`/`.ssh`/`.aws`/密钥） | 文件工具拒绝；命令有独立参数闸门 | 文件工具按 Full 既有规则 |
| 定义信任/技能信任 | 生效 | 按 Full 基线采纳 |

结论：新 Auto **不是**「没有边界检查的 Full」。它去掉的是普通写入的路径围栏；保留的是显式配置、信任、所有权、不可逆检测、凭据与网络这些独立约束。Full 去掉的是全部准入判定。

## 8. 必须修改的模块（概览）

| 层 | 模块 | 改动性质 |
|---|---|---|
| 权限核心 | `leveler-execution/src/risk.rs` | 新增 `PermissionContract`、`PolicyKind`；`PolicyKind::write_scope()` / `permits()` |
| 权限核心 | `leveler-execution/src/approval.rs` | 保留旧 Auto 分支；新增 `EffectAuto` 判定与理由码；`Requirement` 增加边界失败语义 |
| 新检测器 | `leveler-execution/src/irreversibility.rs`（新） | 结构化不可逆检测、可重建目标分类、理由码 |
| 准入 | `leveler-agent/src/executor/host.rs` | 按 `PolicyKind` 解析；新 Auto 写围栏；边界失败；理由码写入 `ApprovalRequest` |
| 提示面 | `leveler-agent/src/prompt.rs` | `operating_rules` 改为按 `PolicyKind` 输出；四套文案（LegacyRestricted/LegacyAuto/EffectAuto/Full），不能只按档位名 |
| 工具策略 | `leveler-tools/src/tool.rs` | `ToolPolicy` 持有 `(contract, profile)`；`write_scope()` 按 policy kind；`unrestricted_execution()` 语义不变 |
| 沙箱 | `leveler-execution/src/command.rs`、`windows_sandbox.rs` | 新 Auto 的 `Unrestricted` 路径验证；显式断网的 BoundaryFailure；本地 Git remote 写根 |
| MCP | `leveler-tools/src/mcp.rs`、`mcp/mcp_http.rs` | 类型化 `McpAnnotations`；`destructiveHint` |
| 会话 | `leveler-storage`、`leveler-app` | `permission_contract` 列、迁移、创建/恢复/切换、快照 |
| 协议 | `leveler-client-protocol` | 快照与命令新增可选契约字段；审批理由码 |
| CLI / TUI | `leveler-cli`、`leveler-tui` | `--permission-contract`、v1 标记与升级确认、三档文案 |
| 调度 | `leveler-schedule` | 授权绑定 `(mode, contract)`，旧授权 fail-closed |

## 9. 需要产品确认的开放点

| # | 问题 | 本文默认 | 影响 |
|---|---|---|---|
| 1 | 新 Auto 下私有/局域网 `web_fetch` 是否仍 Ask | 保留 Ask，视为独立资源边界 | 若要求「普通内网也自动」，需删除该 admission reason |
| 2 | Tier 1 的 MCP「this turn」是否继续按工具复用 | 继续（friction patch 已验收） | 若要求按参数边界，会改变已完成的 patch |
| 3 | `rm -rf target` 等可重建目录的判定是否接受「生态系统锚点」启发式 | 接受，闭集 + 锚点 | 见 03 的剩余风险 |
| 4 | Full 是否应尊重用户 `deny` 规则 | 不改变（现有契约） | 需要独立产品决策，不在 V2 范围内 |
| 5 | v1 Session 的升级入口是否允许「重选同档位」即视为采纳 v2 | 默认要求显式契约升级，不因重选同档位隐式采纳 | 见 05 |

未决项不影响 Phase 2A 的契约层落地；1、3、5 在对应阶段开始前定稿即可。

## 10. 十个最终问题的回答

### Q1. 第一档如何继承旧 Auto？

第一档的 v2 `request_approval` 直接映射到 `PolicyKind::LegacyAuto`，复用现有 `ApprovalPolicy` 的 Assisted 分支、`WriteScope::Workspace`、MCP/凭据/定义写入路径和 Session/Project 授权。旧 Auto 的基线是 `5800dd8a` + friction patch；不复制第二套权限实现，也不恢复 patch 前的所有权绕过。详见 02。

### Q2. 新 Auto 如何避免普通审批？

它不再按工具名、命令复杂度、工作区边界或抽象 RiskLevel 提问，只调用一个确定性检测器：解析本次调用的 argv/AST/Git effects/MCP annotations，只有命中「可直接识别的不可恢复破坏」才 Ask。未知程序、复杂 Shell、脚本、构建、普通 Git/网络/MCP 全部 AutoAllow。详见 03。

### Q3. `python script.py` 为什么可以无需审批？

运行时只解析到 `python` + `script.py` 两个词，不读脚本、不分析依赖、不追踪子进程。脚本内部可能删除或联网，但那是用户选择 Auto 时接受的风险。产品契约明确禁止「因为无法证明安全而审批」，也禁止为了「更安全」逐项加回间接审查。详见 03 §1/§5/§8。

### Q4. 哪些明确的破坏操作必须审批？

闭集：真实文件/目录删除（`rm`/`rmdir`/`unlink`/`shred`/`find -delete`/`xargs rm`/`rsync --delete` 且目标不可重建）、文件清空（`truncate -s 0`）、磁盘/分区/设备销毁、内联 SQL 的 DROP/TRUNCATE/DELETE、容器卷删除、`kubectl delete`/`terraform destroy`、Git 历史丢失（`reset --hard`、`clean -f`、`push --force`、`branch -D` 等）、MCP `destructiveHint=true`、主机停机。可重建的构建缓存删除自动执行。详见 03 §4。

### Q5. 工作区外普通写入如何真正执行成功？

新 Auto 的 `PolicyKind::write_scope` 预设为 `Unrestricted`：文件工具用 `Workspace::resolve_for_write(Unrestricted)`，命令用 `ProcessRequest.write_scope = Unrestricted`，因此不再出现「策略允许、沙箱拒绝」。所有权（子 Agent claim）在预设之前收窄，敏感路径/信任存储在工具层独立拒绝。详见 04 §2/§3。

### Q6. 如何避免新 Auto 变成没有边界检查的 Full？

新 Auto 保留：显式 `deny`/`ask` 规则与 hook、凭据使用与凭据库读取、`sudo`、私有网络、定义写入真人同意、定义/技能信任闸门、子 Agent 所有权、不可逆检测、显式 `--deny-network`、OS 权限。Full 在准入链首短路，连规则和所有权都不看。两者在 `resolve_policy` 中的分支位置不同。详见 01 §7、04 §4。

### Q7. 旧 Session 如何防止静默提权？

新增 `permission_contract` 列，旧行一律 V1；恢复按行内契约运行；切换档位不改变契约；采纳 V2 必须显式确认并在同一个 CAS 写中同时更新 `(mode, contract)`；调度授权绑定 `(mode, contract)`，旧 v3 授权 fail closed；项目配置缺省视为 V1；eval 固定 V1。回滚到旧二进制只会更严。详见 05。

### Q8. Full 是否完全不变？

是。`resolve_policy` 的 Full 短路、`full_permission_*` 契约、显式 `--deny-network` 保留、Reviewer/Explorer 角色边界保留；不新增 Ask，不新增沙箱限制。唯一的既有事实需要公开说明：Full 绕过用户 `deny` 规则（短路在规则之前），V2 不改变它。详见 01 §6.3、04 §1。

### Q9. 需要修改哪些具体模块？

`leveler-execution`（risk/approval/irreversibility/git_effects/command/windows_sandbox/policy）、`leveler-tools`（tool/mcp/mcp_http/workspace/editor）、`leveler-agent`（executor/host/prompt）、`leveler-engine`、`leveler-storage`（迁移/session_repo/task_creation_store）、`leveler-project`、`leveler-app`（session/interactive/lib/mcp_service/parallel/schedule_tools/doctor）、`leveler-client-protocol`、`leveler-local-transport`、`leveler-cli`、`leveler-tui`、`leveler-schedule`。逐文件清单见 06 的每个 Phase。

### Q10. 后续开发的准确顺序？

P0 落盘 friction patch → 2A 契约与兼容层（`EffectAuto` 暂映射为 `LegacyAuto`）→ 2B 不可逆检测器并切换 `EffectAuto` → 2C 执行边界一致性 → 2D 四臂真实 Dogfood → 2E 三平台验收与收尾。每个阶段的验收/阻断/回滚见 06；未通过 2A 的「第一档与旧 Auto 决策等价」不得进入 2B。
