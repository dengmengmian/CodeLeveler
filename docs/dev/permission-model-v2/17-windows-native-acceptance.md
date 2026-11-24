# 17 · Windows 原生验收（Phase F5）

日期：2026-11-24
结论：**BLOCKED_ENVIRONMENT / UNVERIFIED**
没有 Windows 实机、VM 或可远程使用的 Windows 主机。未用交叉编译、Wine、静态分析或 CI 配置冒充 Windows 实机 PASS。

> 后续：本文记录的是首轮无 Windows 主机时的结论。Windows 11 实机上的原生验收已在同一验收系列完成，结论见 [20 · Windows 原生最终验收](20-windows-native-final-closure.md)；本文保留为历史记录，不再代表最终状态。

## 1. 环境探测

| 检查 | 结果 |
|---|---|
| 本机平台 | macOS `darwin` / aarch64 |
| UTM | `utmctl list` 为空；无已安装 guest |
| Parallels / VMware / VirtualBox | 未安装（仅 UTM.app 存在） |
| Azure / AWS / gcloud / doctl CLI | 不存在 |
| SSH `known_hosts` / `config` | 无 Windows/RDP 主机 |
| Docker 镜像 | 全部 Linux；无 Windows 容器能力（宿主机为 macOS） |
| 现有 Windows 进程 | 无 `leveler.exe` / `leveler-confine.exe` 可执行环境 |

## 2. 唯一可做的编译级检查

| 命令 | 结果 | 说明 |
|---|---|---|
| `cargo check --workspace --target x86_64-pc-windows-gnu` | **PASS**（exit 0，21.51s） | 仅证明当前工作树能在 GNU Windows target 下完成类型检查；**不是** Windows 运行、OS 权限、sandbox 或行为验收 |
| 交叉链接器 | `x86_64-w64-mingw32-gcc` 存在 | 未构建/运行 Windows 二进制 |
| MSVC target | 已安装 `x86_64-pc-windows-msvc` | 未用 MSVC 链接（无 `cl.exe`/SDK）；不作为证据 |

## 3. 必须但无法执行的 Windows 矩阵

| 类别 | 要求 | 状态 |
|---|---|---|
| 三档权限 | V2 RequestApproval / V2 Assisted / Full | UNVERIFIED |
| V1 恢复 | 历史 V1 Session | UNVERIFIED（macOS 已覆盖逻辑；Windows 行层未测） |
| 文件系统 | 工作区内/外写、绝对/相对路径、空格与中文路径、`\` 分隔符、Junction/Symlink、权限不足、不可逆删除 | UNVERIFIED |
| Shell | PowerShell 正常命令、`cmd /C`、批处理、构建测试、直接删除、复合命令、脚本不审查间接副作用 | UNVERIFIED（`cmd /C` 分类有 macOS 可跑的单元测试，但不是 Windows 执行） |
| 网络 | 普通 HTTP、Loopback、私有网络、`--deny-network`、无法隔离时的正确失败 | UNVERIFIED |
| Git | 本地 bare remote、push、reset、clean、数据丢失审批、拒绝后无副作用 | UNVERIFIED |
| Session | V1 恢复、V2 新建、同档位重选、显式采纳、重启后权限身份、调度授权版本 | UNVERIFIED |

## 4. 已有代码级线索（不能替代实机）

| 项 | 证据 | 局限 |
|---|---|---|
| `cmd /C` 不可逆分类 | `leveler-execution/tests/auto_permission_decision_table.rs`（`cmd /C` 行）与 `classify_adversarial.rs` 的 Windows 形状 | 只验证分类器输入输出，不验证 Windows 进程/ACL |
| Windows 写围栏 | `leveler-execution` 的 Windows 后端（AppContainer / low integrity 历史实现） | 无 Windows 主机，未验证实际拒绝/允许 |
| 网络拒绝能力 | `windows_sandbox::probe_sandbox_capabilities().network_deny` | 无实机，未验证能力探测与真实结果 |
| Windows 安全 canary | `crates/leveler-execution/tests/windows_security_canary.rs` | `cfg(windows)`；macOS 上 0 tests，不能算 PASS |
| 真实 Windows GUI/ConPTY | 无 | 未验证 |

## 5. 阻断原因与影响

| 项 | 说明 |
|---|---|
| 阻断原因 | 环境中不存在可用的 Windows 主机/VM；不是产品代码失败 |
| 是否需要产品修复 | 当前证据不足以判定；未发现具体 Windows 产品缺陷 |
| 补测条件 | 一台非提权 Windows 10/11 x86_64 主机（或可交互 VM），安装当前构建产物，执行三档权限、文件、Shell、网络、Git、Session 矩阵，并保留 `Policy Decision → Execution Policy → OS Execution → Actual Result` 证据 |
| 是否影响 Release | **影响**：按本轮标准，Windows 原生 V2 验收未完成；不能宣称全平台 PASS。若 Release 明确只承诺 macOS/Linux，则需在发布说明中如实标注 Windows UNVERIFIED |
