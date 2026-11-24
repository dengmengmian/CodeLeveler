# 19 · Permission Model V2 最终收尾报告

日期：2026-11-24
源码基线：`5800dd8aad887137e86806e10d02ec84f5860c5d`（`main`，工作区未提交）
总体结论：**PARTIAL / BLOCKED（Windows 原生门禁未完成）**
V2 行为结论：**macOS PASS · Linux PASS · Windows BLOCKED**
正式 CLOSED：**NO**（按本轮定义，Windows 实机是阻断门禁）
Release：**可以进入下一次 Release 的准备流程，但不能宣称全平台 PASS；Windows 必须在发布范围中明确排除并如实标注，或先完成实机验收。**

## 1. 本轮做了什么

Permission Model V2 的 Phase 2A–2E 已在前一轮完成。本轮只补齐证据缺口：

| 门禁 | 内容 | 结果 |
|---|---|---|
| F1 | 工作区 / 集成状态审计、并发修改隔离 | **PASS** |
| F2 | 真实 DeepSeek Agent Dogfood（A–F） | **PASS** |
| F3 | 真实 MCP 端到端（本地 fixture + 第三方） | **PASS** |
| F4 | 真实子 Agent / 后台任务 | **PASS** |
| F5 | Windows 实机 | **BLOCKED_ENVIRONMENT** |
| F6 | Linux 补充验收 | **PASS** |
| 集成 | fmt / check / workspace tests / strict clippy | **PASS** |

本轮**没有修改产品代码**；新增内容仅为 `docs/dev/permission-model-v2/` 下的报告与证据。没有 commit / push / tag / release，没有新建或删除分支/worktree。

## 2. 最终集成门禁

| 命令 | 结果 | 证据 |
|---|---|---|
| `cargo fmt --all --check` | **exit 0** | `evidence/final/fmt-check.log` |
| `cargo check --workspace --all-targets` | **exit 0**（17.99s） | `evidence/final/workspace-check.log` |
| `env -u NODE_OPTIONS cargo test --workspace --no-fail-fast` | **362 binaries, 9586 passed, 0 failed, 111 ignored**（exit 0） | `evidence/final/workspace-test.log` |
| `cargo clippy -p leveler-execution -p leveler-tools -p leveler-agent -p leveler-storage -p leveler-schedule -p leveler-client-protocol -p leveler-app -p leveler-tui --lib -- -D warnings` | **exit 0** | `evidence/final/clippy.log` |
| `cargo clippy -p leveler-cli --bin leveler -- -D warnings` | **exit 0** | 同上 |
| `cargo check --workspace --target x86_64-pc-windows-gnu` | **exit 0**（21.51s，编译级，不是实机） | `evidence/final/windows-gnu-check.log` |

说明：`env -u NODE_OPTIONS` 是本仓库已记录的环境陷阱（`NODE_OPTIONS` 会让 confined 测试失败），不是放宽测试。工作区测试包含其他并发专项的代码；全绿说明当前合并树在 macOS 上无失败，但不把并发专项算作 V2 的交付。

V2 专项测试（macOS，workspace 运行内）：

| 套件 | 结果 |
|---|---|
| `full_permission_acceptance` | 6 passed |
| `full_permission_lifecycle` | 8 passed |
| `permission_persistence` | 8 passed |
| `policy_kind_decision_table` | 5 passed |
| `leveler-execution --lib` | 469 passed（macOS） |

## 3. 验收指标

| 指标 | 标准 | 实测（真实模型 / 真实执行） |
|---|---|---|
| V2 Auto 普通开发 | 无不必要审批 | **0**（A：10 轮；B：4 条命令） |
| `python script.py` | 不审查间接副作用、不审批 | **0**；脚本内部删除真实发生 |
| 工作区外普通写入 | 自动执行成功 | **0**；文件真实落盘（macOS + Linux） |
| 普通 MCP | 自动执行 | **0**（read/write） |
| 明确破坏性 MCP | 正确审批 | **1**；拒绝后保留，批准后只删批准目标 |
| 真实文件删除 | 正确审批 | **1**；拒绝后文件存在 |
| Git 数据损失 | 正确审批 | **1**；拒绝后 HEAD/dirty 不变 |
| 审批拒绝 | 无未经批准的副作用 | 全部拒绝场景无副作用；denied 准确返回模型 |
| 子 Agent 所有权 | 正确执行，不可通过审批绕过 | 越界写 Deny 返回模型，0 审批；claim 后成功并跑测试 |
| 历史 V1 Session | 不静默升级 | 项目默认 V1 实测触发旧 Auto 审批；V2 显式 0 审批；持久化/迁移测试 8+3 通过 |
| Full | 与原行为一致 | **0 审批**，真实删除执行 |
| 主对话 UI | 无布局变化 | 本轮未改 TUI；TUI 测试全绿 |
| 额外权限模型调用 | 0 | 所有 macOS/Linux 运行 `rounds == usage == stream_attempt_started` |

## 4. 平台状态

| 平台 | 状态 | 说明 |
|---|---|---|
| macOS | **PASS** | 真实 CLI / Provider / 工具 / 文件 / 进程 / 网络 / Git；真实 DeepSeek；真实 MCP；真实子 Agent |
| Linux | **PASS** | Docker aarch64、真实 Linux 内核、bwrap；9 个真实模型场景；`--init` 下 execution 449、tools 566、storage 215+3、schedule 7、V2 dogfood 5 臂 |
| Windows | **BLOCKED_ENVIRONMENT** | 无实机/VM/远程主机；只做了 GNU 编译检查；不冒充 PASS |

## 5. 未关闭问题与风险

| 级别 | 项 | 影响 / 说明 |
|---|---|---|
| 高（发布门禁） | Windows 原生 V2 验收 | 无主机，无法验证写围栏、`cmd /C` 删除、PowerShell、网络拒绝、Git、Session 恢复；见 17 |
| 中 | 真实模型完成契约摩擦 | A 的 `update_goal` 被 citation/requirement 记账拒绝两次后成功；F1 在业务动作（修改/提交/推送）完成后卡在 12 步机械上限（`TurnLimitReached`）。这不是权限决策，但影响“Completed”体验，见 15 §7 |
| 中 | MCP 无 typed annotation 的破坏工具 | V2 Auto 按契约不审批，已用 `unannotated_delete` 真实复现；残余风险已登记，见 16 §4 |
| 低 | 第三方 MCP 覆盖 | server-everything 只验证了 `echo`；其 16 个工具、不同 annotations、不同 server 行为未逐一验证 |
| 低 | Linux 宿主形态 | 容器内核是 LinuxKit VM；私有网络是 `172.17.0.8`，不是物理宿主 LAN；未使用真实桌面 Linux |
| 低 | V1 Session 的 live resume | 实测覆盖项目默认 V1 vs V2；会话级恢复/切档由 `permission_persistence`、`full_permission_lifecycle`、迁移测试覆盖，未再做一次真实模型 resume |
| 低 | MCP capability 启用提示 | 首次 destructive 尝试中，模型把“下一请求生效”误判为工具不可用，未发起调用、无副作用；显式提示后正常。属模型行为/UX，不是权限缺陷 |
| 信息 | 并发修改 | 工作区还包含 web latency/parallel、web search/doctor、memory transcript 等其他专项修改；本轮未触碰、未清理 |

## 6. 是否具备进入 Release 流程的条件

| 问题 | 回答 |
|---|---|
| V2 行为是否达到可发布质量（macOS/Linux） | **是**：真实模型、真实 MCP、子 Agent、Full/V1 兼容、全量 macOS 测试、Linux 受控测试均通过 |
| 是否可以宣称 Permission Model V2 正式 CLOSED | **否**：按本轮验收定义，Windows 实机未完成，不能给出全平台 PASS |
| 是否可以进入下一次 Release 准备 | **可以**，前提是发布范围明确：若包含 Windows，必须先完成 17 的实机矩阵；若只发布 macOS/Linux，发布说明必须标注 Windows UNVERIFIED/BLOCKED |
| 是否有新的安全或历史权限回归 | **未发现**：拒绝后无副作用、V1 不静默升级、Full 不变、子 Agent 不可用审批绕过所有权 |
| 本轮是否 commit / push / tag / release | **否**，遵守用户限制 |

## 7. 证据地图

| 内容 | 文件 |
|---|---|
| F1 审计 | `14-final-integration-audit.md` |
| 真实 DeepSeek A–F / Full / V1-V2 | `15-real-deepseek-dogfood.md`、`evidence/final/real-model/`、`evidence/final/real-model-index.json` |
| MCP / 子 Agent / 后台 | `16-live-mcp-and-agent.md`、`evidence/final/mcp/protocol-{macos,linux}.log` |
| Windows | `17-windows-native-acceptance.md`、`evidence/final/windows-gnu-check.log` |
| Linux | `18-linux-completion.md`、`evidence/final/linux/` |
| 集成门禁 | `evidence/final/{workspace-test,fmt-check,clippy,workspace-check,release-build}.log` |
| 机器可读索引 | `13-evidence-index.json` |
