<p align="center">
  <img src="assets/brand/codeleveler-app-icon.svg" width="88" alt="CodeLeveler 标志">
</p>

<h1 align="center">CodeLeveler</h1>

<p align="center">在本地仓库里工作的编程智能体</p>
<p align="center"><a href="README.md">English</a> · <a href="LICENSE-APACHE">Apache 2.0</a></p>

CodeLeveler 在你的本地工作区读取代码、编辑文件并运行测试。你可以在终端或浏览器里查看工具输出和改动，稍后重新打开会话，也可以通过 CLI 直接执行任务。模型可选 BigModel、DeepSeek、Moonshot/Kimi、OpenAI、Anthropic 或其他 OpenAI 兼容服务。

## 安装

macOS（Apple Silicon 或 Intel）与 Linux x86_64 可以用安装脚本。脚本会校验下载包的 SHA-256。

```sh
curl -fsSL https://raw.githubusercontent.com/dengmengmian/CodeLeveler/main/install.sh | sh
```

默认安装到 `~/.local/bin`，请确保该目录在 `PATH` 中。

也可以使用 Homebrew（macOS、Linux x86_64）：

```sh
brew install dengmengmian/tap/leveler
```

Windows x86_64 请从[发布页](https://github.com/dengmengmian/CodeLeveler/releases)下载对应压缩包，校验附带的 SHA-256，解压后将目录加入 `PATH`。保持 `leveler.exe` 与 `leveler-confine.exe` 在同一目录。

Linux 使用受限命令前需安装 `bubblewrap`，例如 `sudo apt install bubblewrap`。

从源码构建请看[贡献指南](CONTRIBUTING.zh-CN.md#本地构建)。

## 快速开始

安装后，配置模型服务商，再从项目目录打开终端界面。

```sh
leveler login
cd /path/to/your/project
leveler
```

试着输入「梳理这个项目的测试入口，然后运行最小的一组相关测试」。用 `/help` 查看界面命令，遇到配置或环境问题时运行 `leveler doctor`。

| 任务 | 入口 |
| --- | --- |
| 查看项目并交互修改 | `leveler` |
| 在浏览器打开界面 | `leveler web` |
| 从脚本或命令行直接执行任务 | `leveler run "找出失败的测试并修复"` |
| 打开已有会话 | `leveler resume` |
| 在终端界面运行带验收条件的目标 | `/goal <任务>` |

## 权限与隐私

默认 Auto 模式允许日常开发操作，危险操作需要批准。Restricted 采用更严格的资源与授权边界。Full 在 CodeLeveler 层开放文件和网络访问。

会话记录保存在本机。模型请求可能包含工作区内容，并发送给你配置的服务商。查看[安全策略](SECURITY.md)了解权限、数据流向及平台限制。

## 更新

Homebrew 安装用 `brew upgrade leveler`。其他安装方式可运行 `leveler update` 获取稳定版。

## 文档、帮助与贡献

- [文档索引](docs/README.zh-CN.md) · [日常使用与排查](docs/USAGE.zh-CN.md) · [版本历史](CHANGELOG.zh-CN.md)
- [贡献指南](CONTRIBUTING.zh-CN.md) · [开发代理规则](AGENTS.md)
- [提交问题或建议](https://github.com/dengmengmian/CodeLeveler/issues/new/choose) · [私下报告安全漏洞](SECURITY.md#reporting-a-vulnerability)

## 许可证

[Apache License 2.0](LICENSE-APACHE)。
