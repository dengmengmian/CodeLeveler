# 12 · Permission Model V2 收尾报告

日期：2026-11-24
总体结论：**PASS（macOS 实机完整；Linux 实机核心范围 PASS；Windows UNVERIFIED）**
代码状态：工作区修改，**未 commit、未 push、未 tag、未 release**。未新建分支，未做破坏性 Git 操作。

## 1. 交付内容

| 层 | 改动 |
|---|---|
| 权限核心 | `PermissionContract` / `PolicyKind` / `SharedPermissionProfile`；`EffectAuto`；不可逆检测器 `irreversibility.rs`；`NetworkScope::Any`；`BoundaryFailure` |
| 工具层 | `ToolPolicy` 按契约读写/网络边界；typed MCP annotations；browser/MCP/HTTP 的 `Any` 守卫；`editing_workspace` 无围栏写入；Git 本地 push 写根 |
| Agent | `resolve_policy` 结构化效果判定；直接不可逆 ASK；私有网络/已持有权限不额外 ASK；read-only overlay 按 PolicyKind；Loopback 收窄修复 |
| 存储 | 0066 迁移；Session `permission_contract`；调度 0006；`UnattendedGrant` v4 |
| 协议 | 快照/命令/事件契约字段；golden 仅新增字段 |
| App/CLI | 显式采纳、V1 保留、项目默认、eval 固定 V1、调度命令、desktop bridge |
| TUI | `/permission-contract v2`；`ask·v1` / `auto·v1` 旧契约标记；无第四档、无新页面 |
| 文档 | Phase 2 报告 07–12 + 证据索引 + 两份 dogfood JSON；`.gitignore` 仅放行本契约目录 |

## 2. 门禁

| Gate | 结果 | 说明 |
|---|---|---|
| 2A 契约与兼容 | PASS | 矩阵、迁移、重选不采纳、调度 fail-closed、eval 固定 |
| 2B EffectAuto | PASS | 普通脚本 0 审批；真实删除/Git 历史 ASK；不读脚本；包装器剥离 |
| 2C 执行边界 | PASS | 工作区外写入/私有 LAN/本地 push 真实成功；拒绝后无副作用；所有权 deny 不可绕过 |
| 2D Dogfood | PASS | 5 臂 × 8 case，macOS 0 断言失败 |
| 2E 跨平台 | macOS PASS / Linux PARTIAL PASS / Windows UNVERIFIED | 真实宿主，不冒充 |

## 3. 关键行为

- 新会话默认 V2 Assisted（EffectAuto）；V1 会话按行内契约恢复。
- 重选档位不改变契约；只有 `contract: Some(V2)` 显式采纳。
- 第一档（V2 request_approval）与旧 Auto 在 macOS/Linux dogfood 的全部可测 case 上一致。
- Full 在两契约下完全相同；dogfood 全 0 审批，删除真实执行。
- 显式 `--deny-network` 永远不被批准清除；不可约束的 MCP 返回 `BoundaryFailure`。

## 4. 剩余问题

| 级别 | 事项 | 影响 |
|---|---|---|
| 高（发布前） | Windows 实机未验证 | 不能声称 Windows PASS；需补写围栏、网络拒绝、cmd 删除与 dogfood |
| 中 | 真实 MCP destructive fixture 未接入 dogfood | 仅有 host typed-annotation 证据；live MCP UNVERIFIED |
| 中 | Linux 全量 tools lib 有环境/时序失败（第一次 `network_budget` 限速 1 个；重跑 6 个进程组/任务等待） | Docker PID 1 无 init；V2 相关用例全部通过；真实 Linux 宿主需复验 |
| 中 | 真实 LLM 未使用 | dogfood 使用确定性 Mock Provider；权限决策不依赖模型文本 |
| 低 | 子 Agent/后台 V2 live dogfood 未单列 | 现有所有权/后台回归全绿；V2 专项证据 PARTIAL |
| 低 | Linux 私有 LAN 未测 | 容器无 `ipconfig`/LAN 地址；macOS 已真实通过 |
| 低 | `cargo fmt --check` 仍有一处并发文件差异 `network_budget.rs` | 非本任务文件，未改动 |
| 低 | `config_hot_reload_e2e::a_running_daemon_switches_the_real_provider_on_reload` 在满载并发的全量运行中失败一次（5.73s），单独重跑通过（5.00s） | 既有 daemon E2E 时序，与本改动无交集；`--no-fail-fast` 全量结果见 13 |

## 5. 验证边界

- macOS：真实 `cargo test --workspace --no-fail-fast` 全绿（362 个测试二进制、9586 passed、0 failed）、真实 TUI、真实 sandbox、真实 LAN、真实本地 Git remote；受影响 crate 的 clippy `-D warnings` 全绿。
- Linux：真实 Docker Linux 内核、真实工具执行与 bwrap 路径；5 臂 dogfood PASS（私有网络 NOT_VERIFIED），`auto_permission_sandbox.rs` 是 macOS-only，在 Linux 上 0 测试，不能算 PASS。
- Windows：无宿主，全部标记 UNVERIFIED；未用交叉编译或静态分析冒充实机。

## 6. 证据

- `13-evidence-index.json`：机器可读索引。
- `evidence/permission-dogfood-macos.json`：macOS 5 臂 dogfood 原始报告。
- `evidence/permission-dogfood-linux.json`：Linux 5 臂 dogfood 原始报告。
- 具体命令与结果见 07–11。
