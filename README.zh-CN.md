<p align="center">
  <img src="assets/brand/codeleveler-app-icon.svg" width="88" alt="CodeLeveler 标志">
</p>

<h1 align="center">CodeLeveler</h1>

<p align="center">在本地仓库里工作的编程智能体</p>
<p align="center"><a href="README.md">English</a> · <a href="LICENSE-APACHE">Apache 2.0</a></p>

给 CodeLeveler 一个任务，让它读代码、修改文件、运行测试。你可以随时查看改动，关闭后也能继续会话。

支持终端、浏览器和命令行，以及 BigModel、DeepSeek、Moonshot/Kimi、OpenAI、Anthropic 和其他 OpenAI 兼容服务。会话保存在本机，模型请求发送给你配置的服务商。

## 安装

macOS、Linux：

```sh
curl -fsSL https://raw.githubusercontent.com/dengmengmian/CodeLeveler/main/install.sh | sh
```

默认安装到 `~/.local/bin`，请确保该目录在 `PATH` 中。

也可以使用 Homebrew（macOS、Linux x86_64）：

```sh
brew install dengmengmian/tap/leveler
```

Windows 请从[发布页](https://github.com/dengmengmian/CodeLeveler/releases)下载对应压缩包，校验附带的 SHA-256，解压后将目录加入 `PATH`。保持 `leveler.exe` 与 `leveler-confine.exe` 在同一目录。

Linux 使用受限命令前需安装 `bubblewrap`，例如 `sudo apt install bubblewrap`。

## 开始使用

```sh
leveler login              # 选择服务商并配置 API Key
cd your-project
leveler
```

建议从干净的 Git 工作区开始，方便检查改动。遇到配置或环境问题时运行 `leveler doctor`。

| 想做什么 | 命令 |
| --- | --- |
| 打开终端界面 | `leveler` |
| 打开浏览器界面 | `leveler web` |
| 直接执行任务 | `leveler run "找出失败的测试并修好"` |
| 继续之前的会话 | `leveler resume` |
| 查看帮助 | `leveler --help` |

`leveler run --parallel 2` 保留指定的权限模式、网络限制和模型步数上限，支持 `--output jsonl`。并行任务会在启动前拒绝 shipping 参数（`--commit`、`--push`、`--pr`、`--branch`、`--pr-base`）；请审查合并结果后单独提交或推送。单任务的 shipping 支持两种输出格式，失败时返回非零退出码。 候选初始化、执行或提交出错时，会在集成前保留原始原因并返回失败，同时记录候选结束并清理本次 worktree。 并行候选继承父任务的状态 home。清理失败会以警告报告残留路径并持久化，不撤销已集成结果，也不覆盖原始任务错误。

终端里可以使用 `/develop <目标>` 完成分析、编码、验证和评审。`/help` 查看命令与快捷键，`/thinking` 调整模型思考强度。

## 权限与隐私

默认 Auto 模式允许普通开发操作，危险操作需要批准。需要更严格的审批时选择 Restricted；Full 允许全部操作，请仅在信任的环境中使用。

模型可能读到工作区内容，并将相关内容发送给服务商。不要把密钥放在智能体可访问的工作区。权限与平台限制见[安全说明](SECURITY.md)。

## 更新

Homebrew 安装：

```sh
brew upgrade leveler
```

其他安装方式默认自动检查稳定版，也可以手动更新：

```sh
leveler update
```

## 使用文档

- [日常使用](docs/USAGE.zh-CN.md)
- [自定义 Agent](docs/AGENT_EXTENSIBILITY.zh-CN.md)
- [持久记忆](docs/MEMORY.zh-CN.md)
- [本版更新](docs/RELEASE.zh-CN.md)
- [历史版本](CHANGELOG.zh-CN.md)

安全漏洞请按[安全策略](SECURITY.md)私下报告。
