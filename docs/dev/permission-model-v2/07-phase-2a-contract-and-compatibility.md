# 07 · Phase 2A 报告：权限契约与历史兼容层

日期：2026-11-24
阶段结论：**PASS（macOS 本地）**
范围：`(contract, profile)` 状态、旧语义保留、存储迁移、显式采纳、调度授权契约、eval 固定 V1。

## 1. 实现结果

| 项 | 结果 |
|---|---|
| 契约状态 | `PermissionContract { V1, V2 }`，`SharedPermissionProfile` 用一个 `AtomicU16` 同时保存契约与档位；`set(profile)` 只改档位，`set_state` 原子改两者 |
| 策略折叠 | `PolicyKind::of` 六种组合折叠为 `LegacyRestricted` / `LegacyAuto` / `EffectAuto` / `Full` 四种实现 |
| 默认值 | `PermissionContract::default() = V1`；`SharedPermissionProfile::new(profile)` = V1（失败安全）；新会话显式写 V2 |
| 存储 | `0066_permission_contract.sql`：新增 `permission_contract` 列，默认 1；旧 `plan`/`workspace_write` 归一为 V1 模式 |
| 切换 | `SetPermissionProfile.contract: Option<...>`：`None` 保留会话契约；`Some(V2)` 才是显式采纳；采纳与档位在同一写入中落盘 |
| 项目配置 | 无 `mode:` → 新会话 V2；有 `mode:` 但无 `permission_contract` → V1；`workspace_write`/`plan` 名字强制 V1；显式 `permission_contract: v2` → V2 |
| 调度 | 定义新增 `permission_contract`；`UnattendedGrant::VERSION = 4` 绑定 `(mode, contract)`；v3 及更早 fail-closed |
| eval | `eval_permission_state()` 固定 `(V1, Assisted)`，并有单测 |
| 协议 | 快照/命令/事件新增契约字段；golden 仅新增 `"permission_contract": "v1"` |

## 2. 组合矩阵（已验证）

| contract | profile | PolicyKind | 写围栏 | 网络默认 | 说明 |
|---|---|---|---|---|---|
| V1 | request_approval | LegacyRestricted | workspace | 关闭 | 旧严格策略，历史数据 |
| V1 | assisted | LegacyAuto | workspace | 开启 | 旧 Auto，历史数据 |
| V1 | full_access | Full | 无 | 开启 | 完全访问 |
| V2 | request_approval | LegacyAuto | workspace | 开启 | 第一档继承旧 Auto |
| V2 | assisted | EffectAuto | 无 | 开启（含私有/LAN） | 第二档新 Auto |
| V2 | full_access | Full | 无 | 开启 | 与 V1 Full 完全相同 |

## 3. 证据

| 证据 | 命令 | 结果 |
|---|---|---|
| 决策表 | `cargo test -p leveler-execution --test policy_kind_decision_table` | 5 passed |
| 旧行迁移 | `cargo test -p leveler-storage --test permission_contract_migration` | 3 passed（含 `switching_the_tier_never_adopts_v2`） |
| 重选不采纳 | `cargo test -p leveler-app --test permission_persistence reselecting_a_v1_tier_does_not_adopt_v2` | passed（含重启后仍为 V2 的显式采纳路径） |
| 项目配置 | `cargo test -p leveler-cli --bin leveler common` | 13 passed（缺省 V1 / 显式 V2 / 旧名字强制 V1 / 无 mode 使用 V2 内置默认） |
| 调度授权 | `cargo test -p leveler-schedule --test scheduled_policies an_unattended_grant_never_covers_a_different_contract` | passed（契约不匹配、v3 均拒绝） |
| eval | `cargo test -p leveler-cli --bin leveler eval_contract` | passed |
| 全量回归 | 见 `13-evidence-index.json` 的 `macos.workspace_tests` | 全绿 |

## 4. 验收

| # | 标准 | 结果 |
|---|---|---|
| A1 | v1 两档与改造前决策一致 | PASS（`v1_assisted` 与 `v2_request_approval` 在 2D dogfood 全部 8 个 case 上审批次数完全一致） |
| A2 | v2 request_approval = 旧 Auto | PASS（策略单测 + dogfood 逐项一致） |
| A3 | Full 两契约相同 | PASS（`full_is_identical_under_both_contracts`；Full 验收矩阵与 dogfood 0 审批） |
| A4 | 新会话写 V2；v1 行恢复不改语义 | PASS（迁移测试 + 持久化测试） |
| A5 | v3 fail-closed；v4 绑定两字段 | PASS |
| A6 | eval 固定 V1 | PASS（代码 + 单测） |
| A7 | 旧 client/新 runtime 不放大权限 | PASS（`contract: None` 保留会话契约，不隐式采纳 V2；协议字段全部可选） |

## 5. 与 Phase 1 计划的偏差

- Phase 1 计划把 2A 做成「`EffectAuto` 暂映射为 `LegacyAuto`」的中间态，再在 2B 切换。本次工作区是一次性实现，最终树中 `EffectAuto` 已启用；2A 的门禁通过单测逐项验证，而不是通过独立提交点验证。
- 计划中的测试文件名（`policy_kind_matrix.rs`、`permission_contract_resume.rs`、`permission_contract_switch.rs`、`unattended_contract.rs`）没有逐字创建，对应能力落在 `policy_kind_decision_table.rs`、`permission_persistence.rs`、`permission_contract_migration.rs`、`scheduled_policies.rs`。
- 未提交、未推送；所有改动仍在工作区。
