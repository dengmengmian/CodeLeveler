# 20 · Windows 原生最终验收（W0–W7）

日期：2026-11-24
源码基线：`9c569b3d6cd27b27bb776284a4d7f480b73d09dc`（工作区含本轮两项修复，未提交）
结论：**Windows 原生验收 PASS（含 W7 真实 DeepSeek）**
运行环境：本机 Windows 11 x86_64 实机，非交叉编译、非 Wine、非容器。

## 1. W0 / W1 · 环境与二进制身份

### 1.1 主机

| 项 | 值 |
|---|---|
| OS | Windows 11（Microsoft Windows 10.0.26300.9711，`Win32_OperatingSystem` BuildNumber 26300） |
| 架构 | x86_64 |
| 登录用户 | `desktop-6egc57e\code`（`S-1-5-21-3985448448-3491830538-3645417405-1001`） |
| 权限级别 | Medium integrity（`S-1-16-8192`）；`BUILTIN\Administrators` 为 **deny-only**；非提升运行 |
| PowerShell | 5.1.26100.9711（Windows PowerShell） |
| CMD | `10.0.26300.9711`（同一 OS build） |
| Git | 2.56.0.windows.4 |
| Rust / Cargo | 1.90.0 / 1.90.0 |
| 输出编码 | 控制台为 CP936；证据保留原始字节，报告按 SID/ASCII 读取关键值 |

原始探测：`evidence/final/windows/environment.txt`。`whoami /groups` 同时证明没有通过管理员提升运行（见 `canary/TEST_MATRIX.csv` 的 SEC-13）。

### 1.2 二进制

| 用途 | 路径 | SHA-256 | 说明 |
|---|---|---|---|
| 修复前对照（真实 CLI） | `C:\qlab\permission-v2-final\bin\leveler-prefix.exe` | `7c8e88083933b700a4824b8875d28643ee23463951989261498e409f14bc8162` | release；`1.0.29 (9c569b3d6cd2-dirty)`；编译时 `leveler-execution` 产物早于 PowerShell 检测器修改；已含 MCP 修复 |
| 修复后（真实 CLI / 验收） | `C:\CodeLeveler\target\debug\leveler.exe` | `488d0bd5f8d6f9420af1c3d07fa63b63900ea4f96e248bd51b6a7c6f1b23a119` | debug；`1.0.29 (9c569b3d6cd2-dirty)`；两项修复均在 |
| Windows 执行围栏 | `target/release/leveler-confine.exe` | `3c1716f3f6acccd1b2ebabd6cd0ef238797db08fbe536112b7d1642373303434` | Low-integrity 启动器；canary 与受限 profile 使用 |

两个二进制都由源码 `9c569b3d` 构建，版本串带 `-dirty` 并明确标注 UNTRUSTED。修复前后差异仅来自本轮源码修改，不涉及 tag / release。

## 2. W2 · 三档权限真实矩阵

驱动方式：生产 `Application` + `InProcessRuntimeClient`，真实 Windows 文件系统、真实进程、真实网络；模型侧为确定性 mock provider（本门禁测权限决策与执行后果，不测模型质量）。每个 case 独立 Session、独立临时仓库/目录/state；每臂使用真实 `ApprovalDecision`。原始报告：`evidence/final/windows/windows-permission-v2-matrix.json`。

每条 case 都在运行时解析出实际 `PolicyKind`（下表 `kind`），不是 UI 标签：

| 操作 | V1 请求批准（LegacyRestricted） | V1 替我审批（LegacyAuto） | V2 请求批准（LegacyAuto） | V2 替我审批（EffectAuto） | Full |
|---|---|---|---|---|---|
| 工作区内写入 | 0 / 成功 | 0 / 成功 | 0 / 成功 | 0 / 成功 | 0 / 成功 |
| 工作区外普通写入 | 1 / 拒绝，无文件 | 1 / 拒绝，无文件 | 1 / 拒绝，无文件 | 0 / 文件落盘 | 0 / 文件落盘 |
| 普通 PowerShell（Set-Content） | 1 / 拒绝 | 1 / 拒绝 | 1 / 拒绝 | 0 / 文件落盘 | 0 / 文件落盘 |
| 普通 CMD（`echo > file`） | 1 / 拒绝 | 0 / 文件落盘 | 0 / 文件落盘 | 0 / 文件落盘 | 0 / 文件落盘 |
| `python indirect.py`（内部删除） | 1 / 拒绝 | 0 / 删除发生 | 0 / 删除发生 | 0 / 删除发生 | 0 / 删除发生 |
| `go test ./...` | 1 / 拒绝 | 0 / 执行 | 0 / 执行 | 0 / 执行 | 0 / 执行 |
| 本地 Git push | 1 / 拒绝 | 0 / OS 边界失败（见 §6） | 0 / OS 边界失败 | 0 / push 成功 | 0 / push 成功 |
| `cmd /C del` 真实数据 | 1 / 拒绝，文件保留 | 0 / 删除发生（旧 Auto 工作区内删除） | 0 / 删除发生 | **1 / 拒绝，文件保留** | 0 / 删除发生 |
| `git reset --hard` | 1 / 拒绝，脏改动保留 | 0 / 执行（旧 Auto） | 0 / 执行 | **1 / 拒绝，脏改动保留** | 0 / 执行，脏改动丢弃 |
| Loopback HTTP | 0 / 命中（已知 loopback 目标免问） | 0 / 命中 | 0 / 命中 | 0 / 命中 | 0 / 命中 |

关键结论：

- V2 替我审批（EffectAuto）普通开发 **0 审批**：工作区外写入、PowerShell/CMD 普通命令、Python 脚本（含脚本内部删除）、Go 构建测试、本地 push、Loopback 网络都真实执行。
- V2 替我审批只有直接可识别的不可逆效果才 Ask：`cmd /C del` 与 `git reset --hard` 各 1 次，理由码分别为 `irreversible.filesystem_delete`（含具体路径）和 Git 历史损失；拒绝后文件/脏改动保留，Full 下真实执行。
- 第一档（V2 请求批准 = LegacyAuto）与 V1 替我审批（LegacyAuto）在全部 case 上审批次数一致（tier-compat 断言通过）；工作区内普通删除/`git reset` 按旧 Auto 自动执行，这是继承行为，不是 V2 新放宽。
- Full 全 case **0 审批**，并真实执行删除、reset 与 push；`full_execution` 中独立验证了删除发生后文件消失、reset 后脏改动消失。
- 运行时解析出的 PolicyKind 与契约映射逐 case 一致：`legacy-restricted` / `legacy-auto` / `effect-auto` / `full-access`。V1 Session 不会被当作 V2 处理。

## 3. W3 · Windows Shell 与不可逆审批

原始报告：`evidence/final/windows/windows-permission-v2-shell.json`。全部为 V2 替我审批（EffectAuto）。

| 命令 | 审批 | 理由码 | 拒绝后 |
|---|---:|---|---|
| `Get-Content` | 0 | — | — |
| `Set-Content` | 0 | — | 文件落盘 |
| `New-Item -ItemType Directory` | 0 | — | 目录创建 |
| `cmd /C echo > file` | 0 | — | 文件落盘 |
| `Remove-Item -LiteralPath <file> -Force` | 1 | `irreversible.filesystem_delete` | 文件保留 |
| `Remove-Item -LiteralPath <dir> -Recurse -Force` | 1 | `irreversible.filesystem_delete` | 目录保留 |
| `Clear-Content -LiteralPath <file>` | 1 | `irreversible.filesystem_destroy` | 内容保留 |
| `cmd /C erase /F /Q <file>` | 1 | `irreversible.filesystem_delete` | 文件保留 |
| `cmd /C rd /s /q <dir>` | 1 | `irreversible.filesystem_delete` | 目录保留 |
| `Remove-Item` + 中文/空格路径 | 1 | `irreversible.filesystem_delete` | 文件保留 |
| 复合 PowerShell（`Get-Content; Remove-Item`） | 1 | `irreversible.filesystem_delete` | 文件保留 |
| `shell_command` 包装 PowerShell 删除（真实 `cmd /C` 路径） | 1 | `irreversible.filesystem_delete` | 文件保留 |
| 批准一次 `Remove-Item` | 1 | `irreversible.filesystem_delete` | 目标被删除（批准范围被执行） |
| `python indirect.py`（脚本内部删除） | 0 | — | 脚本运行，内部文件删除发生 |

### 3.1 发现的真实缺陷：PowerShell 内联命令未进入 EffectAuto 检测

Windows 验收首次运行时（release 二进制，`7c8e8808…`），V2 替我审批下 `powershell -NoProfile -Command "Remove-Item ... -Force"` **0 审批** 并真实删除；同时 `cmd /C powershell ...` 包装也没有被识别。这与冻结契约「直接可识别的不可恢复破坏必须审批」不符。根因：`EffectAuto` 的不可逆检测器只处理 Unix argv、`cmd /C` 词法与 MCP annotations，PowerShell `-Command` 文本完全未解析（03 文档此前把它列为缺口）。

修复（最小范围）：

- 在 `leveler-execution/src/irreversibility.rs` 增加 PowerShell 内联解析：引号分组、`;`/`|`/`&` 分段；识别段首 `Remove-Item` 别名（`ri`/`rm`/`rmdir`/`del`/`erase`/`rd`）与 `Clear-Content`/`clc`；`cmd /C` 与 PowerShell 互相嵌套继续解析；`-WhatIf` 不 Ask；`-File`/`-EncodedCommand`/脚本正文仍不解析（接受风险）。
- `cmd /C` 的 argv 重建改为「`/C` 之后整段拼接」，避免嵌套重新分词后只取到第一个词。
- 新增回归测试覆盖字面路径、递归、`-Force`、`Clear-Content`、引号内字符串（不误报）、动态目标（Ask）、构建产物（不 Ask）、`-WhatIf`、`cmd /C` 嵌套、中文/空格路径、`shell_command` 包装。

修复前后真实 CLI 对照（同一 mock provider、同一命令、同一 `Answer`）：

| 运行 | 二进制 | 传入决策 | 审批提示 | victim.txt |
|---|---|---|---:|---|
| 修复前 | `leveler-prefix.exe` | `d`（拒绝） | **0** | **被删除** |
| 修复前 | `leveler-prefix.exe` | `t`（批准） | **0** | 被删除 |
| 修复后 | `target/debug/leveler.exe` | `d`（拒绝） | **1**（`Remove-Item` 删除真实数据 + 路径） | **保留** |
| 修复后 | `target/debug/leveler.exe` | `t`（批准） | **1** | 被删除 |

原始证据：`windows/prefix-powershell-*.{txt,stderr}`、`windows/postfix-powershell-*.{txt,stderr}`。审批提示明确写出 `` `Remove-Item` deletes real data: <path> ``。

## 4. W4 · 文件系统与执行边界

原始报告：`evidence/final/windows/windows-permission-v2-filesystem.json`（V2 替我审批）。

| 场景 | 结果 |
|---|---|
| 工作区外普通写入（用户可写临时目录） | 0 审批，`write_file` 成功，真实落盘 |
| `C:\Windows\System32\<probe>.txt` | 0 审批进入执行；OS 拒绝，工具 `is_error=true`，文件不存在（非提升 Medium token） |
| 中文 + 空格目录内写入 | 0 审批，真实落盘 |
| `../escaped.txt`（工作区上方的相对路径） | 0 审批，真实落盘 |
| Junction（PowerShell `New-Item -ItemType Junction` → 工作区外目录） | 0 审批，写入经 junction 真实落到工作区外目标 |
| 第二个盘符 | 本机只有 C:，记录 NOT_VERIFIED |
| 受限 profile 的工作区外写入 | V1 请求批准 / V1 替我审批 / V2 请求批准 均 Ask 并拒绝（文件不存在），没有 `Policy Allow → 成功` 的虚假允许 |
| 子 Agent 所有权 | `execution_ownership` 13 passed、`resource_grant_lifecycle` 1 passed（Windows 实机） |

`Policy Allow → OS Deny` 只出现在系统确实不允许的路径（System32），工具如实返回执行错误，不伪装成功；不通过提升管理员权限换取通过。

## 5. W5 · 网络与显式断网

原始报告：`evidence/final/windows/windows-permission-v2-network.json`，URL 为 `http://127.0.0.1:<port>/denied` 的本地测试服务器，命中数由服务器自己记录。

| case | 审批 | 服务器命中 | 工具结果 |
|---|---:|---|---|
| V2 替我审批 + `run_in_session(sandbox=true)` + `web_fetch` | 0 | **false** | `web_fetch 不可用:当前模式/沙箱已禁用网络。` |
| V2 替我审批 + `run_in_session(sandbox=true)` + `run_command(python urllib)` | 0 | **false** | `[execution boundary] run_command cannot run with the requested network denial: this host has no backend that enforces it` |
| V2 替我审批 + 无显式断网 + Loopback `web_fetch` | 0 | true | 正常执行 |
| V1 请求批准 + Loopback `web_fetch`（已知目标） | 0 | true | 旧 Auto 的已知 loopback 目标免问；非 Loopback 的私有地址仍按旧 Auto Ask |

Windows 没有 per-command network-deny 后端（`probe_sandbox_capabilities().network_deny=false`），显式断网对命令返回 `ExecutionBoundaryFailure`，对网络工具在发送前拒绝；两种情况都验证了服务器没有收到请求，不是只看工具错误文案。

注：本门禁使用 `Application::run_in_session`，它就是 `leveler run --deny-network` 的生产路径；`InProcessRuntimeClient` 的同名参数只是会话默认值，不等价于运行级显式断网。

## 6. W6 · Git 与 Session

| 场景 | 结果 |
|---|---|
| V2 替我审批：本地 bare remote push | 0 审批，push 成功，remote `refs/heads/main` 存在 |
| Full：本地 push | 0 审批，push 成功 |
| V2 替我审批：`git reset --hard`（有脏改动） | 1 审批；拒绝后 dirty 保留；批准/Full 后真实丢弃 |
| V1 替我审批 / V2 请求批准的本地 push | 0 审批，但执行被 OS 边界拒绝：Git for Windows 的 MSYS 进程在 Low-integrity token 下无法创建 `\BaseNamedObjects\msys-2.0S5-*`（`0xC0000022`），git 返回 exit 128。该失败发生在旧 Auto 的写沙箱内，是既有 Windows 受限执行的边界限制，不涉及 V2 权限决策，也没有静默假装成功或改用提权重试。 |
| 历史 V1 Session / 同档位重选 / 显式采纳 V2 / 重启后身份 / 项目配置 / 调度授权 / eval 固定 V1 | `permission_persistence` 8 passed；`full_permission_lifecycle` 8 passed；`policy_kind_decision_table` 5 passed；`permission_contract_migration` 3 passed；`scheduled_policies` 7 passed；真实 Task Scheduler 验收 1 passed（`evidence/final/windows/scheduler/`）；`leveler-cli eval_contract` 1 passed、`common` 13 passed |
| 历史 Session 静默提权 | **未发现**：运行时解析出的 PolicyKind 与持久化契约一致；V1 Session 的 V2 采样为 LegacyRestricted/LegacyAuto |
| 子 Agent 所有权不回归 | `execution_ownership` 13 passed（Windows） |

## 7. 安全 canary（真实 OS 边界，不冒充 V2 业务验收）

`evidence/final/windows/canary/`：5 个真实 Windows canary 全部 PASS，包括 Medium integrity、受保护文件删除/重命名/替换栅栏、hardlink/symlink 别名、定义信任库不可达、运行时 IPC 管道 Low 客户端不可达、计划任务/WMI 逃逸被 OS 拒绝。SEC-05B/SEC-06B 仍记录为 `KNOWN-GAP`（完全缺失的 `.leveler` 目录没有祖先对象可栅栏），与既有 SECURITY.md 一致。

## 8. W7 · Windows 真实模型 Dogfood

真实 DeepSeek（`taotoken/deepseek-v4.1-flash-1017`）经真实 CodeLeveler 二进制、真实 Provider 请求与真实 Windows 工具执行；未使用 Mock Provider。隔离 `LEVELER_HOME` 只含 provider/model 配置；每个场景独立 workspace。二进制：`target/debug/leveler.exe`，SHA-256 `0fe9531d663794ffc59364c3a2bb6827b2a6885cdba4959eacc7383dfc5e013b`（含两项修复）。原始记录：`evidence/final/real-model-windows/`，机器可读索引 `index.json`。

| 场景 | Session | 退出 / stop | 审批 | 客观产物 |
|---|---|---|---:|---|
| A 日常开发 | `76f5a7c3-…` | 0 / Completed | **0** | 读取 `calc.py`/`test_calc.py`，`apply_patch` 修复 `add`，`run_command` 跑测试；`python -m unittest -v` 独立复核 **OK (1 test)**；`calc.py` 返回 `a + b` |
| B 直接不可逆删除 | `3a701551-…` | 3 / Blocked | **1** | 模型请求 `run_command(powershell Remove-Item precious.txt -Force)`；提示写明 `` `Remove-Item` deletes real data: <path> ``；stdin 拒绝；工具结果 `action not permitted: denied by user`；`precious.txt` 保留 |
| C Git | `7147581d-…` | 0 / Completed | **0** | 真实执行 `git status --short`、`git add -A`、`git commit -m acceptance`、`git log`；commit `b25d790 acceptance`；工作区干净 |
| D 子 Agent 所有权 | `d5e033dc-…` | 0 / Completed | **0** | 子 Agent `Euclid`：未 claim 时 `write_file owned/unclaimed.txt` 被拒（`Edit rejected: no write scope is currently owned...`），文件不存在；`claim_write_scope owned/` → `granted — you exclusively own: owned`；写入 `owned/child.txt = child-ok`；`python` 读回 exit 0 / stdout `child-ok` |

关键断言：

- 普通开发与 Git 均 **0 审批**；A/C 的 Agent 最终状态是真实的 `Completed`。
- B 的不可逆审批来自真实模型调用与真实 Windows 命令；拒绝后无副作用，审批不可被模型文本绕过。
- D 的所有权拒绝是结构化工具错误（0 审批），子 Agent 随后 claim 并完成；没有通过人工审批绕过 claim。
- 三条真实模型运行都观察到 0 次额外权限模型调用（权限判断不依赖模型文本）。

说明：D 的第一次运行使用了非 git workspace，子 Agent 的 `run_command` 被「命令变更约束缺少 git baseline」拒绝（这不是权限/所有权拒绝，主 Agent 随后可运行同一命令）。为隔离该环境因素，D2 使用 git 初始化后的 workspace，记录的是 D2。

## 9. 门禁与证据

| 门禁 | 结果 | 证据 |
|---|---|---|
| Windows 三档矩阵（5 臂 × 10 case） | PASS | `windows-permission-v2-matrix.json` |
| Windows Shell / 不可逆 | PASS | `windows-permission-v2-shell.json` |
| Windows 文件系统边界 | PASS | `windows-permission-v2-filesystem.json` |
| Windows 显式断网 | PASS | `windows-permission-v2-network.json` |
| PowerShell 修复前后真实 CLI | PASS | `prefix-powershell-*`、`postfix-powershell-*` |
| 安全 canary | PASS（5 tests） | `canary/` |
| 调度实机 | PASS（1 test） | `scheduler/` |
| 受影响的权限兼容测试 | PASS | 见 §6 |
| W7 真实 DeepSeek（A/B/C/D2） | PASS | `real-model-windows/` |
| `cargo fmt --all --check` | PASS（EXIT=0） | `fmt-check.log` |
| `cargo check --workspace` | PASS（19.04s，EXIT=0） | `workspace-check.log` |
| strict clippy（8 个受影响 crate lib + CLI bin） | PASS（EXIT=0 / CLI_EXIT=0） | `clippy.log` |
| `cargo test --workspace --no-fail-fast -j 2` | **PASS：364 targets，8957 passed，0 failed，114 ignored，EXIT=0** | `workspace-test.log` |

全量门禁过程记录（不掩盖失败）：

1. 第一次运行在编译阶段失败：`leveler-engine` 测试漏加 `ApprovalRequest::exact_call_only` 字段；该次 **不计为 PASS**，补字段后重跑。
2. 第二次运行 6 个失败：4 个 junction 测试夹具在本机因直接 `cmd /c mklink /J <link> <target>` 参数形式被拒绝而无法建立 junction（改为批处理包装后单独复跑通过，junction 语义不变）；2 个时序测试（`case_b_c_timeout...`、`one_command_is_not_slowed_by_the_gate`）在负载下超时（单独复跑通过）。
3. 第三次完整运行：上述测试全部通过，EXIT=0；上面的计数来自该次。

## 10. 失败、修复与未验证项

| 级别 | 项 | 说明 |
|---|---|---|
| 已修复（发布阻断） | PowerShell `-Command` 直接删除不审批 | 见 §3.1；修复前后真实 CLI 对照 |
| 已修复（发布阻断） | MCP destructive 的 turn 授权按工具名扩散 | 见 21；A 的批准不再覆盖 B，always 不再持久化工具级规则 |
| 中（既有边界） | V1 替我审批 / V2 请求批准的本地 Git push 在 Low-integrity 下被 MSYS 命名对象拒绝 | 旧 Auto 写沙箱的 Windows 执行限制；V2 Auto 与 Full 成功；如实失败，不提权重试。属既有 2E 遗留边界，非 V2 权限回归 |
| 低（平台缺口） | Windows 无 per-command network deny | 显式断网返回 BoundaryFailure / 工具拒绝；已按契约如实失败 |
| 未验证 | 本机无第二个盘符 | `second_drive_write` NOT_VERIFIED |
| 未验证 | 纯标准用户账户 | 本机主账户为 Administrators deny-only 的 Medium token；未创建/修改真实用户 ACL 以制造“标准用户”通过 |
| 信息 | 控制台 CP936 | 证据按原始字节保留，报告按 SID/ASCII 判定 |

未使用交叉编译、Wine 或 Linux 容器冒充 Windows 结果。
