# 日常使用

[English](USAGE.md) · [文档索引](README.zh-CN.md)

[模型配置](#配置模型) · [会话](#会话与后台运行) · [命令行](#命令行运行) · [审查](#审查) · [排查问题](#排查问题)

## 配置模型

运行 `leveler login` 选择服务商并配置模型。配置保存在 `~/.leveler/config.toml`；接入其他 OpenAI 兼容服务时，先运行 `leveler init` 再编辑该文件。

改完配置后，下一轮对话会使用新配置，也可以输入 `/reload` 立即重读。正在执行的回合仍用开始时的模型和服务商。用过 `/model` 手动选模型的会话会保留选择，其他会话跟随默认模型。

如果新配置无效，程序继续用上一版。界面语言等只能在启动时读取的设置会提示重启。

在终端里用 `/thinking high` 调整当前会话的思考强度，`/thinking reset` 恢复配置默认值。可选值为 `auto`、`off`、`minimal`、`low`、`medium`、`high`、`max`，具体支持情况见 `leveler doctor`。默认是 `high`，`max` 使用当前模型支持的最高档，`auto` 使用服务商默认值。

`--deny-network`（旧别名 `--sandbox`）是运行级限制：它禁止 `run_command` 进程以及网络工具访问网络，Full 权限也不会撤销它。`computer_act` 驱动的桌面自动化运行在进程沙箱之外，**不受**该参数约束；需要保持离线时应避免或拒绝这类动作。后端无法强制断网时会拒绝执行，而不是放开网络运行。

服务商的暂时性故障（连接被拒绝或重置、可重试的超时、429、可重试的 5xx）会自动重试，间隔逐步拉长并带随机偏移，原任务会保留。默认 10 分钟内最多重试 12 次。按 `Ctrl+C` 取消等待。在 `~/.leveler/config.toml` 里可以调整这两个值。

```toml
[retry]
max_retries = 12        # 首次尝试之后的自动重试次数；0 表示关闭自动恢复
recovery_seconds = 600  # 重试（等待与请求执行）的总时长上限，从首次尝试开始计算
```

永久性错误（认证失败、请求无效、会话被拒绝）不会重试。预算耗尽后按可恢复失败报告：任务和已完成的工具结果都会保留，发送新消息即可继续。

### 自定义 OpenAI 兼容服务的模型发现

以 A2Agent 为例，先在 `~/.leveler/config.toml` 登记服务地址和 API Key 的环境变量名，再单独配置要用的模型。

```toml
[providers.a2agent]
base_url = "https://api.a2agent.me/v1"
api_key_env = "A2AGENT_API_KEY"
```

在本地将 `A2AGENT_API_KEY` 设为你自己的 key，不要把真实 key 写入共享配置或示例。
手动配置的模型条目使用 `provider = "a2agent"`，模型 ID 和限制应以所选端点已核实的
信息为准；出现在目录中不代表模型具备某项能力。

列出模型时只请求 `/models`。缺少 `data` 或返回空数组时，列表为空。若 `data` 不是数组，程序会报格式错误。查询失败时，已手动配置的模型仍可选。

## 输入与查看结果

- `Ctrl+R` 搜索输入历史，Enter 放回草稿，不会直接发送。
- `Ctrl+Z` 撤销输入，`Ctrl+Shift+Z` 或 `Alt+Z` 重做。
- 点击工具组查看执行详情，`Ctrl+O` 打开最新执行，`Ctrl+G` 跳到最终回答。
- 输入 `/技能名` 启用 skill。CodeLeveler 交付流程正文和已校验的参考文件清单，执行时仅向模型返回所请求的参考文件内容。
- 已完成的回复末尾提供 `[Copy Markdown]` 和 `[Copy Text]`，分别复制完整 Markdown 原文和纯文本。可以直接点击，也可以在对话区按 Tab 选择，再按 Enter 复制。
- 空输入框输入 `!` 进入 Shell Mode，Tab 补全命令和路径。
- `/output` 查看完整本地命令输出，`/copy output` 复制输出。
- 文件编辑默认展开完整真实 Diff，点击编辑头行或按 `Ctrl+O` 折叠为一行 `+A/-R` 摘要。在 Diff 上拖选复制的是选中源码行（不含行号与 `+`/`-` 标记）；`/copy diff` 复制完整已应用补丁。
- `Ctrl+B` 将运行中的本地命令转入后台，`Ctrl+C` 停止该次命令。
- Windows Terminal 在原生控制台程序里处理 `Ctrl+V` 时会把换行当作 Enter，并把非 BMP Emoji 变成 `?`。在 Windows 上使用 `Alt+V` 直接读取系统剪贴板并按一次粘贴插入，保留换行和 Emoji；`Ctrl+V` 仍可用于图片或单行文本。

点击底部后台任务入口可以查看任务、输出或单独停止任务。Esc 只返回列表；连接后台服务时，关闭终端也不会停止后台任务。

前台命令何时转到后台，取决于你有没有设置 `timeout_seconds`。超时从进程启动时算起。

| 设置 | 行为 |
| --- | --- |
| 未指定超时 | 命令运行满 15 秒还没结束时转到后台，返回 `task_id` |
| 超时不超过 120 秒 | 在这次调用中等待，到了期限就结束命令 |
| 超时大于 120 秒 | 等到 120 秒时将仍在运行的命令转到后台，原来的超时期限继续生效 |

拿到 `task_id` 后，可用 `get_task` 或 `wait_task` 查看最终结果，用 `kill_task` 停止。要直接在后台启动长期服务，设置 `background=true`。用 `background_lifetime` 指定它跟随 Goal、运行时或会话结束，或设为 `persistent`。默认值是 `goal`。

工具组摘要显示从实际调度到执行结束的耗时，重连后沿用运行时记录。有可靠计时数据的执行才会显示耗时。权限对话框默认选中**仅允许本次**，按 Esc 拒绝。

## 凭据接管

在聊天里直接给出 API Key、Token 或密码时，CodeLeveler 会先在本地识别，默认不把明文交给模型：

- 高置信度或需要确认的值会打开审查：**安全接管并继续**（默认）、**作为普通文本发送**、**返回编辑**。Esc 永远不发送，原草稿保留。
- 未知格式或自动识别漏掉的值，用 `/secret` 打开掩码输入框。粘贴内容不回显；Enter 接管后会把 `[secret:sec_…]` 引用插入草稿，继续说明要配置到哪里。
- 被接管的明文只进入本会话的运行时 Vault。模型只收到引用，并通过 `secret_apply` 请求 Host 写入工作区的 `.env`（或 `.env.*`）；模型不会从 Vault 或工具结果中拿到明文（写入后的 `.env` 见下一条）。
- 消息里写出的 `.env` / `.env.*` 目标，以及 `FIELD=` 形式的字段名，会绑定这次接管的用途。Agent 只能写入这些目标和字段；请求其他位置时会被拒绝，并应请你先在消息里明确说明。Full 模式不弹审批，但同样不能越过这条捕获授权。
- 选择“作为普通文本发送”是明确的风险决定：明文会进入模型请求和 Provider。本机会话记录仍走既有脱敏；“明文”只代表模型与 Provider 能看到它，不代表它会被重复持久化。
- 审查界面之外的客户端（包括旧客户端、直接 IPC 和 `leveler run`）遇到任何检测到的凭据都会被拒绝，而不是静默放行。`leveler run --allow-plaintext-secrets` 是显式选择明文发送的入口。
- Vault 只存在于当前进程。重启或 resume 后，历史里的旧引用会失效，需要重新提供凭据。`/secret revoke <sec_…>` 可以立即撤销本会话中的某个引用。
- 写入后的 `.env` 是真实明文文件。它能否被其他进程读取或外发，取决于当前权限模式与 sandbox；Full 模式不提供隔离保证。
- 不支持安全接管的客户端（例如 `leveler run` 的纯文本参数）遇到检测到的凭据会被拒绝，而不是静默外发；显式加 `--allow-plaintext-secrets` 才按明文发送。
- 模型 Provider 自身的 API Key 仍通过 `leveler login` 或 `api_key_env` 配置，与本功能相互独立。

## 会话与后台运行

`leveler resume` 继续已有会话。macOS 和 Linux 还可以用 `leveler serve` 保持后台运行；Windows 暂不支持本地后台服务。

如果持有后台服务的进程异常退出（崩溃或被终止），`leveler tui` 仍可启动：受影响的任务会标记为 `unknown`，启动提示会说明其进程是否仍然存在。此时 CodeLeveler 不会终止或重新执行这些任务；用 `leveler background list` 查看遗留任务，用 `leveler background logs` 查看保留的输出，用 `leveler background journal list` 查看仍未解决的写入预留。

同一项目的多个 TUI 共用运行时。最后一个界面断开后，运行时会继续处理已有工作。没有活跃轮次、运行时持有的后台工作或本地界面时，开始累计空闲时间，默认连续空闲 30 分钟后自动退出。`LEVELER_DAEMON_IDLE_TIMEOUT_SECS` 可为启动的运行时指定正整数秒数。

如果升级时旧运行时仍在工作，交互式启动会等待安全交接。旧运行时若上报空闲且没有本地界面连接，新版本会核实其进程身份后请求它协作退出：它先停止接收新工作，完成任务后退出；不会取消任务、不会发送信号，也不依赖磁盘上二进制路径或哈希。仍有任务或仍被界面使用时，启动会说明具体原因并继续等待；按 Ctrl+C 可以停止等待。无法核实实例身份时不执行任何接管或终止，并保留安全边界。

独立执行宿主在没有活跃任务、没有未决启动意图，且持续一段时间没有请求时会自行退出；运行时正常退出时也会请求它回收。仍持有活跃服务的宿主不会退出，服务跨运行时更新继续运行。`LEVELER_EXECUTION_HOST_IDLE_TIMEOUT_SECS` 指定自回收的正整数秒数，`0` 关闭；默认 30 分钟。

- `leveler runtime status` 查看当前项目的运行实例、构建、任务计数和空闲回收状态。`--json` 输出结构化数据，无法观测的字段为 `null`。
- `leveler runtime stop` 请求运行时在安全空闲时正常退出。存在任务、实例变更或无法验证安全退出时，命令会报告原因并返回非零退出码。

正常退出 TUI 时，程序会取消本会话尚未结束的回合、停止本会话的后台任务并等待清理，其他会话继续运行。断开连接或强制结束客户端时，共享运行时中的任务继续运行。

## 命令行运行

`leveler run "任务"` 执行任务到结束，加 `--collaboration chat` 可只获取一次回答。运行 `leveler run --help` 查看参数。

```sh
leveler run "为当前解析器补上空输入的回归测试" --parallel 2 --output jsonl
```

并行运行需要干净且已有提交的 Git 工作区。它创建候选分支和 worktree，提交经过验证的候选，并把成功候选集成到当前分支。候选继承父任务的权限、网络限制、模型步数上限和状态目录。

并行模式在启动前拒绝 `--commit`、`--push`、`--pr`、`--branch`、`--pr-base`。单任务支持通过相应参数交付，文本和 JSONL 模式下交付失败均返回非零退出码。候选建立、执行或提交失败时保留原始错误，并在集成前结束。若 worktree 清理失败，残留路径会出现在警告和持久化记录中，已完成的集成仍保留。

## 定时自动化

`leveler schedule` 把定义保存在 `~/.leveler/state/schedules.db`。定义属于当前用户，每个定义在它指定的工作区运行（默认是当前仓库）。创建一个每日 shell 运行：

```sh
leveler schedule add --name nightly --command "cargo test" --daily 02:00 --timezone Asia/Shanghai --grant-unattended
```

选择一个动作（`--command`、`--native-action ID` 配合 `--native-input JSON`，或 `--agent-goal TEXT`）和一个触发器（`--at RFC3339`、`--every SECONDS`、`--daily HH:MM`、`--weekly mon,wed --local-time HH:MM`，或 `--cron "..."` 配合 `--timezone IANA`）。其余策略由 `--session-policy continue|fresh`、`--overlap skip-if-running|queue-one|allow-concurrent`（后者要求 `fresh`）、`--misfire skip|run-once` 与 `--misfire-grace SECONDS`、`--timeout SECONDS` 设置。

无人值守的运行需要 `--grant-unattended`；动作会向工作区外发送或修改内容时，再加 `--allow-external-side-effects`。修改动作、工作区、触发器、权限模式或该选择会使授权失效：用 `leveler schedule grant <id> [--allow-external-side-effects]` 为新身份重新授权。Full 权限不能替代授权。

`leveler schedule list|show|update|runs|pause|resume|run-now|cancel|remove` 查看和管理定义。`leveler schedule dispatch` 执行一轮；加 `--wait` 会继续观察已经交出的运行直到终态，不会派发后续时刻的任务或开启系统唤醒；`leveler schedule serve` 在前台持续调度。macOS 上 `leveler schedule enable` 注册 launchd agent，Windows 上注册当前用户的 Task Scheduler 任务；`disable` 与 `uninstall` 移除注册，`status` 与 `wakeups` 报告注册，`doctor` 列出诊断项。Linux 没有唤醒后端，会如实报告；可用 `serve` 或由系统调度器调用 `dispatch`。

TUI 中用 `/schedules` 打开定义列表、详情和编辑表单。Agent 通过 `schedule_inspect` 与 `schedule_manage` 工具执行同样的操作。

## 任务工作台与诊断

`/tasks` 打开一个只读任务工作台，聚合当前会话的目标、子 Agent、后台任务与审查，定时任务定义及最近一次运行，以及全局存储中的全部会话。`Tab` 在“进行中 / 最近 / 全部”之间切换；Enter 进入已有详情页（活动、目标或定时任务页）；`x` 通过运行时取消当前会话中允许取消的任务；`r` 重新读取两个快照来源。会话行会标注快照时间，子 Agent 与后台任务行实时更新。其他工作区的会话在这里只读，因为当前运行时不能打开别的工作区的存储。

`/doctor` 打开结构化诊断：运行时连接、构建、会话有效性与崩溃恢复窗口；配置；Provider 凭据；模型推理配置；MCP 配置；权限档位；沙箱能力；终端能力。每条记录都有稳定 ID、类型化状态（`pass`、`warn`、`fail`、`unknown`、`not checked`、`not applicable`）、真实观测证据和可选的已有操作。`UNKNOWN` 与 `NOT CHECKED` 永远不会显示为通过。`R` 重新执行共享检查；任务运行时也可以打开，且不会调用模型。MCP 的实时连接状态目前没有权威来源，页面会如实标注。`leveler doctor --json` 以机器可读形式输出同一组 finding。

## 原生动作与桌面操作

`leveler native capabilities` 报告 macOS 的 Mail 或 Windows 的 Outlook 是否已安装、正在运行、已登录，以及是否允许被自动化。`leveler native action` 不经过模型执行一个确定性动作：

```sh
leveler native action --action mail.draft.create --to you@example.com --subject "周报" --body-file report.txt
```

动作包括 `mail.draft.create`、`mail.send.open_ui`、`mail.send.submit` 和 `mail.sent.check`。超时或无法识别的结果会报告为 `unknown`，不会重试。macOS 需要目标应用的自动化权限；未解决的授权弹窗会报告为 `unknown`，而不是替用户假设答案。

面向模型的工具是 `native_capabilities`、`native_action`、`native_verify`、`computer_observe` 和 `computer_act`。桌面操作在 macOS 需要辅助功能与屏幕录制权限（Windows 使用 UI Automation），权限状态在调用时报告，每个观察引用在动作前都会对实时界面重新校验。只有编译进对应平台后端时这些工具才会出现；Linux 两者都没有。

## 接续 Claude Code 或 Codex 会话

在 TUI 中，`/resume-claude` 与 `/resume-codex` 列出当前工作区的本机会话；直接给出会话 ID 可跳过选择器。CodeLeveler 只读访问源存储，为选中的记录新建一个 CodeLeveler 会话，并带上简短的任务和结束状态。工作区不一致时会在导入前提示。旧会话说「已完成」的事项，要对照当前工作区重新核实。模型可以用 `foreign_session_history` 工具读取原始对话。

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

可以在启动 CodeLeveler 前设置对应环境变量，也可以在本机 `~/.leveler/config.toml` 的 `[web.search]` 中写入 `api_key = "your-search-key"`。非空的 `api_key` 优先。如果省略 `api_key` 或只填空白，CodeLeveler 会读取 `api_key_env` 指定的变量。省略 `api_key_env` 时，DeepSeek 默认使用 `DEEPSEEK_API_KEY`，Tavily 默认使用 `LEVELER_SEARCH_API_KEY`。`[web.search]` 独立于对话供应商启用搜索。

`leveler config show` 显示解析后的供应商、API 基础地址和凭据状态，Key 的值会隐藏。`leveler doctor` 提供 `config: web search` 检查项。在受限命令中，两者都会标明看到的是沙箱内的配置。界面链接使用系统默认浏览器。

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

也可以运行 `leveler mcp add devorder --url http://127.0.0.1:8081/mcp --header X-API-Key=DEVORDER_MCP_API_KEY`。Bearer 用 `--bearer-env SOME_TOKEN` 或 `bearer_token_env = "SOME_TOKEN"`，发出去的是 `Authorization: Bearer <值>`。请求头的值同样是环境变量名。不要把令牌写进 URL。HTTPS 会校验服务端证书。目前不支持 OAuth 2.1。只接受 OAuth 的服务无法用静态 Bearer 配置接入。

`leveler mcp list` 显示传输方式、地址和请求头名称。`leveler doctor` 会在引用的环境变量未设置时给出警告。工具页在调用发生后显示该次调用；连接状态写在日志里（`MCP server connected`，含协商到的协议版本和工具数量）。

## 审查

在终端界面输入 `/review`，可以手动发起只读审查。每次审查有独立的 ID、预算和持久化报告，也可以单独取消。该功能在 v1.0.27 之后加入，见[版本历史](../CHANGELOG.zh-CN.md)。

### 选择审查范围

单独输入 `/review` 会打开范围选择。默认只看当前任务已接受的交付和该交付改过的文件。也可以在命令里直接指定范围，再写想重点检查什么。

```text
/review 检查支付模块的安全边界
/review @workspace 检查当前未提交改动
/review @project 检查整个项目
/review @session <session-id> 分析这次执行过程
/review @commit <rev> 检查这次提交
/review @branch <base> 检查当前分支相对 base 的提交
/review @path src/a.rs,src/b.rs 检查这两个文件的边界
```

在审查重点前加 `@model provider/model` 可指定本次模型。用 `/review cancel [review-id]` 停止正在运行的审查。

### 审查使用的证据

审查从你的原始需求、后续补充和指定的改动开始。有差异可读时，程序会预先收集有限数量的改动符号和跨文件调用位置，也会提供项目文档入口。启用跨会话记忆时，审查还可以通过只读的 `memory_search` 查相关笔记，并用当前源码核对。

- 审查会话时，`session_history` 只能读取选定会话。它可以分页查看消息、回合结果、工具调用、模型调用的耗时和 token、需求变更及检查点。普通回合没有这个工具。
- 查符号和引用时可直接用 `find_symbol`、`read_symbol`、`find_references`、`diagnostics`。审查用进程内扫描，无需启动语言服务器或提前启用能力。
- 审查提交或分支时，范围固定在解析出的提交 SHA。读取当前工作区的工具不可用。`git_history` 只查询审查对象及其祖先提交，支持 `log`、`show`、`file`、`search`、`list` 和 `blame`。

### 调查与预算

遇到复杂改动，主审可以用 `spawn_agent` 把问题分给多个调查者。调查者各自读取同一个审查对象，可以并行或在后台工作。它们只能读取审查范围内的材料，不能写文件、运行命令、读取凭据、切换 Agent 或继续委派。每条候选问题会保留来源，主审核对证据后给出一份报告。

主审与调查者共用项目 `limits` 中的 token 和费用上限。整次审查最多 20 分钟，项目可设更短期限。用完预算后，程序停止发起新请求，保留已经取得的证据，并标明没查完的范围。

### 查看审查报告

结束后点对话里的摘要卡或「查看报告」。卡片显示各严重度计数、问题总数和源码是否有变化。重新打开会话仍可查看。若审查期间对象发生变化，报告会标明。

报告会列出发现了什么、实际查了哪些范围，以及环境、权限或工具造成的限制。每条问题可附分类、P0–P3 严重度、置信度、证据、影响、修复方向和复现情况。结论和覆盖范围由审查者通过 `report_review` 提交。审查结果单独记录，Goal 和普通回合的状态由各自的流程决定。

调查者用 `report_finding` 提交的只是候选。主审用 `adjudicate_finding` 逐条采纳（`adopted`）、拒绝（`rejected`）、合并同一根因（`duplicate`），或留作未决（`unresolved`）。正式问题数只统计主审报告的问题和采纳的候选，未决项单独计数。拒绝和合并的记录仍可追查。

`report_review.report` 是报告正文。最后一条聊天消息会单独保存。主审提交报告时，程序会记下它依据的证据版本。之后若有新调查结果、新发现或裁决变化，且没重交完整报告，界面会标明这份报告没覆盖最新证据。

「审查完成」表示审查过程已结束。「已审查」和「未发现问题」记录的是审查者给出的范围和结论。要判断还有哪些地方没查到，可以看报告里的覆盖范围和限制。如果审查停止或取消，已经确认的内容和停止原因仍会保留。

## 排查问题

先运行 `leveler doctor` 检查模型、终端和系统能力；`leveler trace` 查看会话记录。分享记录前请检查是否含有私密内容。

同一模型轮内相互独立的 `web_fetch` / `web_search` 调用可以并发执行，最多同时有 3 个请求在途；结果仍按原始调用顺序返回。整个进程还有更外层的资源预算：所有会话合计最多 8 个请求在途，单会话最多 4 个，同一目标主机最多 4 个，同一搜索供应商最多 2 个并发请求且每秒最多发起 20 次。这些是运行时的资源上限，不是供应商配额；供应商自己的 429 仍是权威的额度信号。等待用户批准时不占用执行许可。若某次 Web 调用在执行前、资源排队或交付阶段等待超过阈值，日志会记录一行 `leveler::web_slow` 阶段摘要。设 `RUST_LOG=leveler_web_latency=debug` 可查看每次调用的完整阶段。
