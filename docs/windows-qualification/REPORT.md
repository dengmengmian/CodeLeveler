# CodeLeveler Windows Platform Final Closure

> 状态：Task Scheduler P1 已在本轮修复并以真实非提升宿主、真实到期触发和真实业务副作用验证关闭。Windows 11 当前候选在已执行范围内 **PASS**。独立标准用户、Console Host、PowerShell 7、VS Code、中文 IME、Release Lab/RC、远端 CI、macOS/Linux 与 Windows 10 当前候选仍为 BLOCKED / NOT VERIFIED；这些外部项不因本地通过而放宽。未执行 push、tag、release 或远端写操作。

## 1. 候选与版本身份

| 项目 | 值 |
| --- | --- |
| 版本 | 1.0.27 |
| 最终代码 HEAD | `7966c829697356afaef0881755894fff5bd78d59` |
| 候选构建 revision | `03def7aaeecb07632a9ab74c47862eb6310799c2`（代码提交 + 仅证据的 docs 提交；无源代码差异） |
| 二进制 provenance | `1.0.27 (03def7aa)`，干净构建（无 dirty/UNTRUSTED） |
| 本报告文档 HEAD | `03def7aaeecb（证据提交；最终文档提交在收口提交中记录）` |
| 基线 `main` | `644d765c6208eba7b59bc0db88559a0445036135` |
| 本轮代码提交 | `7966c829`：Windows LogonTrigger 绑定当前用户；Windows doctor 不再比较平台无法保存的 environment map |
| 上一候选（保留证据） | `4bc08296`（1.0.27；Task Scheduler P1 未修复） |
| 构建方式 | `cargo build --release --locked -p leveler-cli -p leveler-win-confine`，工作树干净 |
| 归档 | `C:\qlab\artifacts\candidate-7966c829\leveler-v1.0.27-x86_64-pc-windows-msvc.zip` |
| 安装/验收方式 | 从正式 ZIP 解压到 `C:\qlab\install\candidate-7966c829\leveler-v1.0.27-x86_64-pc-windows-msvc\`；未覆盖用户已安装的 `C:\Users\code\.local\bin\leveler.exe` |

证据归属：`4bc08296` 的 TUI、Dogfood A/B/E、MCP、Edge、安全 canary 与全量测试结论保留为**上一候选证据**；本轮 `7966c829` 只修改 schedule wakeup/doctor，不对未改动的 agent/TUI 代码重复完整专项。凡未在 `7966c829` 上重跑的项目均明确标注，不自动继承为新候选证据。

## 2. 真机环境

完整身份见 [ENVIRONMENT.md](ENVIRONMENT.md)。要点：

- Windows 11 家庭中文版，Build 26300.9711，x64。
- Windows Terminal 1.25.3131.0；PowerShell 5.1 / CMD / Git Bash；无 PowerShell 7、无 VS Code、无 WSL 发行版。
- zh-CN，ANSI/OEM 代码页 936；1920×1080 @150% DPI；已安装 Microsoft Pinyin，但本轮未用合成输入驱动 IME 组合/候选。
- 当前账户 `DESKTOP-6EGC57E\code` 属于 Administrators 组，但 token 未提升，进程 IL 为 Medium。
- 本机唯一启用的本地账户为 `code`；没有已准备好的独立标准用户。创建标准用户需要管理员授权，本轮未执行。
- `HKCU\Console\%%Startup` 未设置，Windows 11 将传统控制台委派给 Windows Terminal；本轮未修改用户终端设置。
- Release Lab 脚本 `eval/scripts/release_check.py` / `rc_check.py` 不在本仓或本机；`./dev pre-release` 以 `DOGFOOD_ROOT_NOT_FOUND` 阻断。

## 3. 方法与证据原则

- 根因先于修复：保留修复前失败证据，用最小对照（无 LogonTrigger / 无 UserId / 有 UserId / 简单 `schtasks`）定位到具体 XML 语义，而不是把“非提升用户”当作不可修复前提。
- 产品入口优先：真实业务验收通过 `leveler schedule add|enable|run-now|cancel|update|grant|pause|resume|disable` 与真实 Task Scheduler 到期唤醒执行，不只手写 `schtasks` 或 `/Run`。
- 候选身份优先：最终业务验收使用从正式 ZIP 解压的 `leveler.exe`；修复过程中的 debug 构建只作功能定位，不冒充出货字节。
- 安全结论以真实身份、IL、文件副作用和退出码为准，不以错误文案或“命令返回 0”代替。
- 未执行的项标记 NOT VERIFIED；环境不允许的项标记 BLOCKED，不以其他宿主结果外推；不把不同性质的缺失混为一谈。

## 4. Task Scheduler P1：根因、修复与真机验收

### 4.1 修复前的真实失败

上一候选 `4bc08296` 在非提升 Medium 会话中：

```text
LEVELER_TASK_SCHEDULER_ACCEPTANCE=1
schtasks.exe /Create failed (exit code: 1): 错误: 拒绝访问。
```

同账户简单 `/Create` 成功；导出的简单任务 XML 重新注册也成功。证据：`evidence/task-scheduler-acceptance.json`（上一候选）与本轮 `evidence/final-7966c829/task-scheduler-root-cause.log`。

### 4.2 根因

产品 XML 的 `<LogonTrigger>` 只有 `<Enabled>` 和 `<Repetition>`，没有 `<UserId>`。Task Scheduler 语义：**没有 UserId 的登录触发器作用于所有用户**；非提升用户无权注册“所有用户”的触发器，因此返回 Access Denied。它不是“普通用户不能使用计划任务”，也不是 XML 编码/文件 ACL/任务目录权限问题。

最小对照（当前宿主，非提升 Medium，原始日志 `evidence/final-7966c829/task-scheduler-root-cause.log`）：

| 对照 | XML 形状 | 结果 |
| --- | --- | --- |
| A | 产品 XML，去掉 LogonTrigger | exit 0，创建成功 |
| B | 产品 XML，LogonTrigger 无 UserId | exit 1，拒绝访问 |
| C | 产品 XML，LogonTrigger 绑定 `DOMAIN\user` | exit 0，创建成功 |
| D | `schtasks /SC ONLOGON /RU <user> /IT`（无 XML） | exit 1，拒绝访问 |
| E | 简单 `/Create` 导出的 XML 重新注册 | exit 0，创建成功 |

### 4.3 修复

`task_scheduler_xml` 在 `<LogonTrigger>` 中写入与 Principal 相同的 `<UserId>`，顺序为 `<Repetition>`、`<Enabled>`、`<UserId>`（符合 Task Scheduler 扩展 schema）。没有提升权限、没有 SYSTEM、没有保存密码、没有修改系统 ACL 或安全策略、没有切换 FullAccess、没有伪造任务状态。

先写的回归 `a_logon_trigger_is_bound_to_the_registered_user` 在修复前失败，修复后通过；真实 `task_scheduler_acceptance` 在修复前 FAIL、修复后 PASS（注册、按需执行、IgnoreNew 去重、自然到期触发、删除）。

同轮发现的第二个 Windows 缺陷：`schedule doctor` 把 Task Scheduler 无法保存的 environment map 与期望 spec 比较，导致每次成功注册都报 `environment` FAIL。修复为在 `WakeupStatus` 记录平台是否报告 environment（Task Scheduler=false，launchd=true），仅在可比较时比较；两处 doctor（CLI 与共享 manage）各有先 Red 后 Green 的回归。

### 4.4 产品级真机验收（20 项对照）

最终候选 Release 归档二进制 + 隔离 `LEVELER_HOME`/config + 中文与空格工作区。详细 JSON/XML 见 `evidence/final-7966c829/`；生命周期与恢复场景的补充 E2E 见 `evidence/final-7966c829/debug-candidate/`（debug 构建，代码与 `7966c829` 相同，仅用于行为覆盖）。

| # | 场景 | 结果 | 证据/说明 |
| --- | --- | --- | --- |
| 1 | 创建一次性任务 | PASS | `schedule add --at` 返回 id 与 next occurrence |
| 2 | 创建周期任务 | PASS | `--every 60`；自然运行 ≥2 次 |
| 3 | 查询任务 | PASS | `list/show --json`；label、workspace、grant、next |
| 4 | 手动触发 | PASS | `run-now`；run 结算 succeeded |
| 5 | 自然到期触发 | PASS | 不调用 `dispatch`/`/Run`，由真实 Task Scheduler wake 执行；OS wake claimed=1 |
| 6 | 修改任务 | PASS | `update` 改命令；revision 前进；新命令生效、旧命令未执行 |
| 7 | 禁用/重新启用 | PASS | `pause` 后 enabled=false；`resume` 后 true；`enable` 幂等；只保留一个同名任务 |
| 8 | 取消任务 | PASS | 运行中 `cancel`；run 结算 cancelled，未伪造 succeeded |
| 9 | 删除任务 | PASS | `remove` 删除定义与运行历史；无孤儿 run |
| 10 | 同名冲突 | PASS | 重复 `enable` 使用 `/F` 覆盖同一 label；两个独立 home 的 label 不同，互不覆盖 |
| 11 | 注册失败后的状态 | PARTIAL | 修复前真实失败证据保留；`enable` 失败返回错误且无本地“已启用”伪状态；本轮无授权制造第二次系统级拒绝 |
| 12 | 成功任务结果 | PASS | 退出码 0、due.txt 含 `定时中文完成`、OS wake 记录 |
| 13 | 失败任务结果 | PASS | `exit /b 7` → state=failed、exit_code=7 |
| 14 | 中文/空格工作目录 | PASS | 最终候选 cwd=`C:\qlab\ts-final-7966c829\工作 空间` |
| 15 | 中文参数与输出 | PASS | 命令与输出保留中文；输出文件按宿主 ANSI/CP936 读取为 `定时中文完成` |
| 16 | 客户端退出后触发 | PASS（CLI 语义） | enable 的短生命周期进程已退出，真实到期 wake 独立执行；未在本轮重新驱动 TUI |
| 17 | Runtime/daemon 重启恢复 | PASS | 杀掉 serve 进程后下一 OS wake 启动新 PID，持久事件恢复并结算 succeeded |
| 18 | Session/Goal 关联与执行记录 | PARTIAL | run 绑定 durable session；Goal 动作的真实模型调度本轮未重跑（agent runtime 未改动） |
| 19 | 重复触发/并发策略 | PASS | Task Scheduler IgnoreNew 验收 PASS；两个并发 `dispatch` 对同一 due slot 只产生 1 个 run |
| 20 | 无关用户任务安全 | PASS | `disable/uninstall` 只删除本 label；无关任务保留；测试后清理本轮任务 |

实际执行身份与边界（最终候选）：

- 宿主：`DESKTOP-6EGC57E\code`，非提升，Medium（`S-1-16-8192`）。
- 定时动作子进程：同一用户，Low Integrity（`S-1-16-4096`），由 Assisted 写沙箱启动；这是安全契约，不是降级或提权。
- 实际 cwd：定义的工作区（中文+空格），不是 Task Scheduler 的 `WorkingDirectory`（后者是 home，仅用于 dispatch 进程）。
- 退出码：0；业务副作用：工作区内 `due.txt`。

## 5. 缺陷闭合

| ID | 级别 | 缺陷 | 状态 | 根因/修复 |
| --- | --- | --- | --- | --- |
| WIN11-D11 | P1 | 非提升会话无法启用 Task Scheduler wakeup | **CLOSED** | LogonTrigger 缺 UserId → any-user；绑定当前用户；真实到期业务验收 PASS |
| WIN11-D12 | P2 | Windows 注册成功后 `schedule doctor` 恒失败 | **CLOSED** | Task Scheduler 无 environment map；新增平台能力位，仅在可比较时比较；真实 doctor PASS |

上一候选的 D1–D10 保留在历史报告中，不因本轮修复被重写；本轮回归覆盖其受影响范围（见第 8 节）。

## 6. 验收结果

| 验收项 | 结果 | 证据 |
| --- | --- | --- |
| Task Scheduler 真实验收 | PASS：注册、按需执行、IgnoreNew、自然触发、删除 | `evidence/final-7966c829/task-scheduler-acceptance.log` |
| 最终候选自然到期业务任务 | PASS：真实 OS wake，非 `/Run` | `evidence/final-7966c829/product-e2e-final.json` |
| 定义生命周期 | PASS：失败 7、取消、修改/授权失效/重授权、暂停/恢复、删除 | `evidence/final-7966c829/debug-candidate/product-e2e-lifecycle.json` |
| 周期运行与 daemon 重启 | PASS：≥2 次自然运行；kill→新 daemon→新 run succeeded | `evidence/final-7966c829/debug-candidate/product-e2e-os-stage-b1.json` |
| 并发与任务隔离 | PASS：单 due slot 单 run；无关任务保留；两 home label 独立 | `evidence/final-7966c829/debug-candidate/product-e2e-os-stage-b2.json` |
| 安全 canary（独立） | PASS：11/11（Low IL、写根、Junction/.. /\\?\ /ACL Deny、大小写） | `evidence/final-7966c829/security-canary-manual.log` |
| 安全 canary（CI 同款） | `PASS：76 passed / 0 failed（3 tools + 73 execution）` | `evidence/final-7966c829/security-canaries-ci.log` |
| Workspace tests | `PASS：328 targets，7933 passed / 0 failed（PATH 含文档化便携 CPython 3.12.10；首次缺 python3 的 4 个 web fixture 已复跑 39/39）` | `evidence/final-7966c829/workspace-tests.log` |
| Workspace Clippy | `PASS：--all-targets --all-features -- -D warnings，exit 0` | `evidence/final-7966c829/clippy.log` |
| Format | `PASS：exit 0` | `evidence/final-7966c829/fmt.txt` |
| Release payload | `PASS` | `evidence/final-7966c829/release-payload.log` |
| ZIP 冒烟 | `PASS：--version 1.0.27 (03def7aaeecb)，completions 6043 bytes，confine exit 23` | `evidence/final-7966c829/package.log` |
| Browser acceptance | PASS：12 passed / 0 failed；真实 Edge（当前候选） | `evidence/final-7966c829/browser-acceptance.log` |

`./dev verify` 本轮输出 `NO_OWNED_CHANGES；version 1.0.27；verify PASS（不是独立完整验证）`：它是所有者变更检查，不是又一次独立完整验证；不作为上述门禁的替代。

### 6.1 Dogfood A–G 覆盖关系

| 场景 | 当前状态 | 证据 / 说明 |
| --- | --- | --- |
| A：基础业务实现 | PASS（4bc08296，按影响范围复用） | 真实 Provider/模型；独立测试 4/4；本轮代码未改动 agent runtime |
| B：库存对账 + CLI + 后台测试 | PASS（4bc08296，按影响范围复用） | 两次后台 `cargo test` + `wait_task`；独立输出核对 |
| C：取消与恢复 | PARTIAL | 当前候选真实取消（运行中 → cancelled）已验证；完整 TUI 退出/重启/Session Resume 后继续剩余业务未在本轮重跑 |
| D：Windows Unicode 路径 | PASS（当前候选范围） | 最终候选中文+空格工作区的真实定时业务；TUI/路径/显示复用上一候选证据 |
| E：后台 mutation 与验证证据 | PASS（确定性回归，Windows） | `task_commit_test::b12_a_background_write_expires_the_verification_it_followed` 与 `a_workspace_change_after_acceptance_prevents_verified_delivery` 在 Windows 套件运行通过；Unix-only 后台 barrier 测试覆盖启动版本窗口；真实 Goal 同场景未重跑 |
| F：长 Session / Compaction | NOT VERIFIED | 本轮未真实触发 Compaction；不以普通长对话冒充 |
| G：其他正式执行入口 | PARTIAL | MCP 真实调用为 4bc08296；本轮通过 CLI + 真实 Runtime/daemon + schedule 入口验证；未新建客户端 |

## 7. Windows 10 历史与当前候选

用户提供的 Windows 10 历史真机结论（外部基线）：

- Windows 10 Education 19045。
- 历史候选 `1.0.26 (5a4fc17480a6)`；代码提交 `5a4fc17480a63e14b29fa785778fcde92ca3b940`；文档 HEAD `4e984c3993dd1d3736ee0f1608a0b03caf8e9254`；报告目录 `docs/windows-tui-acceptance/`。
- TUI、Windows Terminal、Console Host、PowerShell/CMD、权限契约、编码修复及最终候选真实 Goal Dogfood 已验证。

本轮检索结果：

1. **历史版本已有验收结论**：以上述外部基线为准，保留为 Windows 10 历史事实，不再写成“无历史证据”。
2. **原始历史证据**：不在当前仓库（`docs/windows-tui-acceptance/` 不存在，`git log --all` 无记录，两个历史提交对象不存在）、不在本机文件系统/归档、不在用户会话记录中；未编造截图或日志。
3. **Windows 10 当前候选**：本机无 Windows 10 宿主，未运行跨机器验收 → **NOT VERIFIED**。
4. **Windows 11 当前候选**：见第 1、4、6 节。

`b1f002d`、`5a4fc174...` 等历史 SHA 在本仓不可解析；按“行为而非 SHA”核对，Display Path（`leveler-core/src/path.rs`）、OutputDecoder（`leveler-execution/src/encoding.rs`，含 UTF-8 跨读、GBK lead/trail 与截断测试）、Welcome 图片/无字符画降级（TUI splash）、Alt+V（Footer/粘贴路径）、sidecar 精确性（`leveler-execution`）均在当前代码中，并由上一候选真机证据/测试覆盖。

## 8. 工程回归（四个 Windows 11 修复）

| 提交 | 机制 | 回归结论 |
| --- | --- | --- |
| `1016b495` | 活动写根租约 + guard 独占锁 + 恢复跳过 held | 同进程/跨进程、恢复幂等、活动租约不被恢复均有测试；本轮 workspace 复用 |
| `3390713d` | overwrite 用 `std::fs::rename`；新建保留 `persist_noclobber` | 并发读 100/100；Unix/NTFS 原子语义；未引入 Memory 2.0 产品面 |
| `940ae12c` | Windows named pipe 握手 EOF 归为端点不存在 | 平台无关分类测试覆盖 NotFound/Refused/Reset/EPIPE/EOF，PermissionDenied 仍不是“不存在”；真实 runtime host 验收见上一候选 |
| `4bc08296` | Windows 原生命令/显式 `.\`/宿主环境夹具 | 保留断言，只替换不可移植的 POSIX 工具；后台 .cmd 回归仍在 |

Memory topic 的所属模块与契约：`leveler-memory` 的 topic 文件写入（`access_policy.rs`）负责用户全局 memory 的原子覆盖；本次改动只替换 overwrite 系统调用，不新增/恢复任何 Memory 2.0 功能、状态或 UI。

## 9. 工程门禁

| 门禁 | 命令 | 结果 |
| --- | --- | --- |
| Format | `cargo fmt --all -- --check` | `PASS：exit 0` |
| Workspace check | `cargo check --workspace --all-features --locked` | `PASS：exit 0（1m11s）` |
| Workspace Clippy | `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | `PASS：--all-targets --all-features -- -D warnings，exit 0` |
| Workspace tests | `RUST_TEST_THREADS=2 cargo test --workspace --all-features --locked --no-fail-fast` | `PASS：328 targets，7933 passed / 0 failed（PATH 含文档化便携 CPython 3.12.10；首次缺 python3 的 4 个 web fixture 已复跑 39/39）` |
| Sandbox canary | `cargo build -p leveler-win-confine --all-features` + `cargo test -p leveler-tools windows_` + `cargo test -p leveler-execution windows_` | `PASS：76 passed / 0 failed（3 tools + 73 execution）` |
| Release build | `cargo build --release --locked -p leveler-cli -p leveler-win-confine` | PASS（干净工作树） |
| Release payload | `sh packaging/scripts/check_release_payload.sh` | `PASS` |
| ZIP 冒烟 | 正式 ZIP 解压 + `--version`/completions/`leveler-confine exit 23` | `PASS：--version 1.0.27 (03def7aaeecb)，completions 6043 bytes，confine exit 23` |
| Release Lab / RC | `./dev pre-release` / `./dev release` | BLOCKED：`DOGFOOD_ROOT_NOT_FOUND`，无外部 dogfood lab |
| 远端 CI | GitHub Actions | NOT RUN（未授权 push/远端运行） |
| macOS / Linux | 本地跨平台 | NOT VERIFIED（本机无宿主；未运行 cross-target） |

## 10. 最终产物

版本 `1.0.27 (03def7aa)`；provenance 为干净构建（无 dirty/UNTRUSTED）。

| 文件 | SHA-256 |
| --- | --- |
| `leveler.exe` | `899a9cc635554019f48a4d9f717d7acaa4924145e904023c1963f80bf9907951` |
| `leveler-confine.exe` | `51398c17c0dfb7ae896392ba9df284927efee47e3896eb0e6a794f21ba7b9ab5` |
| `leveler-v1.0.27-x86_64-pc-windows-msvc.zip` | `c80e2f49f43ef68249ab63968f740edfebae6715ae1827658677dd707ce82628` |
| `README.md` | `d56333375683882ed0ac8928b607ed71a65bd19562a4181c7e66272c04e0fd2d` |
| `LICENSE-APACHE` | `c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4` |

ZIP：`C:\qlab\artifacts\candidate-7966c829\leveler-v1.0.27-x86_64-pc-windows-msvc.zip`；解压至 `C:\qlab\install\candidate-7966c829\leveler-v1.0.27-x86_64-pc-windows-msvc\`。归档冒烟：`--version`、`completions bash`、`leveler-confine -- cmd /d /c exit 23` 全部通过。未覆盖用户已安装的 `C:\Users\code\.local\bin\leveler.exe`。

## 11. 剩余风险与未验证项

| ID | 级别 | 项 | 状态 / 影响 |
| --- | --- | --- | --- |
| L1 | P2 | Git Bash/MSYS `rm`/`printf`/`sh` 在 Low IL 沙箱内无法初始化 | 确认；原生命令正常；替代为原生命令或 FullAccess |
| L3 | P2 | 本机 `NoDefaultCurrentDirectoryInExePath=1`，裸脚本名不被 cmd 搜索 | 环境限制；显式 `.\` 可用 |
| L4 | P2 | Windows Terminal 原生 Ctrl+V 多行/非 BMP 丢失并拆分提交 | 宿主限制；Alt+V 为可靠替代 |
| L5 | — | 独立标准用户（非 Administrators） | BLOCKED：本机仅有 `code`；创建需要管理员授权。未把非提升管理员组 token 等同于标准用户 |
| L6 | — | Console Host 独立窗口 | BLOCKED：Windows 11 默认委派 WT；未修改 `HKCU\Console\%%Startup` |
| L7 | — | PowerShell 7 / VS Code Integrated Terminal | NOT VERIFIED：均未安装 |
| L8 | — | 中文 IME 候选/组合输入 | NOT VERIFIED：Microsoft Pinyin 已安装，但未用合成的 IME 组合可靠驱动；需人工按真实键盘验收 |
| L9 | — | 160×45 精确网格 / 图片渲染失败注入 | PARTIAL / NOT VERIFIED（沿用上一候选限制） |
| L10 | — | Release Lab / RC | BLOCKED：无 `eval/scripts/release_check.py` / `rc_check.py`，`DOGFOOD_ROOT_NOT_FOUND` |
| L11 | — | 远端 CI | NOT RUN：未授权 push/远端运行 |
| L12 | — | macOS / Linux 真实终端、Windows 10 当前候选 | NOT VERIFIED：无对应宿主 |
| L13 | — | Dogfood F（长 Session/Compaction） | NOT VERIFIED：本轮未真实触发 Compaction；不以普通长对话冒充 |
| L14 | — | Dogfood E 的旧证据拒绝（Windows 真机 Goal） | 确定性回归在 Windows 套件通过（`b12_a_background_write_expires_the_verification_it_followed`、`a_workspace_change_after_acceptance_prevents_verified_delivery`）；Unix-only 后台 barrier 覆盖启动版本窗口；真实 Goal 同场景未重跑 |
| L15 | — | Defender 误报（旧 dirty 构建） | 已通过：干净构建的 CLI test binary 未被 Defender 阻断；旧 dirty 构建的误报未影响本轮；未关闭防护、未加排除。 |

安全结论不因这些限制放宽：P0 未发现；P1 Task Scheduler 已关闭；标准用户路径仍未独立验证，不能据当前非提升管理员组 token 外推。

## 12. 合并与发布资格

| 判定 | 结论 | 依据 |
| --- | --- | --- |
| Windows 11 当前候选 | **PASS（已执行范围）** | Task Scheduler P1 关闭；最终 Release 归档真实到期业务任务 PASS；doctor PASS；生命周期/恢复/并发/隔离 PASS；安全 canary PASS；构建干净 |
| Windows 10 历史版本 | **外部基线已确认** | 历史结论保留；原始证据未在当前工作区恢复 |
| Windows 10 当前候选 | **NOT VERIFIED** | 本机无 Windows 10 宿主 |
| 合并资格 | **条件性可合并（仅本地）** | 代码已本地提交；Windows 11 已执行范围通过；远端 CI、macOS/Linux 与发布门禁未运行，合并前应由远端 CI 覆盖 |
| 发布资格 | **NOT ELIGIBLE** | Release Lab/RC、远端 CI、独立标准用户、跨宿主与完整发布流水线未验证；不得以本地候选替代发布门禁 |

本轮未执行任何 push、tag、release、远端分支或远端 CI 写操作。
