# Windows 11 已确认限制与未验证项

本文件只记录本次真机执行得到的限制；它们不是推测，也不把未执行项写成通过。

## L1 — Git Bash/MSYS 工具在 Low Integrity 沙箱内无法初始化（P2）

复现：

```powershell
mkdir C:\qlab\msys-probe
echo x > C:\qlab\msys-probe\file.txt
icacls C:\qlab\msys-probe /setintegritylevel (OI)(CI)L
leveler-confine.exe -- cmd /c "rm file.txt"
```

结果：

```text
0 [main] rm (8852) ...\Git\usr\bin\rm.exe: *** fatal error -
NtCreateDirectoryObject(\BaseNamedObjects\msys-2.0S5-...): 0xC0000022
exit 127；file.txt 仍存在
```

原生 `rmdir /S /Q` 在同一 Low Integrity 写根内成功（exit 0）。因此这是 MSYS/Cygwin 运行时与 Mandatory Integrity Control 的兼容限制，不是权限判定被绕过：受保护操作仍然没有发生。

影响与替代：Agent 默认使用 cmd/PowerShell；`cargo`、`git`、`rustc` 等原生工具正常。需要 `rm`/`printf`/`sh` 等 MSYS 工具时，应改用原生命令；FullAccess 下不受该限制（本次未用 FullAccess 掩盖标准权限体验）。

## L2 — 空环境测试夹具无法执行批处理脚本（已撤销 P1 结论，测试夹具问题）

早期手工复现使用了 `BackgroundTaskRegistry::new()` / `CommandRunner::new()`。这些构造函数读取的是进程级 `leveler_core::environment()`；在独立测试二进制中它没有被安装，命令子进程的 `env_clear()` 后只剩 `cmd` 自身补的少量变量，**缺少 `SystemRoot`**。在此环境下 `cmd /C .\run-tests.cmd` 会静默退出 1：

```text
abs empty:   code=1 out="" err=""
abs comspec: code=1 out="" err=""
abs withroot: code=0 out="marker"
```

真实产品入口（`leveler-cli` main、`Application::assemble`）在启动时安装完整宿主快照，`SystemRoot` 存在，批处理脚本正常。修复后的 Windows 原生回归测试 `background::tests::a_background_batch_script_runs_with_the_host_environment` 使用 `host_registry()`（完整快照）在真实后台注册表中运行 `.cmd`，exit 0 且日志含 `marker`。

因此早期“后台 `.cmd` 失败”的 P1 结论不成立；保留为测试夹具约束：**任何绕过产品入口、直接构造 registry/runner 的测试必须传入完整宿主环境快照**，否则 `cmd` 不能执行批处理文件。产品不负责把缺失的宿主变量补进显式空快照。

## L3 — `NoDefaultCurrentDirectoryInExePath=1` 下的相对脚本路径（P2/环境）

本机环境包含：

```text
NoDefaultCurrentDirectoryInExePath=1
```

因此 `cmd /C run-tests.cmd`（裸名）不会搜当前目录，而 `cmd /C .\run-tests.cmd` 可以。产品没有静默移除该用户安全设置；测试夹具改为显式 `.\` 或绝对路径。普通 Windows 用户通常不设置该变量；设置它的用户需要显式路径。

## L4 — Windows Terminal 原生 Ctrl+V 的多行/非 BMP 限制（P2，宿主）

Windows Terminal 1.25.3131.0 的原生 Ctrl+V / Ctrl+Shift+V 在 ConPTY 应用中把换行当 Enter、丢失非 BMP 字符：

- 单行 BMP 中文：成功（`evidence/tui-ctrlc-paste.png`）。
- 多行 + emoji：文本被拆成多条消息提交，emoji 变为空（`evidence/tui-multiline-paste.png`、`evidence/tui-ctrlshiftv-paste.png`）。

产品 Footer 已提示 `Alt+V 粘贴`；Alt+V 读取真实 Unicode 剪贴板，多行与 🙂 均完整、不提交（`evidence/tui-altv-multiline.png`）。这是宿主不可控路径，不是本次引入的缺陷。

## L6 — Task Scheduler 在非提升会话中拒绝 XML 注册（P1，未修复）

`LEVELER_TASK_SCHEDULER_ACCEPTANCE=1 cargo test -p leveler-schedule --test task_scheduler_acceptance` 在本次非提升 Medium 会话中失败：

```text
schtasks.exe /Create failed (exit code: 1): 错误: 拒绝访问。
```

同一账户下，简单任务 `schtasks /Create /SC MINUTE /MO 1 /RL LIMITED /RU code /IT /TR "cmd /c exit 0"` 成功；带 `/XML` 的最小任务同样被拒绝。因此失败发生在 **XML 注册路径**，不是 Task Scheduler 服务或账户完全不可用。产品当前的 Windows wakeup 后端固定使用 `/Create /XML`；它在本机无法由非提升用户启用，属于真实功能缺陷/平台实现选择问题，本次未修复。

影响：计划任务/唤醒功能在本机无法完成真机执行验收（注册、去重、按需运行均未进入）；schedule 的其余 pure logic 由 workspace 测试覆盖。不得用提升管理员运行来掩盖该失败。

## L5 — 未验证 / BLOCKED 项

| 项 | 状态 | 原因 |
| --- | --- | --- |
| 独立标准用户（非 Administrators） | BLOCKED | 创建需要管理员授权；未把管理员组非提升 Medium 等同于标准用户 |
| Console Host 独立窗口 | BLOCKED | Windows 11 默认委派给 Windows Terminal；未修改 `HKCU\Console\%%Startup` |
| PowerShell 7 | NOT VERIFIED | 未安装 |
| VS Code Integrated Terminal | NOT VERIFIED | 未安装 |
| 中文 IME 候选/组合输入 | NOT VERIFIED | 无法用合成按键可靠驱动系统 IME；需人工按报告步骤验证 |
| 160×45 精确网格 | PARTIAL | 1080p + 当前字体不可达；以最大化窗口作为大窗口近似 |
| 图片渲染失败注入 | NOT VERIFIED | 能力未知/隐藏路径已验证；渲染失败未注入 |
| Release Lab / RC | BLOCKED | 仓库/本机无 `eval/scripts/release_check.py`、`rc_check.py` |
| 远端 CI | NOT RUN | 未授权 push/远端运行 |
| macOS/Linux 真实终端回归 | NOT VERIFIED | 本机无对应宿主 |
