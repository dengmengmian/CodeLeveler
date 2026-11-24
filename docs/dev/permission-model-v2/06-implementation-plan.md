# 06 · 分阶段实施计划

日期：2026-11-24
阶段：Phase 1 只输出计划，不执行。
原则：每个阶段先红后绿；每阶段结束都有可回滚点；未通过阻断项不得进入下一阶段。

## 0. 前置条件

| # | 条件 | 说明 |
|---|---|---|
| P0 | friction patch 单独落盘 | 当前 patch 在工作区未提交，且同树有并发改动。先取 `docs/dev/auto-permission-friction/evidence/auto-permission-friction.patch` 作为权威副本；提交前核对只包含 patch 的内容（需单独授权） |
| P1 | 记录当前 HEAD 与工作区快照 | `git status --porcelain > /tmp/pmv2-baseline-status.txt`；避免并发会话丢失未提交改动 |
| P2 | 确认第 01 篇第 9 节的开放点 | 至少确认 #1（私有网络）、#3（可重建目标）、#5（v1 升级入口） |
| P3 | macOS 本机基线 | `cargo test -p leveler-execution --test auto_permission_decision_table`、`auto_permission_sandbox`、`multi_agent_test` 全绿 |
| P4 | Windows / Linux 宿主可达 | 2C 与 2E 需要真实宿主；不可达时只能标记 `UNVERIFIED`，不能发布 |

## Phase 2A · 权限契约与兼容层

**目标：** 引入 `(contract, profile)` 状态与 `PolicyKind`，把旧语义完整保留；新档位映射先不启用 `EffectAuto`。

### 涉及文件

| 文件 | 改动 |
|---|---|
| `crates/leveler-execution/src/risk.rs` | `PermissionContract`、`PolicyKind`、`of()/permits()/write_scope()/network_by_default()` |
| `crates/leveler-execution/src/lib.rs` | 导出 |
| `crates/leveler-execution/src/approval.rs` | `evaluate` 接受 `PolicyKind`；旧分支原样绑定 `LegacyRestricted`/`LegacyAuto` |
| `crates/leveler-tools/src/tool.rs` | `ToolPolicy` 保存 `(contract, profile)`；`write_scope()` 按 `PolicyKind`；`mode()` 保留 |
| `crates/leveler-agent/src/executor.rs` | 执行器构造传入契约；live cell 同时保存两个字段 |
| `crates/leveler-agent/src/executor/host.rs` | `resolve_policy` 读 `PolicyKind`；Full 短路不变 |
| `crates/leveler-agent/src/prompt.rs` | `operating_rules` 按 `PolicyKind` 输出四套文案；`TurnContext` 携带契约 |
| `crates/leveler-agent/src/executor/handlers.rs` | `request_permissions` 的 Full 判定不变；契约切换 supersede |
| `crates/leveler-engine/src/engine.rs` | `NewSession.permission_contract` |
| `crates/leveler-storage/src/task_creation_store.rs`、`session_repo.rs` | INSERT/SELECT/EDIT 契约列 |
| `crates/leveler-storage/migrations/0066_permission_contract.sql` | 新增列 + 旧名字归一 |
| `crates/leveler-app/src/session.rs`、`interactive.rs`、`lib.rs` | 创建/恢复/切换/快照；`permission_profile_for(scope, contract, mode)` |
| `crates/leveler-app/src/mcp_service.rs` | `admission_for(contract, mode)`：V2 request_approval 不再要求 network grant |
| `crates/leveler-project/src/config.rs` | `permission_contract` 可选字段；旧名字 + V2 的冲突规则 |
| `crates/leveler-app/src/parallel.rs` | 候选继承父契约 |
| `crates/leveler-app/src/schedule_tools.rs`、`interactive/review.rs` | 模型创建的调度/审查子会话继承创建者的契约 |
| `crates/leveler-client-protocol/src/wire_types.rs`、`command.rs`、`snapshot.rs` | 新增可选字段 |
| `crates/leveler-local-transport/src/lib.rs` | `CreateSessionRequest` 新增可选契约；远端边界不得借此提权 |
| `crates/leveler-cli/src/cli.rs`、`common.rs`、`run_cmds.rs`、`eval_cmd.rs`、`schedule_cmd.rs` | `--permission-contract v2`；eval 固定 V1 |
| `crates/leveler-cli/src/desktop_bridge.rs` | 会话/调度 draft 携带契约；不得用默认值静默升级 |
| `crates/leveler-schedule/src/lib.rs`、`store.rs`、`migrations/0006_*` | 定义与授权绑定契约 |
| TUI 状态/快照 | 读取 `permission_contract`；显示旧契约标记（先只读） |

### 实施步骤

1. **写测试（红）：** `PolicyKind::of` 六种组合；`SessionRepository` 迁移；`PersistedSettings` 恢复；`SetPermissionProfile` 保留契约；`UnattendedGrant` v3 fail-closed。**先加最小骨架并保持当前行为**（例如 `PolicyKind::of` 先全部返回 `LegacyRestricted`/`LegacyAuto` 的现有答案），再写行为断言；红必须是「断言不符」，不能是编译失败或 fixture 错误。迁移测试用真实旧 schema 建库，红是「列不存在」这个目标行为缺失，不是测试环境错误。
2. 加 `PermissionContract` / `PolicyKind`，先用 `PolicyKind::of` 把 `(V2, assisted)` 映射为 `LegacyAuto`（保守中间态）。
3. 加 0066 迁移与所有读写路径；**新插入路径写 V1**，直到第 7 步统一切 V2。
4. 把 `ApprovalPolicy::evaluate` 的旧分支绑定到 `PolicyKind`；`PermissionProfile::permits/write_scope` 改为委托 `PolicyKind`，保留旧测试的语义。
5. 加 `SetPermissionProfile.contract` 与显式采纳命令；TUI 先只显示标记，不做交互。
6. 加调度契约列与 v3 fail-closed。
7. 新会话默认切 V2；eval 固定 V1；parallel/review/后台继承；项目配置缺省 V1。
8. 跑全部既有权限测试与新增契约测试。

### 测试

- `crates/leveler-execution/tests/policy_kind_matrix.rs`：6 种 (contract, tier) → policy kind。
- `crates/leveler-project` 的配置解析测试：缺省 V1、显式 V2、`plan`/`workspace_write` + V2 冲突。
- `crates/leveler-storage/tests/permission_contract_migration.rs`。
- `crates/leveler-app/tests/permission_contract_resume.rs`。
- `crates/leveler-app/tests/permission_contract_switch.rs`（切档位保留契约、采纳原子写、supersede）。
- `crates/leveler-schedule/tests/unattended_contract.rs`。
- 既有 `permission_persistence.rs`、`live_permission_profile.rs`、`full_permission_*`、`multi_agent_test` 全绿。

### 验收

| # | 标准 |
|---|---|
| A1 | v1 `request_approval` = 旧严格策略；v1 `assisted` = 旧 Auto；两者与 2A 前决策逐项相同 |
| A2 | v2 `request_approval` = 旧 Auto；v2 `assisted`（中间态）= 旧 Auto |
| A3 | `full_access` 在两种契约下完全相同；`full_permission_dogfood` 全绿 |
| A4 | 新会话写 V2；v1 行恢复不改变语义 |
| A5 | 调度 v3 授权 fail closed；v4 绑定 `(mode, contract)` |
| A6 | eval 固定 V1，回归测试证明 |
| A7 | 协议双向兼容：旧 client/新 runtime、新 client/旧 runtime 均不放大权限 |

### 风险

| 风险 | 缓解 |
|---|---|
| `SharedPermissionProfile` 从一个原子量变两个字段，可能出现撕裂读 | 用一个 `AtomicU16` 编码 `(contract, profile)`；`get()` 返回整体；测试并发读写 |
| 默认切 V2 后大量工具单测失败（它们假设 Assisted 有 workspace 围栏） | 先跑 `cargo test -p leveler-tools --lib`；需要旧语义的测试显式 `with_permission_contract(V1)`；不要把断言改成新语义来掩盖 |
| 内部构造器漏传契约 | `SharedPermissionProfile::new` 改为必须给契约；编译期强制 |
| 迁移默认值被新插入路径依赖 | 新插入显式写 2；测试断言 `DEFAULT 1` 不会出现在新行 |

### 回滚

- 迁移只加列；回滚代码即可，DB 不需要 down migration。
- 新会话默认若出问题，把默认写回 V1（一处常量 + 一处 insert 绑定）。
- 2A 不启用 `EffectAuto`，因此行为回滚 = 旧策略。

## Phase 2B · 新 Auto 决策引擎

**目标：** 实现不可逆检测器，把 `(V2, assisted)` 从 `LegacyAuto` 切到 `EffectAuto`。

### 涉及文件

| 文件 | 改动 |
|---|---|
| `crates/leveler-execution/src/irreversibility.rs`（新） | `EffectInput`、`EffectVerdict`、`IrreversibleFacts`、闭集检测、可重建目标分类、理由码 |
| `crates/leveler-execution/src/approval.rs` | `EffectAuto` 分支；`ApprovalRequest.reason_code`；`Requirement::Unsupported` |
| `crates/leveler-execution/src/git_effects.rs` | `filter-branch`/`filter-repo`/`reflog expire`/`gc --prune=now` 标记 irreversible |
| `crates/leveler-tools/src/mcp.rs`、`mcp/mcp_http.rs` | 类型化 `McpAnnotations`；stdio + HTTP 解析 |
| `crates/leveler-tools/src/tool.rs` | `Tool::mcp_annotations()` 默认方法 |
| `crates/leveler-agent/src/executor/host.rs` | 在规则之后、凭据之前调用检测器；理由码写入 `ApprovalRequest`；`Executor::ask` 对 `irreversible.mcp_destructive` 不写工具级 turn key；`decisions()` 不提供 `ApproveAlways` |
| `crates/leveler-engine/src/event.rs` | `ApprovalRequested.reason_code` |
| `crates/leveler-client-protocol/src/event.rs` | `UiApprovalRequest.reason_code` / facts |
| `crates/leveler-tui/src/overlay/approval.rs` | 显示理由码与目标列表 |

### 实施步骤

1. **写红测试：** 第 03 篇第 9.1 节的决策表；先加 `effect_auto_*` 的占位实现（返回当前）；确认失败原因是「应 Ask 却 Auto / 应 Auto 却 Ask」，不是编译错误。真实执行测试可以先写成 `#[ignore]` 或独立 target，避免环境错误冒充红。
2. 实现 `irreversibility.rs` 的纯函数，先不接线；用单元测试喂 argv/AST。
3. 补 Git effects 缺口；用 `git_permission` 与新测试锁住。
4. 实现 MCP annotations 解析（stdio + HTTP）；用现有 MCP echo server 测试 typed 字段。
5. 把 effect analysis 接到 `host.rs`：先只解析、把删除/写入目标加入 `rule_paths`，再评估规则；facts 在规则之后用于 Ask。保留第一档完全不调用。
6. 加理由码到事件/TUI；断言稳定字符串。
7. 把 `PolicyKind::of` 的 `(V2, assisted)` 切到 `EffectAuto`；跑 2A 所有测试 + 新测试。
8. 跑 `auto_permission_sandbox` 的真实执行；补新 Auto 越界写与删除场景。

### 验收

| # | 标准 |
|---|---|
| B1 | 普通开发 0 审批：读、写、shell、构建、依赖、普通 Git、普通网络、普通 MCP、子 Agent/后台 |
| B2 | 闭集不可逆动作 100% Ask，且提示列出具体对象 |
| B3 | `python script.py`、`npm run build`、未知程序、`$()` 失败脚本 0 审批 |
| B4 | `rm -rf target`（有 `Cargo.toml`）0 审批；`rm -rf src` Ask |
| B5 | `git reset --hard` / `clean -fd` / `push --force` Ask；普通 push/commit Allow |
| B6 | `destructiveHint=true` MCP Ask；普通 MCP Allow |
| B7 | 显式 deny/ask 规则、凭据、私有网络、sudo、定义写入仍生效 |
| B8 | 不新增任何模型调用（对比 2A 基线的 model request 计数） |
| B9 | Full 行为零变化 |

### 风险

| 风险 | 缓解 |
|---|---|
| 检测器把旧分类器的 `Dangerous` 全搬进来，导致审批回潮 | 只用第 03 篇闭集；测试断言 `sudo` 之外的旧危险项（`open`、非工作区 rm、私有网络）按新契约处理 |
| 可重建目标误判 | 锚点 + 测试；默认保守（不确定就不在闭集里 → Ask） |
| MCP annotations 只在 HTTP 支持 | stdio 与 HTTP 同一 `McpToolInfo` 字段；两个 transport 各一条测试 |
| 事件新增字段破坏旧回放 | `#[serde(default)]`；金样测试 |

### 回滚

- 一处映射：`(V2, assisted) -> EffectAuto` 改回 `LegacyAuto`。
- 检测器代码可保留但不再被调用；不删除测试。
- 迁移与契约层不受影响。

## Phase 2C · 执行边界一致性

**目标：** 新 Auto 的普通写入真正成功；第一档的本地 Git remote 修复；无法强制的边界变成 `ExecutionBoundaryFailure`。

### 涉及文件

| 文件 | 改动 |
|---|---|
| `crates/leveler-tools/src/tool.rs` | `PolicyKind::write_scope` 接入（若 2A 未完成）；敏感/信任优先级测试 |
| `crates/leveler-tools/src/workspace/editor.rs` | `editing_workspace()` 按冻结的 `WriteScope` 选择 root；`EffectAuto` 使用文件系统根但保留敏感检查 |
| `crates/leveler-execution/src/command.rs` | `Unrestricted` 路径验证；`sandbox_write_roots` 追加；Loopback/Unsupported 的准入映射 |
| `crates/leveler-execution/src/windows_sandbox.rs` | `network_deny` 探测；`assert_intent_spawn_allowed` 的 BoundaryFailure 语义 |
| `crates/leveler-execution/src/policy.rs` | `PolicyDenial.kind`：`Explicit` / `BoundaryFailure` |
| `crates/leveler-execution/src/git_effects.rs` / `git_isolation.rs` | 本地 remote 目标解析 |
| `crates/leveler-agent/src/executor/host.rs` | 网络强制判定按「冻结的 scope 是否能强制」而不是「是否 profile 默认」；本地 remote 写根 |
| `crates/leveler-tools/src/tools/command_execution.rs` | 传递 `sandbox_write_roots` |
| `crates/leveler-app/src/doctor.rs` | 显示契约、写围栏模型、网络强制能力 |

### 实施步骤

1. **写红测试：** 新 Auto 越界写失败（当前写围栏）；第一档 local remote push 失败；Windows deny 在 spawn 才失败。
2. 把 `EffectAuto` 的预设写围栏设为 `Unrestricted`；验证 `execution_workspace()` 不设置 `unrestricted_access`（敏感检查保留）。
3. 验证所有权优先级：子 Agent 有 claim 仍是 `ScopedWorkspace`，无 claim 仍是 `None`。
4. 实现本地 Git remote 解析与 `sandbox_write_roots`；对敏感目标保持 Ask。
5. 把网络强制判定改为「冻结 scope + 宿主能力」；Windows/Linux 不可强制 → `ExecutionBoundaryFailure`。
6. Linux 启动探测 `unshare`/bwrap 能力；缓存结果。
7. 修 `Loopback`/`LocalLan`/`ConfiguredRemote` 在不可表达平台上的准入拒绝。
8. `doctor` 输出写围栏模型与网络能力；避免用户误以为新 Auto 还有 workspace 沙箱。

### 验收

| # | 标准 |
|---|---|
| C1 | 新 Auto 文件工具/命令写工作区外成功；0 审批 |
| C2 | 第一档越界写仍 Ask；子 Agent claim 仍收窄 |
| C3 | 第一档 local remote push 0 审批、0 失败 |
| C4 | Windows 显式断网 + 命令 = BoundaryFailure（需 Windows 宿主） |
| C5 | macOS/Linux 显式断网真实生效 |
| C6 | MCP + 显式断网 = BoundaryFailure，0 执行 |
| C7 | 敏感路径写入 = ExplicitDeny，无审批、无写入 |
| C8 | 没有「Policy Allow → spawn 才失败」的新增路径 |

### 风险

| 风险 | 缓解 |
|---|---|
| Windows 无 network deny，BoundaryFailure 可能让某些工作流不可用 | 明确提示「该平台不能对命令执行此限制」；网络工具仍可在进程内拒绝；不得自动放宽，也不建议为此切 Full |
| Linux user namespace 禁用 | 探测 + BoundaryFailure；文档写明 |
| 本地 remote 写根被滥用（repo 配置指向敏感路径） | 只接受可解析的本地 Git 仓库；敏感路径拒绝；写根是宿主权威，不读模型文本 |
| 新 Auto 无写围栏被误认为 Full | `doctor`/文案/矩阵明确；Full 仍然跳过规则与所有权 |

### 回滚

- 每个平台边界改动独立；回滚 `PolicyKind::write_scope` 即可让新 Auto 回到 workspace 围栏（但会重新引入「策略允许、沙箱拒绝」，只能作为紧急回滚，不能作为发布状态）。
- 本地 remote 写根回滚 = 恢复旧的 Ask/失败路径。

## Phase 2D · 真实 Dogfood

**目标：** 四臂对比，证明「普通开发零不必要审批、明确不可逆触发批准、硬边界有效」。

### 四臂

| 臂 | 配置 |
|---|---|
| 旧 Auto | 2A 之前的 `assisted`（v1）或保留一个 v1 Session |
| 第一档 | v2 `request_approval` |
| 新 Auto | v2 `assisted` |
| Full | `full_access` |

### 场景

| 组 | 场景 | 期望 |
|---|---|---|
| A 日常 | 读/改/构建/测试/fmt | 第一档、新 Auto：0 审批 |
| B 复杂 Shell | 管道、循环、变量、`$()`、重定向、`/tmp` | 第一档按旧规则；新 Auto 0 审批 |
| C 脚本间接副作用 | `python script.py` 删除工作区外文件 | 新 Auto 0 审批；观察副作用；不声称拦截 |
| D 工作区外写 | `write_file` / `sh -c 'echo > ../x'` | 第一档 Ask；新 Auto 0 审批且成功 |
| E Git | commit/push/reset --hard/clean -fd/push --force | 第一档全 0；新 Auto 仅不可逆项 Ask |
| F 删除 | `rm -rf src` / `rm -rf target` / `rm -rf /tmp/x` / 不可读目标 | 新 Auto：src Ask，其余 0；第一档按旧规则 |
| G MCP | 普通/readOnly/destructiveHint | 新 Auto：普通 0，破坏性 Ask；第一档同旧 Auto |
| H 子 Agent | claim / 未 claim / 构建写 | 两档：未 claim Deny；claim 成功 |
| I 边界 | 私有网络、凭据、sudo、定义写入 | 两档按独立边界 |
| J 规则 | 显式 deny / ask / degraded | 两档 Deny/Ask |
| K Full | 上述危险项 | 0 审批；显式断网仍拒绝 |

### 指标

- 审批数 / 100 次工具调用（总与分组）。
- 完成率、Blocked 率、真实失败率。
- 模型请求数（必须不高于基线；新 Auto 不引入分类器）。
- 每个 Ask 的理由码分布；是否有「同一动作重复 Ask」。
- 安全事件：批准后无副作用、拒绝后无副作用、Full 契约违反。

### 验收

| # | 标准 |
|---|---|
| D1 | 新 Auto 在 A/B/C/E 的审批率显著低于第一档，且 A 为 0 |
| D2 | 新 Auto 对 F/E 的闭集不可逆动作 100% Ask；对可重建目标 0 Ask |
| D3 | 第一档与旧 Auto 的判定逐项一致 |
| D4 | 没有任何「批准后仍失败」的新案例 |
| D5 | Full 0 审批；显式断网仍生效 |
| D6 | 模型请求数不增加 |
| D7 | 子 Agent/后台/所有权无回归 |

**禁止：** 为了跑通场景而删除真实文件、格式化磁盘、推送 force 到真实远端。所有危险动作只在隔离 fixture / `/tmp` 中执行。

## Phase 2E · 跨平台与收尾

**目标：** 三个平台的真实验证、历史兼容、文档、最终报告。

### 平台矩阵

| 平台 | 必须实测 |
|---|---|
| macOS | 新 Auto 越界写、第一档围栏、seatbelt 断网、local remote、子 Agent |
| Windows | MIC 第一档围栏、新 Auto 普通写、显式断网 BoundaryFailure、local remote、进程树 |
| Linux | bwrap/fence 第一档、新 Auto 普通写、unshare 断网、user namespace 不可用时的 BoundaryFailure |

不可达平台必须写 `UNVERIFIED`，不得用交叉编译代替。

### 文档

| 文件 | 更新 |
|---|---|
| `SECURITY.md` | 三档新语义；新 Auto 无写围栏；不可逆检测范围与不检测清单；运行时控制的诚实说明；Full 不尊重 deny 的既有事实 |
| `docs/USAGE.md` / `docs/USAGE.zh-CN.md` | 三档说明、`--permission-contract`、升级 v1 会话、配置示例 |
| TUI i18n | 三档描述、旧契约标记、升级确认、审批理由码 |
| CLI help | `--permission-contract v2`；`--permission` 在已有会话上的行为 |
| `docs/dev/permission-model-v2/` | 最终验收记录、已知限制 |

### 验收

| # | 标准 |
|---|---|
| E1 | macOS/Windows/Linux 证据齐全，或明确 `UNVERIFIED` 且不发布 |
| E2 | v1 数据库升级、恢复、切换、回滚全部通过 |
| E3 | 全 workspace 测试、fmt、clippy 通过（按 AGENTS.md 的 gate 范围） |
| E4 | 最终报告回答第 01 篇/Final Report 的 10 个问题 |
| E5 | 无临时文件、无调试代码、无未使用项 |

## 依赖顺序

```text
P0 → 2A ──→ 2B ──→ 2C ──→ 2D ──→ 2E
             └──────────────┘
2A 的契约层是 2B/2C 的前置；2B 的检测器与 2C 的边界可以并行开发，
但 2C 的验收必须在 2B 之后跑一遍（写围栏变化会改变 Ask/执行结果）。
```

## 全局风险登记

| # | 风险 | 级别 | 缓解 |
|---|---|---|---|
| R1 | 历史 Session 静默提权 | 高 | 契约版本 + 显式采纳 + I1–I10 测试 |
| R2 | 新 Auto 无写围栏被当成 Full | 高 | 四条独立边界；doctor/文案；矩阵测试 |
| R3 | eval 封闭性被新默认破坏 | 高 | eval 固定 V1 + 回归测试 |
| R4 | Windows/Linux 未实机 | 高 | 2E 阻断；`UNVERIFIED` 标记 |
| R5 | 可重建目标误判删除真实数据 | 中 | 锚点 + 保守闭集 + 显式规则 |
| R6 | MCP 服务器谎报 annotations | 中 | 文档；显式 ask 规则；配置即信任 |
| R7 | 旧 Auto 的本地 Git remote 仍失败 | 中 | 2C 精确写根 |
| R8 | 测试 fixture 假设旧 Assisted 围栏，造成大面积假红 | 中 | 显式 `with_permission_contract(V1)`；不篡改断言 |
| R9 | 并发会话改动被覆盖 | 中 | 开工前快照；每次验证前重查 HEAD（历史教训） |
| R10 | 审批提示文案未说明「更自主」 | 中 | TUI 升级确认列出双向变化 |

## 最终交付判定

Permission Model V2 可以进入实现，当且仅当：

1. 第 01 篇的开放点至少 #1/#3/#5 已有明确答复；
2. 2A 的契约矩阵与迁移测试先红后绿；
3. 第一档与旧 Auto 的决策等价有机器可读证据；
4. 新 Auto 的不可逆闭集有决策表 + 真实执行双层测试；
5. 新 Auto 不新增模型调用；
6. Windows/Linux 的证据缺口被如实标注，而不是用 macOS 推断。

未满足任一条 → 停在当前阶段，不进入下一阶段。
