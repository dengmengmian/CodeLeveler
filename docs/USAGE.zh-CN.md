# 日常使用

[English](USAGE.md) · [返回首页](../README.zh-CN.md)

## 配置模型

运行 `leveler login` 选择服务商并配置模型。配置保存在 `~/.leveler/config.toml`；接入其他 OpenAI 兼容服务时，先运行 `leveler init` 再编辑该文件。

程序运行期间会重新读取配置：修改 `~/.leveler/config.toml`（或仓库内的配置包）后，下一轮对话自动使用新配置；`/reload` 可立即重新读取，无需重启。正在执行的任务继续使用开始时的模型和服务商；从未显式选择模型的会话跟随新的默认模型，而通过 `/model` 明确选择的模型保持不变。配置无效或只写入一半时会保留上一版可用配置。只能启动时生效的设置（例如界面语言）会明确报告需要重启，而不会伪装成已生效。

在终端里用 `/thinking high` 调整当前会话的思考强度，`/thinking reset` 恢复配置默认值。可选值为 `auto`、`off`、`minimal`、`low`、`medium`、`high`、`max`，具体支持情况见 `leveler doctor`。默认是 `high`，`max` 使用当前模型支持的最高档，`auto` 使用服务商默认值。

服务商的暂时性故障（连接被拒绝或重置、可重试的超时、429、可重试的 5xx）会自动按指数退避加抖动重试，不需要用户重发原任务。默认预算为 10 分钟内最多 12 次重试；`Ctrl+C` 可取消等待。可在 `~/.leveler/config.toml` 中调整：

```toml
[retry]
max_retries = 12        # 首次尝试之后的自动重试次数；0 表示关闭自动恢复
recovery_seconds = 600  # 重试（等待与请求执行）的总时长上限，从首次尝试开始计算
```

永久性错误（认证失败、请求无效、会话被拒绝）不会重试。预算耗尽后按可恢复失败报告：任务和已完成的工具结果都会保留，发送新消息即可继续。

## 输入与查看结果

- `Ctrl+R` 搜索输入历史，Enter 放回草稿，不会直接发送。
- `Ctrl+Z` 撤销输入，`Ctrl+Shift+Z` 或 `Alt+Z` 重做。
- 点击工具组查看执行详情，`Ctrl+O` 打开最新执行，`Ctrl+G` 跳到最终回答。
- 空输入框输入 `!` 进入 Shell Mode，Tab 补全命令和路径。
- `/output` 查看完整本地命令输出，`/copy output` 复制输出。
- 文件编辑默认展开完整真实 Diff，点击编辑头行或按 `Ctrl+O` 折叠为一行 `+A/-R` 摘要。在 Diff 上拖选复制的是选中源码行（不含行号与 `+`/`-` 标记）；`/copy diff` 复制完整已应用补丁。
- `Ctrl+B` 将运行中的本地命令转入后台，`Ctrl+C` 停止该次命令。

点击底部后台任务入口可以查看任务、输出或单独停止任务。Esc 只返回列表；连接后台服务时，关闭终端也不会停止后台任务。

未设置 `timeout_seconds` 时，前台命令超过 15 秒仍没结束会转入后台并返回 `task_id`。设置了 `timeout_seconds` 时，调用会等待命令：不超过 120 秒的期限会在本次调用内结束命令；更长的期限会先等待到 120 秒，再把仍在运行的进程转入后台，期限继续跟随该进程。转入后台不是成功退出，之后用 `get_task` / `wait_task` 观察、用 `kill_task` 停止；`timeout_seconds` 是从进程启动计时的执行期限。需要长期运行的服务用 `background=true` 显式启动，它的生命周期由 `background_lifetime`（默认 `goal`，可选 `runtime` / `session` / `persistent`）决定，而不是由等待预算决定。

## 会话与后台运行

`leveler resume` 继续已有会话。macOS 和 Linux 还可以用 `leveler serve` 保持后台运行；Windows 暂不支持本地后台服务。

如果持有后台服务的进程异常退出（崩溃或被终止），`leveler tui` 仍可启动：受影响的任务会标记为 `unknown`，启动提示会说明其进程是否仍然存在。此时 CodeLeveler 不会终止或重新执行这些任务；用 `leveler background list` 查看遗留任务，用 `leveler background logs` 查看保留的输出，用 `leveler background journal list` 查看仍未解决的写入预留。

`leveler run "任务"` 执行任务直至结束；只需一轮回答时，添加 `--collaboration chat`。

需要多个候选实现时，可用 `leveler run "任务" --parallel 3`。这要求 Git 工作区干净且已有提交，会创建分支和 worktree、提交通过验证的候选，并集成回当前分支。

## 定时自动化

`leveler schedule` 把定义保存在 `~/.leveler/state/schedules.db`。定义属于当前用户，每个定义在它指定的工作区运行（默认是当前仓库）。创建一个每日 shell 运行：

```sh
leveler schedule add --name nightly --command "cargo test" --daily 02:00 --timezone Asia/Shanghai --grant-unattended
```

选择一个动作（`--command`、`--native-action ID` 配合 `--native-input JSON`，或 `--agent-goal TEXT`）和一个触发器（`--at RFC3339`、`--every SECONDS`、`--daily HH:MM`、`--weekly mon,wed --local-time HH:MM`，或 `--cron "..."` 配合 `--timezone IANA`）。其余策略由 `--session-policy continue|fresh`、`--overlap skip-if-running|queue-one|allow-concurrent`（后者要求 `fresh`）、`--misfire skip|run-once` 与 `--misfire-grace SECONDS`、`--timeout SECONDS` 设置。

无人值守的运行需要 `--grant-unattended`；动作会向工作区外发送或修改内容时，再加 `--allow-external-side-effects`。修改动作、工作区、触发器、权限模式或该选择会使授权失效：用 `leveler schedule grant <id> [--allow-external-side-effects]` 为新身份重新授权。Full 权限不能替代授权。

`leveler schedule list|show|update|runs|pause|resume|run-now|cancel|remove` 查看和管理定义。`leveler schedule dispatch` 执行一轮；`leveler schedule serve` 在前台持续调度。macOS 上 `leveler schedule enable` 注册 launchd agent，Windows 上注册当前用户的 Task Scheduler 任务；`disable` 与 `uninstall` 移除注册，`status` 与 `wakeups` 报告注册，`doctor` 列出诊断项。Linux 没有唤醒后端，会如实报告；可用 `serve` 或由系统调度器调用 `dispatch`。

TUI 中用 `/schedules` 打开定义列表、详情和编辑表单。Agent 通过 `schedule_inspect` 与 `schedule_manage` 工具执行同样的操作。

## 原生动作与桌面操作

`leveler native capabilities` 报告 macOS 的 Mail 或 Windows 的 Outlook 是否已安装、正在运行、已登录，以及是否允许被自动化。`leveler native action` 不经过模型执行一个确定性动作：

```sh
leveler native action --action mail.draft.create --to you@example.com --subject "周报" --body-file report.txt
```

动作包括 `mail.draft.create`、`mail.send.open_ui`、`mail.send.submit` 和 `mail.sent.check`。超时或无法识别的结果会报告为 `unknown`，不会重试。macOS 需要目标应用的自动化权限；未解决的授权弹窗会报告为 `unknown`，而不是替用户假设答案。

面向模型的工具是 `native_capabilities`、`native_action`、`native_verify`、`computer_observe` 和 `computer_act`。桌面操作在 macOS 需要辅助功能与屏幕录制权限（Windows 使用 UI Automation），权限状态在调用时报告，每个观察引用在动作前都会对实时界面重新校验。只有编译进对应平台后端时这些工具才会出现；Linux 两者都没有。

## 接续 Claude Code 或 Codex 会话

在 TUI 中，`/resume-claude` 与 `/resume-codex` 列出当前工作区的本机会话；直接给出会话 ID 可跳过选择器。CodeLeveler 只读访问源存储，把选中的会话导入为一个新的原生会话，并附带一份有界简报继续工作。简报引用任务和源会话结束时的状态，不复制整份对话。工作区不一致会在导入前报告；另一个 Agent 的声明（包括「已完成」）在对照当前工作区验证前仍是未验证的。模型可以用 `foreign_session_history` 工具读取原始对话。

## 浏览器与搜索

浏览器自动化支持 Chrome、Edge 和 Chromium。Safari 需要打开 Remote Automation，并在配置的 `[browser]` 下设置 `default = "safari"`；Safari 无法提供控制台或网络检查。

网页搜索需单独配置，与对话模型无关。在 `~/.leveler/config.toml` 中添加：

```toml
[web.search]
provider = "deepseek" # 或 tavily
api_key_env = "LEVELER_SEARCH_API_KEY"
max_results = 5
timeout_ms = 15000
```

在启动 CodeLeveler 前设置对应环境变量。只设置 Key、不添加 `[web.search]` 不会启用搜索。界面链接和 `/web` 使用系统默认浏览器。

## MCP 服务

CodeLeveler 可以直接调用 MCP 服务的工具。stdio 是本机程序；Streamable HTTP 是 `http://` 或 `https://` 地址。两种工具都显示为 `mcp__<服务名>__<工具名>`。某一个服务连不上时，其他服务和本地工具仍然可用。

stdio 配置与以前相同：

```toml
[[mcp_servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[mcp_servers.env]
GITHUB_TOKEN = "GITHUB_TOKEN"
```

`env` 的值是环境变量名，不是密钥本身。

Streamable HTTP：

```toml
[[mcp_servers]]
name = "devorder"
transport = "streamable_http"
url = "http://127.0.0.1:8081/mcp"

[mcp_servers.headers]
X-API-Key = "DEVORDER_MCP_API_KEY"
```

也可以运行 `leveler mcp add devorder --url http://127.0.0.1:8081/mcp --header X-API-Key=DEVORDER_MCP_API_KEY`。Bearer 用 `--bearer-env SOME_TOKEN` 或 `bearer_token_env = "SOME_TOKEN"`，发出去的是 `Authorization: Bearer <值>`。请求头的值同样是环境变量名。不要把令牌写进 URL。HTTPS 会校验服务端证书。OAuth 2.1 还没有实现：只支持 OAuth 的服务不能靠粘贴一个 Bearer 冒充已经接入。

`leveler mcp list` 显示传输方式、地址和请求头名称。`leveler doctor` 会在引用的环境变量未设置时给出警告。工具页在调用发生后显示该次调用；连接状态写在日志里（`MCP server connected`，含协商到的协议版本和工具数量）。

## 排查问题

先运行 `leveler doctor` 检查模型、终端和系统能力；`leveler trace` 查看会话记录。分享记录前请检查是否含有私密内容。
