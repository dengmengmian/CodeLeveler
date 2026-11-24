# 11 · Phase 2E 报告：跨平台验收

日期：2026-11-24

## 1. 平台矩阵

| 平台 | 环境 | 结论 | 证据 |
|---|---|---|---|
| macOS | 本机 `darwin` / aarch64，真实 TUI 与真实 sandbox | **PASS** | `cargo test --workspace`、dogfood 5 臂、TUI/CLI 回归 |
| Linux | Docker `rust:1.90-bookworm`，aarch64，真实 Linux 内核与进程 | **PARTIAL PASS（V2 核心）** | execution 449、policy 5、V2 tools 5、storage 215+3、schedule 7；tools 全量 lib 有 6 个进程组/任务等待类环境失败 |
| Windows | 无 Windows 实机 | **UNVERIFIED** | 未做交叉编译或静态分析冒充 |

> 用户要求：Windows/Linux 必须真实环境测试，不能把交叉编译或静态分析声称为实机 PASS。本报告严格遵守。

## 2. macOS

| 项 | 结果 |
|---|---|
| workspace 测试 | `cargo test --workspace --no-fail-fast` 全绿：362 个测试二进制、9586 passed、0 failed |
| Phase 2D dogfood | PASS：5 臂 × 8 case，0 断言失败 |
| Full 验收矩阵 | `full_permission_acceptance` 6/6；`full_permission_lifecycle` 8/8（含新增 V1/V2 分档用例） |
| TUI | `leveler-tui --lib` 1892 passed；`slash_command_surface` 9/9 |
| 真实删除/历史拒绝 | 目标保留；`dirty_survived=true` |
| 私有 LAN | 真实本机 LAN `10.9.9.108`，EffectAuto 请求到达服务端 |
| 本地 Git push | EffectAuto/Full 均真实到达 bare remote |

## 3. Linux（Docker，真实内核）

### 3.1 通过

| 套件 | 结果 |
|---|---|
| `leveler-execution --lib` | 449 passed, 0 failed |
| `leveler-execution --test policy_kind_decision_table` | 5 passed |
| `leveler-tools --lib` 中的 V2 用例（`effect_auto_*`、`any_scope_reaches_*`） | 5 passed |
| `leveler-storage --lib` | 215 passed |
| `leveler-storage --test permission_contract_migration` | 3 passed |
| `leveler-schedule --test scheduled_policies` | 7 passed（含契约 fail-closed） |
| `leveler-agent --lib effect_auto` | 4 passed |
| `leveler-app` dogfood（修复后重跑） | **PASS**：5 臂 × 8 case，`private_network` 为 NOT_VERIFIED，其余与 macOS 矩阵一致 |

### 3.2 环境失败（非权限回归）

`leveler-tools --lib` 全量运行两次，失败项都是环境/时序类，不是权限回归：

| 运行 | 结果 | 失败项 |
|---|---|---|
| 第一次 | 563 passed, 1 failed | `network_budget::the_rate_gate_spaces_windows_and_a_cancelled_reservation_is_reusable`（限速窗口时序） |
| 第二次 | 558 passed, 6 failed | 4 个 `foreground_handoff_tests`（`process group termination could not be confirmed`）+ 2 个 `task_control` 等待测试 |

这些测试依赖进程组终止确认、PID 1 回收、后台任务结算和精确的限速窗口；Docker 容器由 `bash` 作为 PID 1、无 init reaper，且容器内进程组/调度语义与真实 Linux 宿主不同。失败信息是环境能力或时序差异，不是 V2 权限判定错误；V2 相关用例两次都通过。**真实 Linux 宿主上这些测试仍未验证**。

`auto_permission_sandbox.rs` 是 `#![cfg(target_os = "macos")]`，在 Linux 上运行 0 个测试；不能把“0 failed”当作 Linux sandbox PASS。

### 3.3 Linux dogfood

见 `evidence/permission-dogfood-linux.json`。Linux 容器没有 `ipconfig`，`private_network` case 标记 `NOT_VERIFIED`；其余 case 为真实 Linux 工具执行。

结果（修复 Loopback 边界后）：

| case | v1_req | v1_auto | v2_req | v2_auto | full |
|---|---:|---:|---:|---:|---:|
| write_inside | 0 | 0 | 0 | 0 | 0 |
| write_outside | 1 | 1 | 1 | 0 | 0 |
| python_script | 0 | 0 | 0 | 0 | 0 |
| shell_build | 0 | 0 | 0 | 0 | 0 |
| git_push_local | 0 | 0 | 0 | 0 | 0 |
| delete_real_file | 1 | 1 | 1 | 1 | 0 |
| git_reset_hard | 1 | 0 | 0 | 1 | 0 |
| private_network | NOT_VERIFIED | NOT_VERIFIED | NOT_VERIFIED | NOT_VERIFIED | NOT_VERIFIED |

与 macOS 的唯一差异是私有网络 case 未测量。Linux 首次运行时发现 V1 严格档的 `Loopback` 在 bubblewrap 后端被误判为 `UnsupportedNetworkScope`，导致普通命令全部边界失败；修复后 V1 严格档的 Python/Shell/Git 均成功，且命令网络被收窄为拒绝（`[execution policy] Network access was denied`），不可逆删除/重置仍 ASK 且拒绝后无副作用。

## 4. Windows

**UNVERIFIED / BLOCKED**：本机没有 Windows 宿主，没有运行 `leveler-confine.exe`、Windows 写围栏、Windows 命令或 Windows 实机 dogfood。未用交叉编译、静态分析或 CI 配置声称实机 PASS。

发布 Windows 前必须补：

1. `auto_permission_sandbox` 的 Windows 对等测试（写围栏、网络拒绝能力协商）。
2. 4 臂 + Full dogfood 在真实 Windows 上重跑。
3. Windows 的 `cmd /C` 包装器检测、`del/erase/rd` 删除、`format/diskutil` 等平台设备操作实机验证。

## 5. UI / Full 非回归

| 项 | 结果 |
|---|---|
| 无第四档 | 是：`PermissionProfile` 仍是三档 |
| 无新权限页面 | 是：只新增 `/permission-contract v2`（Searchable，不进入空 `/` 列表） |
| V1 标记 | `ask·v1` / `auto·v1`；Full 仍显示 `full` |
| TUI 测试 | 1892 passed，0 failed |
| Full 行为 | 0 审批；删除/重置真实执行；显式 `--deny-network` 仍生效 |
