# 05 · 历史 Session 与配置兼容

日期：2026-11-24
定位：正式阻断项。没有这一层，任何 V2 实现都不得发布。

## 1. 问题

数据库里已经存在两种 `mode`：

- `request_approval`：旧的更严策略（网络默认关闭、Destructive/Privileged 工具直接拒绝）；
- `assisted`：旧 Auto（工作区写沙箱、危险命令/MCP/外部写审批）。

V2 重排后：

- `request_approval` 变成旧 Auto（更宽）；
- `assisted` 变成新 Auto（更宽、无写围栏）。

如果直接按新含义解释旧行，历史 Session 会在恢复时**静默提权**。这是本轮不可接受的阻断项。

## 2. 版本模型

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionContract {
    /// 1.x 语义。反序列化缺省值，绝不用于新会话。
    #[default]
    V1,
    /// Permission Model V2 语义。
    V2,
}
```

与档位组合成四种策略：

| 契约 | 档位 | PolicyKind | 语义 |
|---|---|---|---|
| V1 | `request_approval` | `LegacyRestricted` | 旧的严格请求批准 |
| V1 | `assisted` | `LegacyAuto` | 旧 Auto |
| V1 | `full_access` | `Full` | 不变 |
| V2 | `request_approval` | `LegacyAuto` | 第一档 = 旧 Auto |
| V2 | `assisted` | `EffectAuto` | 第二档 = 新 Auto |
| V2 | `full_access` | `Full` | 不变 |

`PermissionContract::default()` 是 V1：任何**持久化数据缺少该字段**时都必须落到旧语义。新建会话必须显式写 V2。

## 3. 数据库

### 3.1 `leveler-storage` 迁移（下一个编号 0066）

```sql
-- 0066_permission_contract.sql
ALTER TABLE sessions ADD COLUMN permission_contract INTEGER NOT NULL DEFAULT 1;

-- 0003 的列 DEFAULT 仍可能留下旧名字；它们都是 v1 语义。
UPDATE sessions SET mode = 'request_approval', permission_contract = 1
  WHERE mode = 'plan';
UPDATE sessions SET mode = 'assisted', permission_contract = 1
  WHERE mode = 'workspace_write';

-- 不回写任何其他行：既有 mode 保持不变，契约固定为 1。
```

规则：

- `DEFAULT 1` 只服务迁移前的旧行；新插入路径必须显式绑定 `permission_contract = 2`，不得依赖列默认值。
- 不删除、不改写历史 Session 的 `mode`。
- 迁移可重复执行（SQLite 的 `ALTER TABLE ADD COLUMN` 只执行一次；`UPDATE` 幂等）。
- 回滚安全：旧二进制忽略新列，按旧 `mode` 解释 → 只会更严（见第 10 节）。

### 3.2 读写路径

| 位置 | 改动 |
|---|---|
| `leveler-engine::NewSession` | 新增 `permission_contract: PermissionContract` |
| `leveler-storage::TaskCreationStore::create_task_once` | INSERT 显式写 `permission_contract` |
| `SessionRepository::create_with_model_pin` | 显式写 V2 |
| `SessionRepository::execution()` / `SessionSettingsSnapshot` | 读取 `(mode, contract, sandbox, kind, thinking)` |
| `SessionRepository::edit_at_version` | `SessionEdit::Permission` 支持同时替换 `(mode, contract)` |
| `leveler-app::create_session_with_mode` | 新增 contract 参数；新会话默认 V2 |
| `leveler-app::persisted_permission_profile` | 返回 `(PermissionProfile, PermissionContract)` |

### 3.3 可验证的迁移测试

`crates/leveler-storage/tests/permission_contract_migration.rs`：

1. 在 0065 schema 上插入 3 行：`request_approval`、`assisted`、`full_access`。
2. 跑 0066。
3. 断言 3 行的 `mode` 不变、`permission_contract = 1`。
4. 用新插入路径创建一行，断言 `permission_contract = 2`。
5. 插入 `plan` / `workspace_write` 两行，断言迁移后为 `request_approval` / `assisted` 且契约 1。

`crates/leveler-app/tests/permission_contract_resume.rs`：

1. 构造 v1 `request_approval` Session，恢复 → `LegacyRestricted`。
2. 构造 v1 `assisted` Session，恢复 → `LegacyAuto`。
3. 构造 v2 `request_approval` Session，恢复 → `LegacyAuto`。
4. 构造 v2 `assisted` Session，恢复 → `EffectAuto`。
5. 重启 daemon 后再次恢复，策略不变。

## 4. Session 生命周期

| 动作 | 契约行为 |
|---|---|
| 新建（TUI/CLI/API/调度） | 显式 V2；项目配置强制旧契约时除外 |
| 恢复（TUI/CLI `--resume`/daemon 重启） | 读取行内契约；不因当前版本默认值改变 |
| 切换档位（`SetPermissionProfile`） | 只改 `mode`，**保留契约**；v1 会话按 v1 解释新档位 |
| 显式采纳 V2（`AdoptPermissionContract` 或 CLI `--permission-contract v2`） | 在同一个 CAS 写中同时设置 `(mode, V2)`；先确认，再 supersede 待审批问题 |
| 运行中切换 | 与现有 `SetPermissionProfile` 一致：更新 live cell、supersede 待审批、下一决策生效 |
| 崩溃/重启 | 恢复时以行内 `(mode, contract)` 为准；未完成的审批按行内策略重新解析 |
| 删除 | 无特殊处理 |

### 4.1 显式采纳的交互

**TUI：**

1. 打开 v1 会话时，状态栏档位旁显示 `旧契约` 标记。
2. `/permission` 选择器顶部显示：「此会话使用 1.x 权限契约。升级后当前档位将采用新语义（更自主；第二档不再有工作区写沙箱）。」
3. 选择档位时：
   - 默认只切换档位，保持 v1；
   - 单独的「升级到新契约」确认按钮/快捷键才发送 `contract = V2`；
   - 确认文案必须列出「更自主」和「更严格」两方面（例如 `rm -rf src` 从自动变为需批准）。
4. 采纳与现有 `SetPermissionProfile` 同一路径：立即更新 live cell、supersede 待审批问题、下一个决策生效；不新增「回合结束后才生效」的第二套机制。若采纳发生在回合中，已授权的调用不受影响（冻结策略），后续调用使用新状态。

**CLI：**

- 新会话：`--permission` 可省略；省略时用项目默认或内置 `assisted`，契约 V2。
- 已有会话：`--permission X` 只改档位，保留契约；要采纳 V2 必须同时给 `--permission-contract v2`，否则在 stderr 明确提示「已按旧契约切换；如需新契约请加 --permission-contract v2」。
- `--permission-contract v1` 只允许用于已有会话的显式降级？**不允许**：V1 没有可见入口，避免制造第四档。该参数只接受 `v2`（采纳）或省略。
- 非交互 `leveler run`：新建会话默认 V2；`--permission-contract v2` 是显式写法，便于脚本自证。
- 如果新建会话因项目配置缺省契约而落到 V1，stderr 必须打印一行明确提示（「项目配置使用旧权限契约；本次按旧语义运行；升级请加 permission_contract: 2」），不得静默。

**协议：**

```rust
ClientCommand::SetPermissionProfile {
    session_id: SessionId,
    mode: PermissionProfile,
    /// None = keep the session's stored contract.
    #[serde(default)]
    contract: Option<PermissionContract>,
}
```

`CreateSessionRequest` 增加 `#[serde(default)] pub permission_contract: Option<PermissionContract>`：`None` 由运行时解析（新会话 V2；旧项目配置可能解析为 V1）。

`UiSessionSnapshot` 增加：

```rust
#[serde(default)]
pub permission_contract: PermissionContract, // 老运行时缺省 = V1
```

UI 只在 `contract == V1` 时显示兼容标记；正常新会话不显示契约版本，避免制造「第四档」的感觉。

### 4.2 模式切换与待审批

现有 `SetPermissionProfile` 会 supersede 该会话的待审批问题（`5d4893d6`）。新增规则：

- 档位切换（契约不变）→ 现有 supersede 逻辑照旧。
- 契约采纳（档位可能同时变）→ 必须 supersede；旧问题是在旧策略下提出的，前提已变。
- Supersede 不是用户决定，不得写入授权记录；重新解析时按新 `(mode, contract)`。
- 如果用户在审批等待中关闭客户端，恢复时按行内契约 + 当前档位重新提出。

## 5. 项目配置与全局默认

### 5.1 `.leveler/config.yaml`

新增可选字段：

```yaml
mode: assisted
permission_contract: 2   # 1 | 2；缺省 = 1（旧语义）
```

解析规则：

| 配置 | 解析 |
|---|---|
| `permission_contract: 2` + `mode: assisted` | 新 Auto |
| `permission_contract: 2` + `mode: request_approval` | 第一档 = 旧 Auto |
| 缺省或 `1` + `mode: assisted` | 旧 Auto（v1） |
| 缺省或 `1` + `mode: request_approval` | 旧的严格请求批准（v1） |
| 任意 + `mode: full_access` | Full |
| 缺省 + 无 `mode` | 内置默认：新会话 V2 + `assisted` |

**旧名字与 V2 的冲突：** `plan` / `workspace_write` 是 v1 名字。若配置写了 `permission_contract: 2` 同时又用旧名字，按 **v1** 解释并告警；不得把 `plan` 在 V2 下解释成第一档（那会把旧的严格策略直接放宽成旧 Auto）。迁移/解析测试必须覆盖这个组合。

实现上不能继续让 `PermissionProfile::parse` 静默把旧名字映射成当前档位。新增 `parse_mode(raw) -> Option<(PermissionProfile, ModeNameKind)>`（`Current` / `Legacy`），由 Session/配置层结合契约字段决定最终 `(mode, contract)`；`parse` 只保留给明确的 v1 读取路径。

**为什么缺省是 V1：** 一个已经写了 `mode: assisted` 的旧项目，其字节无法区分是「上周写的旧 Auto」还是「今天写的新 Auto」。把缺省解释为 V1 是唯一不静默扩权的选择。TUI/CLI 在打开此类项目时提示「项目使用旧权限契约」，并提供一次写回 `permission_contract: 2` 的显式动作。

新项目模板（`leveler init`、文档示例、TUI 写配置）一律写 `permission_contract: 2`。

### 5.2 全局配置

当前没有持久化的全局默认档位（`--permission` 是 CLI 参数）；若有后续新增，同样必须带契约字段。`docs/USAGE` 的示例不得只写 `mode: assisted` 而不写契约。

### 5.3 不可信项目配置

现有规则：不受信任的仓库只能要求更严的默认，不能要求 Full。V2 保持：

- `permission_contract: 2` 本身不构成提权（它只改变档位语义）；
- `mode: full_access` 仍要求项目配置被信任，否则回落到 `assisted`；
- 一个不受信任的仓库写 `mode: assisted, permission_contract: 2` 会得到新 Auto（更宽于旧 Auto）。这是否应被允许？**需要产品确认**：本文默认按现有规则处理（不被信任的项目配置不能要求 Full，但可以要求 Auto），并在 TUI 首次打开时提示「该项目配置请求新 Auto（无工作区写沙箱）」。若产品认为不受信任仓库不能请求 V2 Auto，则回落 V1。

## 6. 调度与无人值守授权

### 6.1 定义

`leveler-schedule` 使用独立 SQLite（下一个迁移编号 0006）：

```sql
ALTER TABLE schedule_definitions
  ADD COLUMN permission_contract INTEGER NOT NULL DEFAULT 1;
```

`ScheduleDefinition`、`NewScheduleDefinition`、协议 draft/view 增加 `permission_contract`。旧行 = V1。

### 6.2 授权绑定

`UnattendedGrant` 当前 v3 绑定 `permission_mode`。V2 必须同时绑定契约：

```rust
pub struct UnattendedGrant {
    pub version: u8,                       // 4
    pub permission_mode: PermissionProfile,
    pub permission_contract: PermissionContract, // 新增
    // …既有字段
}
```

规则：

- 旧 v3 授权在 V2 运行时 **fail closed**（版本不匹配 → 要求重新授权）。理由：旧授权只覆盖旧语义的档位，不能静默覆盖新 Auto。
- 新授权 v4 写契约；`covers()` 同时比较 `(mode, contract)`。
- 调度创建的新 Session 继承定义的 `(mode, contract)`；v1 定义继续以旧语义运行，直到用户显式升级定义。
- 在 TUI 调度页显示「旧契约」标记；升级定义需要重新授权。

### 6.3 验收

- v3 授权 + V2 运行时 → 拒绝调度，提示重新授权。
- v4 授权 + `(assisted, V2)` → 新 Auto 运行。
- 修改定义的契约或档位 → 授权失效。
- 旧定义（V1）dispatch 出的 Session 行 = `(assisted, V1)`，重启后仍为旧 Auto。

## 7. 内部 Session 构造器

这些路径不经过用户档位选择，必须显式声明契约，不能吃「新版本默认 V2」的偶然行为。

| 构造器 | 契约 | 理由 |
|---|---|---|
| `leveler eval`（`eval_cmd.rs`） | **V1（暂定）** | eval 依赖封闭工作区与读拒绝；新 Auto 无写围栏会让被测 Agent 写到 fixture 之外。V2 的 eval 隔离需要单独评审，不在本轮 |
| `parallel.rs` 候选 | 继承父 Session | 候选必须与父任务同一权限/网络/契约 |
| Review / Investigator 子 Session | 继承父 Session | 角色只读边界独立生效；契约跟随父 |
| 后台任务 | 继承创建它的 Session | 不产生新权限 |
| 由模型创建的 Schedule 定义 | 继承创建它的 Session 的契约 | v1 会话创建的调度继续旧语义；避免模型在不理解 V2 的情况下批量创建新 Auto 调度 |
| Foreign session import | V2 | 外部会话从未在 CodeLeveler 契约下运行，不存在旧语义授权 |
| `leveler run` 一次性会话 | V2（默认） | 新会话；显式 `--permission-contract v2` 可自证 |
| 测试 fixture `ToolContext::new(workspace, Assisted)` | V2 | 新构造的上下文属于新世界；需要旧 Auto 的测试显式 `with_permission_contract(V1)` |

**阻断项：** eval 必须在 2A 就固定 V1，并加一条回归测试 `eval_sessions_stay_on_legacy_contract`。否则一旦默认切换，eval 的封闭性会静默失效。

## 8. 客户端与兼容矩阵

| 组合 | 行为 |
|---|---|
| 新 runtime + 旧 client | 旧 client 不发送契约 → `None` → 保持行内契约；快照里的新字段被忽略；正常 |
| 旧 runtime + 新 client | 新 client 发送 `contract` 字段被旧 runtime 忽略（若 serde 拒绝未知字段则需确认；现有 `ClientCommand` 未使用 `deny_unknown_fields`）→ 旧语义；客户端把快照缺省当 V1 → 显示「旧契约」。**安全** |
| 新 runtime + v1 数据库 | 迁移后 0066 给所有旧行 V1；语义与升级前一致 |
| 旧 runtime + v2 数据库 | 旧 runtime 忽略 `permission_contract`；v2 `request_approval` 按旧严格策略、v2 `assisted` 按旧 Auto → 只会更严。**安全** |
| v1 Session 在 TUI 打开 | 显示旧契约标记；默认不改变契约 |
| v1 Session 在 CLI 恢复 | 默认保持 v1；`--permission` 只改档位 |

需要确认：`ClientCommand` 的反序列化是否容忍未知字段。现有枚举大多是 untagged/内部标签；Phase 2A 的协议测试要覆盖「新 client 给旧 runtime 发 contract」与「旧 client 给新 runtime 缺 contract」两种方向。若旧 runtime 拒绝未知字段，则新 client 必须先探测版本或降级发送。

## 9. 不静默提权的可验证不变量

| # | 不变量 | 测试 |
|---|---|---|
| I1 | Session 只按行内契约运行 | `permission_contract_resume` |
| I2 | 档位切换不改变契约 | `switching_tier_keeps_the_contract` |
| I3 | 契约采纳必须显式且原子 | `adopting_v2_writes_mode_and_contract_together` |
| I4 | Full 在 V1/V2 完全一致 | `full_is_identical_under_both_contracts` |
| I5 | 回滚只收紧不放大 | `old_runtime_reads_v2_rows_more_strictly`（用旧策略映射断言，不需要真的跑旧二进制） |
| I6 | 无人值守授权绑定 (mode, contract) | `v3_grant_fails_closed_after_upgrade` |
| I7 | eval 固定 V1 | `eval_sessions_stay_on_legacy_contract` |
| I8 | 项目配置缺省 = V1 | `project_default_without_contract_is_legacy` |
| I9 | 新会话 = V2 | `new_sessions_start_on_v2` |
| I10 | 待审批在契约变更时 supersede | `contract_change_supersedes_pending_approval` |
| I11 | 快照/状态条的契约与行内事实一致 | `snapshot_contract_matches_session_row` |

## 10. 回滚

- **DB：** 0066 只加列；旧二进制忽略它。旧二进制读到 v2 行时：`request_approval` → 旧严格策略（更严）；`assisted` → 旧 Auto（比新 Auto 更严）；`full_access` → 不变。**不会放大权限。**
- **调度授权：** v4 带新字段；旧二进制的 `UnattendedGrant` 使用 `deny_unknown_fields`，反序列化失败 → fail closed。安全。
- **项目配置：** 旧二进制忽略 `permission_contract`（`ProjectConfig` 未使用 `deny_unknown_fields`），按旧 `mode` 解释 → 更严。安全。
- **协议：** 新 client + 旧 runtime 的最坏情况是契约字段被忽略、按旧语义运行 → 更严。
- **代码：** 2A 只引入契约层与默认值；若需要回滚，移除 V2 默认写入即可，v1 路径本来就是旧代码。

## 11. 交付顺序（与 06 对应）

1. 先落 0066 迁移与读写路径（2A）。
2. 再落 `PolicyKind::of()` 与四种策略的等价测试。
3. 最后才允许把新会话默认切到 V2，并同时完成 eval V1 固定与项目配置提示。

在第 3 步之前，所有新会话必须显式 V1，否则会出现「代码已支持 V2、默认还未切换、部分路径意外 V2」的中间态。
