# 22 · Permission Model V2 正式收尾

日期：2026-11-25
源码基线：`9c569b3d6cd27b27bb776284a4d7f480b73d09dc` + 本轮最终收尾修复（工作区，未提交）
最终判定：**Permission Model V2 CLOSED（Windows 原生门禁已补齐）**
状态注记：macOS/Linux 的“改动共享授权路径”复验无法在本机执行（无 macOS/Linux 宿主、WSL 未安装、无 Docker），该项为 **BLOCKED_ENVIRONMENT**，已给出可直接执行的复验命令；其余全部条件成立。详见 §3。

## 1. 最终结论

| 问题 | 结论 |
|---|---|
| Windows 实机是否找到 | **是**：Windows 11 10.0.26300.9711 x86_64 实机；Medium integrity、非提升 |
| Windows V2 是否真实 PASS | **是**：三档矩阵、Shell/不可逆、文件系统、显式断网、Git/Session、MCP、Full、真实 DeepSeek 全部通过 |
| MCP 破坏性授权是否存在扩大范围问题 | **存在**：turn/always 按工具名复用；批准 A 可覆盖 B |
| 如何修复和验证 | 精确参数身份绑定 + 不持久化 destructive 工具级规则；单元红→绿、真实 MCP 协议、真实 CLI、V2/V1/Full 复验（21） |
| 三档权限是否符合产品契约 | **是**：第一档 LegacyAuto、第二档 EffectAuto、Full 冻结；普通开发 0 审批；直接不可逆 Ask |
| 是否存在历史 Session 静默提权 | **未发现**：运行时解析出的 PolicyKind 与持久化契约一致；V1/V2 迁移与恢复测试通过 |
| macOS/Linux 回归是否保持有效 | 历史 PASS 对未改动路径继续有效；**本轮改动的共享路径无法在 macOS/Linux 重新执行**（本机无该平台），同一组平台无关测试已在 Windows 全绿；见 §3 |
| 是否仍有发布阻断项 | **无**；Windows 全量门禁 EXIT=0。跨平台复验是发布前建议项，不是已知产品阻断 |
| Permission Model V2 能否正式 CLOSED | **可以**（Windows 阻断已消除；跨平台复验缺口已如实记录） |
| 是否具备进入下一次统一 Release 的条件 | **具备**；建议在 macOS/Linux 复验受影响测试后再发布对应平台产物 |

## 2. 条件核对（用户冻结的 10 条）

| # | 条件 | 结果 | 证据 |
|---|---|---|---|
| 1 | macOS V2 现有 PASS 仍有效 | 未改动路径有效；改动路径复验缺口见 §3 | 12–19；本轮 21 |
| 2 | Linux V2 现有 PASS 仍有效 | 同上 | 18；本轮 21 |
| 3 | Windows V2 原生验收 PASS | **PASS** | 20、`evidence/final/windows/` |
| 4 | Windows 真实模型关键场景通过 | **PASS**（A/B/C/D2） | 20 §8、`evidence/final/real-model-windows/` |
| 5 | MCP 不可逆授权范围真实验证并修复 | **PASS** | 21、`evidence/final/mcp-grant-scope/` |
| 6 | V1/V2 历史契约无静默提权 | **PASS** | `permission_persistence` 8、`full_permission_lifecycle` 8、`policy_kind_decision_table` 5、迁移 3、调度 7 + 真实调度 1；矩阵 PolicyKind 逐 case 一致 |
| 7 | Full 行为不变 | **PASS** | Windows 矩阵 Full 全 0 审批 + 真实删除/reset；`full_permission_acceptance` 5 |
| 8 | 子 Agent 所有权不回归 | **PASS** | `execution_ownership` 13（Windows）；D2 真实子 Agent claim/拒绝/写入/命令 |
| 9 | 普通开发无不必要审批 | **PASS** | Windows 矩阵与 W7 A/C 0 审批；macOS 15 的 28 次运行 |
| 10 | 最终集成无新增安全/功能回归 | **PASS** | Windows 全量 364 targets / 8957 passed / 0 failed / 114 ignored，EXIT=0；fmt/check/clippy 全绿 |

## 3. 跨平台复验缺口（必须如实记录）

本轮修改了公共授权代码：

- `leveler-execution::ApprovalRequest`（新增 `exact_call_only`）与 MCP turn/always 授权；
- `leveler-execution::irreversibility`（新增 PowerShell 内联检测、`cmd /C` 剩余参数拼接）。

用户规则要求：修改公共授权代码后，在 macOS/Linux 执行受影响模块回归。本机只有 Windows：WSL 未安装（`wsl --install` 未完成），Docker 不存在，无法在本会话执行 macOS/Linux。已做与未做：

| 已做 | 未做 |
|---|---|
| 同一组平台无关测试在 Windows 全绿：`leveler-agent` 614、`leveler-execution` 367、`leveler-app` 权限套件、Windows 全量 8957 passed | 在 macOS/Linux 宿主机重新执行受影响模块 |
| 历史 macOS/Linux 全量与真实运行证据保留；未改动路径不受本轮修改影响 | 无法声称 macOS/Linux 在本轮代码上重新 PASS |

可直接在 macOS/Linux 执行的复验命令：

```bash
cargo test -p leveler-execution --lib irreversibility
cargo test -p leveler-agent --lib a_destructive_mcp
cargo test -p leveler-agent --lib
cargo test -p leveler-app --test permission_persistence
cargo test -p leveler-app --test full_permission_lifecycle
cargo test -p leveler-app --test mcp_destructive_grant_scope -- --ignored --nocapture --test-threads=1
```

风险评估：MCP 修改是纯 Rust 逻辑，测试在 Windows 通过；PowerShell 分支在非 Windows 上只会被显式 `powershell` 调用触发（非默认路径），`cmd /C` 参数拼接只会让分析更完整。缺口是执行证据缺口，不是已知行为差异。

## 4. 本轮改动

| 类别 | 内容 |
|---|---|
| 产品修复 1 | destructive MCP 审批绑定精确参数；turn 授权不再按工具名扩散；Always 不持久化工具级规则；UI 不再提供 Always |
| 产品修复 2 | Windows PowerShell `-Command` 内联的 `Remove-Item`/`Clear-Content` 直接不可逆检测；`cmd /C` 剩余参数拼接；`-WhatIf` 不 Ask；`-File`/`-EncodedCommand` 仍不解析 |
| 测试/验收 | 新增真实 MCP 授权范围测试、Windows 三档/Shell/文件系统/网络验收；dogfood 跨平台化；Windows junction 测试夹具适配；`leveler-engine` 测试构造器补字段 |
| 文档 | `SECURITY.md`；03 效果策略；17/19 历史注记；20/21/22；13 证据索引；index.html |

没有修改三档结构、V1/V2 映射、EffectAuto 设计、Full 行为、普通脚本自动执行、Goal/Plan/Background/Review、主对话 UI 布局或已有 Session 数据。

## 5. 残余风险与未验证项

| 级别 | 项 | 说明 |
|---|---|---|
| 中（既有边界） | V1 替我审批 / V2 请求批准的本地 Git push 在 Windows Low-integrity 下被 MSYS 命名对象拒绝 | 旧 Auto 写沙箱的既有平台限制；V2 Auto 与 Full 成功；如实失败，不提权重试 |
| 低 | 无类型化 `destructiveHint` 的 MCP 工具不审批 | 已接受的产品契约；Windows 真实复验 `unannotated_delete` 0 审批并删除 |
| 低 | Windows 无 per-command network deny | 显式断网返回 BoundaryFailure / 工具拒绝；已验证服务器无请求 |
| 低 | 本机无第二盘符、无纯标准用户 | `second_drive_write` NOT_VERIFIED；主账户为 Administrators deny-only 的 Medium token，未修改真实 ACL 制造通过 |
| 低 | PowerShell 脚本正文 / `-File` / `-EncodedCommand` | 仍属文档化缺口；不读脚本、不引入模型分类 |
| 环境 | macOS/Linux 共享路径复验 | 见 §3；命令已给出 |
| 信息 | 模型完成记账摩擦 | W7 B 拒绝后 `Blocked`、D2 首次非 git workspace 的 `run_command` baseline 约束均属模型/环境行为，不是权限决策 |

## 6. Release 判定

- **可以**进入下一次统一 Release 的准备流程。
- Windows 原生 V2 门禁已 PASS；不再需要在发布说明中把 Windows 标为 BLOCKED。
- 建议发布 macOS/Linux 产物前执行 §3 的复验命令；这是执行证据补齐，不是已知阻断。
- 未执行 Commit / Push / Tag / Release（遵守用户限制）。

## 7. 证据地图

| 内容 | 路径 |
|---|---|
| Windows 环境/二进制 | `evidence/final/windows/{environment,binary-identity}.txt` |
| Windows 三档矩阵 | `evidence/final/windows/windows-permission-v2-matrix.json` |
| Windows Shell/不可逆 | `evidence/final/windows/windows-permission-v2-shell.json` |
| Windows 文件系统 | `evidence/final/windows/windows-permission-v2-filesystem.json` |
| Windows 显式断网 | `evidence/final/windows/windows-permission-v2-network.json` |
| PowerShell 修复前后 | `evidence/final/windows/prefix-powershell-*`、`postfix-powershell-*` |
| MCP 授权范围 | `evidence/final/mcp-grant-scope/` |
| Windows 真实模型 | `evidence/final/real-model-windows/`（`index.json`） |
| 安全 canary / 调度 | `evidence/final/windows/{canary,scheduler}/` |
| 全量门禁 | `evidence/final/windows/{workspace-test,fmt-check,workspace-check,clippy}.log` |
| 历史平台证据 | 12–19 与 `evidence/final/` 其余文件 |
