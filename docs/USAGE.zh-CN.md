# 日常使用

[English](USAGE.md) · [返回首页](../README.zh-CN.md)

## 配置模型

运行 `leveler login` 选择服务商并配置模型。配置保存在 `~/.leveler/config.toml`；接入其他 OpenAI 兼容服务时，先运行 `leveler init` 再编辑该文件。

在终端里用 `/thinking high` 调整当前会话的思考强度，`/thinking reset` 恢复配置默认值。可选值为 `auto`、`off`、`minimal`、`low`、`medium`、`high`、`max`，具体支持情况见 `leveler doctor`。默认是 `high`，`max` 使用当前模型支持的最高档，`auto` 使用服务商默认值。

## 输入与查看结果

- `Ctrl+R` 搜索输入历史，Enter 放回草稿，不会直接发送。
- `Ctrl+Z` 撤销输入，`Ctrl+Shift+Z` 或 `Alt+Z` 重做。
- 点击工具组查看执行详情，`Ctrl+O` 打开最新执行，`Ctrl+G` 跳到最终回答。
- 空输入框输入 `!` 进入 Shell Mode，Tab 补全命令和路径。
- `/output` 查看完整本地命令输出，`/copy output` 复制输出。
- `Ctrl+B` 将运行中的本地命令转入后台，`Ctrl+C` 停止该次命令。

点击底部后台任务入口可以查看任务、输出或单独停止任务。Esc 只返回列表；连接后台服务时，关闭终端也不会停止后台任务。

## 会话与后台运行

`leveler resume` 继续已有会话。macOS 和 Linux 还可以用 `leveler serve` 保持后台运行；Windows 暂不支持本地后台服务。

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
