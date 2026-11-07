# CodeLeveler 1.0.23

[English](RELEASE.md) · [历史版本](../CHANGELOG.zh-CN.md)

本版加入可持久化的定时自动化、确定性的原生动作、桌面操作，以及接续本机 Claude Code 与 Codex 会话。提交的输入在重连和重启后也不再丢失。

- **定时自动化**：`leveler schedule` 与 TUI 的 `/schedules` 页面创建持久定义，可以执行 shell 命令、类型化原生动作，或一个普通的 Agent 目标。触发器支持一次性的 RFC 3339 时刻、固定间隔、每日时刻、指定星期和五段 cron 表达式，并按 IANA 时区求值。定义会记录工作区、权限模式、会话策略、重叠策略（`skip-if-running`、`queue-one`、`allow-concurrent`）、错过策略（`skip`、`run-once`）、超时和授权。运行通过租约认领；租约过期只会把该次运行记为 `unknown`，不会重试。`add`、`list`、`show`、`update`、`grant`、`revoke-grant`、`runs`、`pause`、`resume`、`run-now`、`cancel`、`remove` 管理定义；`dispatch` 执行一轮，`serve` 是常驻调度器。`schedule_inspect` 与 `schedule_manage` 工具向模型提供同一套事实，Web 与桌面端的自动化页面读取同一份运行时状态。
- **无人值守授权**：无人值守运行需要一份已持久化的授权，覆盖它的动作、工作区、触发器、权限模式和外部副作用选择。修改任一被覆盖的字段会使授权失效。Full 权限不能替代授权；缺少授权时动作会在开始前被拒绝，而不是在无人可答时弹出审批。
- **操作系统唤醒**：`schedule enable` 在 macOS 注册 launchd agent，在 Windows 注册当前用户的 Task Scheduler 任务；`disable` 和 `uninstall` 移除注册；`status`、`wakeups` 和 `doctor` 报告注册状态、唤醒历史与诊断。Linux 会把唤醒后端报告为不支持；定时仍可通过 `schedule serve` 或 `schedule dispatch` 运行。
- **原生动作**：`leveler native capabilities` 在不启动应用、不请求权限、不发送内容的前提下探测 macOS 的 Mail 或 Windows 的 Outlook。`leveler native action` 不经过模型、守护进程或会话，确定性地执行 `mail.draft.create`、`mail.send.open_ui`、`mail.send.submit` 或 `mail.sent.check`。输入在规划前校验，用户文本通过权限为 `0600` 的私有文件而不是 argv 或脚本源码传给应用，成功必须看到可识别结果标记。超时或无法识别的结果记为 `unknown`，不会重试。模型工具是 `native_capabilities`、`native_action` 和 `native_verify`。
- **桌面操作**：`computer_observe` 与 `computer_act` 通过 macOS 的 Accessibility 与 CoreGraphics，或 Windows 的 UI Automation 与 Win32 观察和操作桌面。观察引用只属于当前会话，并在每次动作前对实时界面重新校验。工具沿用既有审批策略、能力包和图片通道，不新增第二条执行路径。macOS 需要辅助功能与屏幕录制权限，权限状态作为调用时事实报告。
- **外部会话接续**：`/resume-claude` 与 `/resume-codex` 列出本机 Claude Code 或 Codex 会话（也可直接给出会话 ID），并把其中一个导入为 CodeLeveler 原生会话，附带一份有界简报。源存储只读，原始历史仍以源文件为准。简报引用任务和源会话结束时的状态，不复制整份对话；工具调用不会重放，工作区不一致会在导入前报告，`foreign_session_history` 按需读取源记录。
- **持久输入**：回合运行中提交的输入会先带着自己的身份写入日志，再被接纳。重连和恢复依据这条记录只投递一次，不会丢失或重复发送。最终答案绑定到模型显式的 `deliver_answer` 声明，并在回合结束前持久化；答案写出后进程崩溃不会再次调用模型。
- **TUI**：代码块和 diff 共用一套语法高亮，已应用的 diff 是一份可整体复制的文档，而不是拼接起来的分开着色行。`/schedules` 就地管理定义；`/resume-claude` 与 `/resume-codex` 打开外部会话选择器。
- **修复**：客户端输掉运行时启动竞争时会接管已存活的运行时，而不是永久失败；自然周期规则在 shell 仍运行时保留自己的时间网格；`allow-concurrent` 的 shell 运行各自获得独立执行上下文；一次性定义在触发器重排时保留待执行时刻；`schedule show` 不再把已完成、已取消或已错过的运行当作下一次运行；无头 CLI 输出已提交的最终答案，而不是记账回合的文本。

安装包是命令行程序，包含 macOS（Apple Silicon 与 Intel）、64 位 Linux 和 64 位 Windows。没有 Linux ARM 安装包。桌面客户端可从源码构建，不在本安装包内。安装包不含 Apple 公证或 Microsoft 签名。已有配置可以继续使用。

安装和更新见[首页](../README.zh-CN.md)。版本历史见[更新日志](../CHANGELOG.zh-CN.md)。
