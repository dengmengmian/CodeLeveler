# 日常使用

[English](USAGE.md) · [返回首页](../README.zh-CN.md)

## 配置模型

运行 `leveler login` 选择服务商并配置模型。配置保存在 `~/.leveler/config.toml`；接入其他 OpenAI 兼容服务时，先运行 `leveler init` 再编辑该文件。

程序运行期间会重新读取配置：修改 `~/.leveler/config.toml`（或仓库内的配置包）后，下一轮对话自动使用新配置；`/reload` 可立即重新读取，无需重启。正在执行的任务继续使用开始时的模型和服务商；从未显式选择模型的会话跟随新的默认模型，而通过 `/model` 明确选择的模型保持不变。配置无效或只写入一半时会保留上一版可用配置。只能启动时生效的设置（例如界面语言）会明确报告需要重启，而不会伪装成已生效。

在终端里用 `/thinking high` 调整当前会话的思考强度，`/thinking reset` 恢复配置默认值。可选值为 `auto`、`off`、`minimal`、`low`、`medium`、`high`、`max`，具体支持情况见 `leveler doctor`。默认是 `high`，`max` 使用当前模型支持的最高档，`auto` 使用服务商默认值。

## 输入与查看结果

- `Ctrl+R` 搜索输入历史，Enter 放回草稿，不会直接发送。
- `Ctrl+Z` 撤销输入，`Ctrl+Shift+Z` 或 `Alt+Z` 重做。
- 点击工具组查看执行详情，`Ctrl+O` 打开最新执行，`Ctrl+G` 跳到最终回答。
- 空输入框输入 `!` 进入 Shell Mode，Tab 补全命令和路径。
- `/output` 查看完整本地命令输出，`/copy output` 复制输出。
- 文件编辑默认展开完整真实 Diff，点击编辑头行或按 `Ctrl+O` 折叠为一行 `+A/-R` 摘要。在 Diff 上拖选复制的是选中源码行（不含行号与 `+`/`-` 标记）；`/copy diff` 复制完整已应用补丁。
- `Ctrl+B` 将运行中的本地命令转入后台，`Ctrl+C` 停止该次命令。

点击底部后台任务入口可以查看任务、输出或单独停止任务。Esc 只返回列表；连接后台服务时，关闭终端也不会停止后台任务。

## 会话与后台运行

`leveler resume` 继续已有会话。macOS 和 Linux 还可以用 `leveler serve` 保持后台运行；Windows 暂不支持本地后台服务。

如果持有后台服务的进程异常退出（崩溃或被终止），`leveler tui` 仍可启动：受影响的任务会标记为 `unknown`，启动提示会说明其进程是否仍然存在。此时 CodeLeveler 不会终止或重新执行这些任务；用 `leveler background list` 查看遗留任务，用 `leveler background logs` 查看保留的输出，用 `leveler background journal list` 查看仍未解决的写入预留。

`leveler run "任务"` 执行任务直至结束；只需一轮回答时，添加 `--collaboration chat`。

需要多个候选实现时，可用 `leveler run "任务" --parallel 3`。这要求 Git 工作区干净且已有提交，会创建分支和 worktree、提交通过验证的候选，并集成回当前分支。

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

## 排查问题

先运行 `leveler doctor` 检查模型、终端和系统能力；`leveler trace` 查看会话记录。分享记录前请检查是否含有私密内容。

移动端仍是冻结的源码预览，没有正式安装包。远程会话流量经过 relay，目前没有端到端加密；评估时应自行部署 relay。
