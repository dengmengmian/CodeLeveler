# Windows 已确认限制与未验证项

本文件只记录真机执行得到的限制；它们不是推测，也不把未执行项写成通过。任务级缺陷已修复的条目保留闭合说明。

## L1 — Git Bash/MSYS 工具在 Low Integrity 沙箱内无法初始化（P2）

复现：

```powershell
mkdir C:\qlab\msys-probe
echo x > C:\qlab\msys-probe\file.txt
icacls C:\qlab\msys-probe /setintegritylevel (OI)(CI)L
leveler-confine.exe -- cmd /c "rm file.txt"
```

结果：MSYS 运行时无法创建 `\BaseNamedObjects\msys-2.0S5-...`（0xC0000022），exit 127，文件仍在。原生 `rmdir /S /Q` 在同一 Low IL 写根内成功。影响：Agent 默认使用 cmd/PowerShell；`cargo`、`git`、`rustc` 等原生工具正常；需要 MSYS 工具时改用原生命令；FullAccess 不用于掩盖标准权限体验。

## L2 — 空环境测试夹具无法执行批处理脚本（已撤销 P1 结论，测试夹具问题）

早期手工复现直接构造 `BackgroundTaskRegistry::new()` / `CommandRunner::new()`，子进程 `env_clear()` 后缺少 `SystemRoot`，`cmd /C .\run-tests.cmd` 静默退出 1。真实产品入口安装完整宿主快照，批处理正常。修复后的 Windows 原生回归 `background::tests::a_background_batch_script_runs_with_the_host_environment` 使用 `host_registry()`，exit 0 且日志含 `marker`。约束：任何绕过产品入口、直接构造 registry/runner 的测试必须传入完整宿主环境快照。

## L3 — `NoDefaultCurrentDirectoryInExePath=1` 下的相对脚本路径（P2/环境）

本机设置 `NoDefaultCurrentDirectoryInExePath=1`，`cmd /C run-tests.cmd` 不搜索当前目录，显式 `.\run-tests.cmd` 可用。产品未静默移除该用户安全设置；测试夹具显式化路径。

## L4 — Windows Terminal 原生 Ctrl+V 的多行/非 BMP 限制（P2，宿主）

Windows Terminal 1.25.3131.0 的原生 Ctrl+V / Ctrl+Shift+V 在 ConPTY 应用中把换行当 Enter、丢失非 BMP 字符：

- 单行 BMP 中文成功。
- 多行 + emoji 被拆成多条消息提交，emoji 丢失。
- 产品 Footer 提供 `Alt+V 粘贴`；Alt+V 读取真实 Unicode 剪贴板，多行与 🙂 完整、不提交。

这是宿主不可控路径，不是产品缺陷。

## L5 — 独立标准用户（BLOCKED）

本机唯一启用的本地账户为 `code`（Administrators 组成员，但非提升 Medium token）。没有已准备好的标准用户；创建本地标准用户、登录并安装候选需要管理员授权，本轮未执行。**不把非提升管理员组 token 等同于标准用户。**

最小人工步骤（需管理员授权，本轮未做）：

1. 管理员创建本地标准用户（不在 Administrators）。
2. 以该用户登录，解压正式 ZIP 到其可写目录。
3. 运行 `leveler doctor`、默认权限下的文件/Shell/Goal、后台任务、Task Scheduler 创建/自然触发、Session Resume 与安全拒绝。
4. 记录身份、IL、任务 owner 与文件副作用。

## L6 — Task Scheduler 非提升 XML 注册（已关闭，P1）

修复前：`schtasks /Create /XML` 在非提升 Medium 会话返回拒绝访问。根因是产品 `<LogonTrigger>` 没有 `<UserId>`，Task Scheduler 将其解释为“所有用户”的登录触发器；非提升用户无权注册。修复后绑定当前用户，真实 Task Scheduler 注册、自然到期触发与业务副作用均通过。复现/二分证据见 `evidence/final-7966c829/task-scheduler-root-cause.log` 与 `evidence/task-scheduler-final/`。

## L7 — Windows `schedule doctor` 的 environment 误报（已关闭，P2）

修复前：Task Scheduler 记录没有 environment map，doctor 仍比较 PATH/HOME/LEVELER_HOME，导致每次成功注册都报 environment FAIL。修复：`WakeupStatus` 记录平台是否报告 environment，仅 launchd 比较；CLI 与共享 manage 各有先 Red 后 Green 的回归；最终候选真实 doctor `healthy=true`。

## L8 — 未验证 / BLOCKED 项

| 项 | 状态 | 原因 |
| --- | --- | --- |
| 独立标准用户（非 Administrators） | BLOCKED | 本机仅有 `code`；创建需要管理员授权；未把非提升管理员组 token 等同标准用户 |
| Console Host 独立窗口 | BLOCKED | Windows 11 默认委派给 Windows Terminal；未修改 `HKCU\Console\%%Startup` |
| PowerShell 7 | NOT VERIFIED | 未安装 `pwsh` |
| VS Code Integrated Terminal | NOT VERIFIED | 未安装 VS Code |
| 中文 IME 候选/组合输入 | NOT VERIFIED | Microsoft Pinyin 已安装；合成输入无法可靠驱动系统 IME，需人工真实键盘验收 |
| 160×45 精确网格 | PARTIAL | 1080p + 当前字体不可达；以最大化窗口近似 |
| 图片渲染失败注入 | NOT VERIFIED | 能力未知/隐藏路径已验证；渲染失败未注入 |
| Dogfood F：长 Session / Compaction | NOT VERIFIED | 本轮未真实触发 Compaction；普通长对话不计为 Compaction |
| Dogfood E：Windows 真机旧证据拒绝 | NOT VERIFIED（Windows） | 确定性拒绝回归存在（`task_commit_test.rs`，Unix-only）；未在 Windows 跑同场景真实 Goal |
| Release Lab / RC | BLOCKED | 仓库/本机无 `eval/scripts/release_check.py`、`rc_check.py`；`DOGFOOD_ROOT_NOT_FOUND` |
| 远端 CI | NOT RUN | 未授权 push/远端运行 |
| macOS/Linux 真实终端回归 | NOT VERIFIED | 本机无对应宿主 |
| Windows 10 当前候选 | NOT VERIFIED | 本机无 Windows 10 宿主；历史证据见 `HISTORICAL_BASELINE.md` |
| 全量 workspace tests 的 CLI bin target | 已通过（干净构建） | 旧 dirty 构建的 CLI 测试二进制曾被 Defender 误报为 `TrojanDropper:Win32/Sirefef`（os error 225）；干净构建的同一 target 通过；首次缺 `python3` 的 4 个 web fixture 用文档化便携 CPython 复跑 39/39；未关闭防护、未加排除 |

