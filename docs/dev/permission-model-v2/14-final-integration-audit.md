# 14 · 最终集成审计（Phase F1）

日期：2026-11-24
结论：**审计通过；V2 实现完整存在，契约传播无断点；工作区包含其他并发专项的修改，本轮未触碰、未清理。**
代码状态：HEAD `5800dd8aad887137e86806e10d02ec84f5860c5d`，分支 `main`，工作区未提交。本轮**没有** commit / push / tag / release，也没有新建分支或 worktree。

## 1. 最终 Git 状态

| 项 | 值 |
|---|---|
| HEAD | `5800dd8aad887137e86806e10d02ec84f5860c5d` |
| Branch | `main` |
| 工作区 | 175 个已跟踪文件修改；10 个未跟踪实现/测试/迁移文件；`docs/dev/` 未跟踪 |
| 本轮新增提交 | 无 |
| worktree | 主工作树 + 6 个历史 worktree；本轮未创建或删除 |

主工作树中的未跟踪文件：

| 文件 | 归属 |
|---|---|
| `crates/leveler-execution/src/irreversibility.rs` | V2 |
| `crates/leveler-execution/tests/policy_kind_decision_table.rs` | V2 |
| `crates/leveler-storage/migrations/0066_permission_contract.sql` | V2 |
| `crates/leveler-storage/tests/permission_contract_migration.rs` | V2 |
| `crates/leveler-schedule/migrations/0006_permission_contract.sql` | V2 |
| `crates/leveler-app/tests/permission_model_v2_dogfood.rs` | V2 |
| `crates/leveler-agent/src/executor/web_latency.rs` | 并发专项（web latency），非 V2 |
| `crates/leveler-agent/tests/web_parallel_test.rs` | 并发专项（web parallel），非 V2 |
| `crates/leveler-tools/src/network_budget.rs` | 并发专项（web parallel），非 V2 |
| `docs/dev/` | 内部文档，含本目录 |

## 2. V2 代码完整性

| 层 | 证据 |
|---|---|
| 契约类型 | `leveler-execution/src/risk.rs`：`PermissionContract::{V1,V2}`、`PolicyKind::{LegacyRestricted,LegacyAuto,EffectAuto,Full}`、`SharedPermissionProfile` 原子状态 |
| 决策表 | `policy_kind_decision_table.rs` 5 tests；`LegacyAuto` = V2 request_approval，`EffectAuto` = V2 assisted，Full 两契约一致 |
| 不可逆检测 | `irreversibility.rs` 仅分析当前调用的 argv / typed MCP annotations / workspace，不读脚本内容；`cmd /C` 走独立字符串分类 |
| 执行边界 | `NetworkScope::Any`；`PolicyResolution::BoundaryFailure`；工作区外普通写不再被 fence；显式 `--deny-network` 不可被批准清除 |
| 存储 | `0066_permission_contract.sql` 为 sessions 增加契约列，旧 wire 名 `plan`/`workspace_write` 归一到 V1；`0006_permission_contract.sql` 为 schedule_definitions 增加契约列 |
| 协议 | `CreateSessionRequest` / snapshot / event 带 `permission_contract`；未带字段的调用保持 `None` = 旧语义 |
| 调度 | `dispatch.rs` 把 `definition.permission_contract` 写入 `CreateSessionRequest`；`UnattendedGrant` v4 绑定契约，旧 grant fail-closed |
| CLI / App | 新会话默认 V2 Assisted；无配置或旧配置默认 V1；`--permission-contract v2` 是唯一显式采纳；eval 固定 V1 |
| TUI | `/permission-contract v2`；旧契约显示 `ask·v1` / `auto·v1`；无第四档、无新页面 |
| 子 Agent | 子执行器克隆父 `ToolContext`，共享同一 `SharedPermissionProfile` 单元；所有权拒绝返回模型，不进入审批 |

## 3. 契约传播审计

| 入口 | 传播方式 | 结论 |
|---|---|---|
| CLI `run` / `resume` | `project_permission_default` / 会话行 / 显式 `v2` | 通过 |
| App 会话 | `Application::permission_profile_for(scope, contract, mode)` | 通过 |
| 子 Agent | 父 `tool_context` 克隆 + 共享 policy cell | 通过 |
| 调度 | `dispatch.rs` 读取 definition 契约 | 通过 |
| TUI 重选档位 | `set` 只改 tier，保留 contract；显式采纳才 `set_state` | 通过 |
| Eval | 固定 `(V1, Assisted)` | 通过 |
| MCP 工具 | `ToolRegistry` → `ToolHost` admission；typed annotations | 通过 |
| 独立 CLI 工具（`native`/`lsp`/`memory`） | 不经过模型会话权限决策 | 不适用 |

`ToolContext::new` 的默认契约仍是 V1：这是保守默认。所有会话型执行入口都通过 `with_permission_profile` 覆盖；Full 在两契约下语义相同，因此 `review_live` 的 FullAccess 默认 V1 不改变行为。

## 4. 并发修改隔离

工作区在本轮开始前已包含其他会话的修改。本轮只读取、未触碰：

| 并发专项 | 文件 | 与 V2 的关系 |
|---|---|---|
| Auto Permission Friction | `executor/host.rs` 的 `mcp_session_key`、所有权 Deny 路径、`recoverable.rs` | 已被 V2 集成保留；本轮验证其行为 |
| Web latency / parallel | `executor/web_latency.rs`、`host.rs` 的 `WebPhase` 调用、`network_budget.rs`、`web_parallel_test.rs`、`docs/USAGE` web 段 | 与 V2 正交；`host.rs` 同文件不同 hunk |
| Web search / doctor | `app/src/doctor.rs`、`doctor_text.rs`、`global_config.rs`、`docs/USAGE` search 段 | 与本轮无关 |
| Memory transcript | `app/src/memory/transcript.rs` 丢弃 reasoning | 与本轮无关 |

没有为了“干净工作区”执行 reset、checkout、stash 或删除。没有提交其他会话的改动。

## 5. 迁移与历史数据

| 检查 | 结果 |
|---|---|
| sessions 迁移 | `0066` 增加 `permission_contract INTEGER NOT NULL DEFAULT 1`；只重写 `plan` / `workspace_write` 两个历史 wire 名 |
| schedule 迁移 | `0006` 增加 `permission_contract INTEGER NOT NULL DEFAULT 1` |
| 旧 session 读取 | 缺列/不可读行按 V1 处理，不静默升级 |
| 显式采纳 | 只有 `contract: Some(V2)` / `--permission-contract v2` 升级 |
| 调度授权 | `UnattendedGrant` v4；旧版本 fail-closed |

## 6. 文档与证据

`.gitignore` 的例外规则允许 `docs/dev/permission-model-v2/` 被纳入版本管理；`git check-ignore` 确认该目录**未被忽略**。由于本轮没有提交授权，目录当前为 untracked。原有 Phase 1/2 文档、`13-evidence-index.json`、两份 dogfood JSON 均保留。

## 7. 未解决项

| 项 | 状态 | 说明 |
|---|---|---|
| Windows 实机 | **BLOCKED_ENVIRONMENT** | 无 Windows 主机/VM；UTM 无 guest；Azure/AWS/gcloud CLI 不存在；只有 GNU 交叉编译检查 |
| Linux 完整 tools 套件 | 已补测 | `--init` 容器中 `leveler-tools --lib` 566 passed / 0 failed；无 init 的 `leveler-execution --lib` 16 个失败在 `--init` 下全部消失（449 passed），归因容器 PID 1 / 进程组语义，见 18 |
| 真实模型完成契约摩擦 | 已记录 | `a-dev` 与 `f1-git-push` 出现 goal requirement/citation 反复；`a-dev` 最终 Completed，`f1` 在机械步数上限处停止（业务动作已完成）。属完成契约摩擦，不是权限决策 |
| 主对话 UI | 未改 | 本轮未修改 TUI 布局/颜色/Composer/Goal/Plan/Background/Review/Footer |
