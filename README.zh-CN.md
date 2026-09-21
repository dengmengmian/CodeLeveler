<p align="center">
  <img src="assets/brand/codeleveler-app-icon.svg" width="88" alt="CodeLeveler 标志">
</p>

<h1 align="center">CodeLeveler</h1>

<p align="center">
  <strong>一个在本地仓库里工作的编程智能体；需要改代码时，留下可审查的改动。</strong>
</p>

<p align="center">
  <a href="README.md">English</a> ·
  <a href="LICENSE-APACHE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="Apache 2.0 License"></a>
</p>

给 CodeLeveler 一个任务。它可以读仓库、改文件、运行构建和测试，也可以使用 Git。会话停止后，对话、审批、事件记录和改过的文件仍然保留。会话状态存储在你的机器上，模型请求只发给你配置的服务商。

终端界面（`leveler`）、浏览器界面（`leveler web`）和无界面命令行（`leveler run`）使用同一套运行时。CodeLeveler 当前面向 macOS、Linux 和 Windows。

```sh
cd your-project
leveler
# 或者
leveler run "找出失败的测试并修好"
```

## 它能做什么

- 需要修改代码时，在仓库里产生可检查的改动，而不是把智能体的一句“已完成”当成证据。
- 持久化会话；关闭界面后，可以用 `leveler resume` 继续。
- 支持智谱 BigModel（GLM Coding Plan）、DeepSeek、Moonshot/Kimi、OpenAI、Anthropic，以及其他 OpenAI 兼容接口。
- 提供显式的 `/develop` 工作流：分析 → 编码 → 验证 → 评审。
- 可选支持浏览器自动化、网页搜索、自定义 Agent 和并行候选 worktree。

## 安装

### 安装脚本（macOS、Linux）

```sh
curl -fsSL https://raw.githubusercontent.com/dengmengmian/CodeLeveler/main/install.sh | sh
```

脚本会从最新稳定版中挑选当前平台的压缩包，SHA-256 校验不通过就拒绝安装，并把 `leveler` 放到 `~/.local/bin`（可用 `LEVELER_BIN_DIR` 改目录）。`LEVELER_VERSION=v1.0.0` 可固定版本。

### Homebrew（macOS、Linux x86_64）

```sh
brew install dengmengmian/tap/leveler
```

更新用 `brew upgrade leveler`。内置更新器不识别 Homebrew，会绕过 Homebrew 直接替换二进制，所以通过 Homebrew 安装时请在 `[update]` 里设置 `auto_update = false`（见[更新](#更新)）。

### 发布包

每个 release 都会为支持的平台发布带 SHA-256 校验的压缩包：

| 平台 | 压缩包 |
| --- | --- |
| macOS（Apple Silicon） | `leveler-v<version>-aarch64-apple-darwin.tar.gz` |
| macOS（Intel） | `leveler-v<version>-x86_64-apple-darwin.tar.gz` |
| Linux（x86_64） | `leveler-v<version>-x86_64-unknown-linux-gnu.tar.gz` |
| Windows（x86_64） | `leveler-v<version>-x86_64-pc-windows-msvc.zip` |

从[发布页](https://github.com/dengmengmian/CodeLeveler/releases)下载对应平台的压缩包和它的 `.sha256` 文件，校验后解压 `leveler` 并放入 `PATH`：

```sh
shasum -a 256 -c leveler-v1.0.0-aarch64-apple-darwin.tar.gz.sha256
tar -xzf leveler-v1.0.0-aarch64-apple-darwin.tar.gz
mkdir -p ~/.local/bin && mv leveler-v1.0.0-aarch64-apple-darwin/leveler ~/.local/bin/
leveler --version
```

Windows 请解压 `.zip`，并保持 `leveler.exe` 与 `leveler-confine.exe` 在同一目录。

### 从源码

安装 Rust 1.90+ 和 Git 后：

```sh
cargo install --path crates/leveler-cli --locked
leveler --version
```

Linux 在运行智能体命令前需要安装 `bubblewrap`：

```sh
sudo apt install bubblewrap
```

不安装时 CodeLeveler 可以启动，但需要 Linux 隔离的命令会直接失败，不会降级成无沙箱执行。运行 `leveler doctor` 可以查看当前机器实际具备的能力。

## 更新

CodeLeveler 会自动保持在最新的**稳定版** GitHub Release。启动时最多每 `check_interval_hours` 检查一次；发现新版本后会下载、校验 SHA-256、替换当前二进制并重启。检查失败不会阻止启动：当前版本正常运行，失败原因写入日志。

手动更新：

```sh
leveler update            # 安装最新稳定版
leveler update --check    # 有更新时退出码为 2
leveler update --version v1.0.4
```

在 TUI 中：

```
/update
```

任务运行期间 `/update` 会被拒绝，不会在有任务执行时替换二进制。

配置（`~/.leveler/config.toml`）：

```toml
[update]
auto_update = true           # false 关闭启动自动检查；手动更新仍可用
check_interval_hours = 1     # 成功检查之间的间隔小时数（最小 1）
```

只追踪稳定版。预发布版本只在显式指定 `--version` 时安装。

## 第一次使用

```sh
leveler login
leveler doctor
cd your-project
leveler
```

`leveler login` 支持智谱 BigModel（GLM Coding Plan）、DeepSeek、Moonshot/Kimi、OpenAI 和 Anthropic。服务商支持模型发现时，它会列出该 key 可见的模型。配置写入 `~/.leveler/config.toml`；在 Unix 上，该文件会被限制为 `0600`。可以用 `leveler login bigmodel`、`leveler login deepseek` 或 `leveler login moonshot` 跳过菜单。

DeepSeek Flash 的模型引用是 `deepseek/deepseek-flash`。已有配置中的预发布名称 `deepseek/deepseek-v4-flash` 需要替换为该名称。

接入其他 OpenAI 兼容接口时，先运行 `leveler init`，再修改 `~/.leveler/config.toml`。[configs/example.yaml](configs/example.yaml) 只是带注释的配置结构参考；这个 YAML 文件本身不会被加载。

第一次使用建议从干净的 Git worktree 开始，改动更容易检查或丢弃。

## 命令

| 目标 | 命令 |
| --- | --- |
| 打开终端界面 | `leveler` 或 `leveler tui` |
| 运行完整开发工作流 | 在 TUI 中输入 `/develop <目标>` |
| 打开浏览器界面 | `leveler web` |
| 无界面运行一个任务 | `leveler run "…"` |
| 一直运行到目标得到终态 | `leveler run "…" --collaboration goal` |
| 在隔离 worktree 中运行并行候选 | `leveler run "…" --parallel 3` |
| 在 TUI 中继续会话 | `leveler resume [session-id]` |
| 查看会话事件记录 | `leveler trace [session-id]` |

`/develop` 在同一个会话中执行分析 → 编码 → 验证 → 评审。未配置 `[develop].model` 时，读取代码的阶段沿用当前会话模型。显式配置时必须写成 `provider/model`；无效值会报错，不会静默换成其他模型。

`--parallel` 是独立的候选实现流程，不等同于运行时子 Agent 委派。它要求 Git 工作区干净且已有提交，会创建隔离 worktree 和分支，为通过验证的候选创建提交，并把成功候选集成回当前分支。

macOS 和 Linux 可以运行 `leveler serve`，通过本机 Unix 套接字让运行时在界面关闭后继续工作。Windows 没有本机 Unix 套接字 daemon，但持久化会话和 `resume` 仍然可用。

## 权限与隔离

默认权限是 `assisted`。普通的仓库内写入、构建、测试、网络操作，以及 `git push`、发布包等命令，可能会在操作系统沙箱内自动执行。不可逆删除、提权和逃出主机边界的操作需要审批。如果希望审批边界更严格，使用 `request-approval`。

| 平台 | 受限命令的隔离方式 |
| --- | --- |
| macOS | Seatbelt |
| Linux | bubblewrap；使用受限命令前必须安装 |
| Windows | Low integrity。需要禁止网络的命令会被拒绝，因为当前还不能按命令隔离网络。 |

`leveler doctor` 会报告当前主机的实际能力。受限模式缺少所需隔离后端时，执行会失败，不会假装已经沙箱化。

## 浏览器和网页搜索

这些都是可选工具。只有当前工作配置允许、并且本机能够提供时，才会出现在模型的工具列表中。

- **Chrome、Edge 或 Chromium：** CodeLeveler 会启动独立的自动化会话。`browser_tab`、`browser_act` 和 `browser_inspect` 可以导航、交互，并读取 console、页面错误和网络记录。
- **Safari：** 在 `~/.leveler/config.toml` 的 `[browser]` 下配置 `default = "safari"`，并打开 Safari Remote Automation。Safari 支持标签页和交互，但 WebDriver 后端不能提供 console、页面错误或网络检查。
- **网页搜索：** 把 Tavily key 放入环境变量 `LEVELER_SEARCH_API_KEY`。`web_search` 只发起一次 HTTP 请求，十秒超时，不重试，也不会改走浏览器自动化。

TUI 的 `/web` 和界面中打开的链接仍然使用操作系统默认浏览器。

## 自定义 Agent

自定义 Agent 是一个包含 `agent.yaml`（声明能力）和 `instructions.md`（工作说明）的目录。可以让 CodeLeveler 创建，也可以在 Web UI 的 设置 → Agents 中创建，或者把 `.leveler/agents/<name>/` 提交进 Git。

Coding Harness 解析定义，能力准入和 Host Authority 强制执行边界。instructions 不能额外授予工具或写权限。详见[自定义 Agent](docs/AGENT_EXTENSIBILITY.zh-CN.md)。

## 实验性移动端

`apps/leveler-mobile` 是已经冻结的源码预览，不是已分发产品。目前的验证证据只覆盖 iOS 模拟器配对流程；Android 构建、真机与蜂窝网络测试、TestFlight 和 Play 分发都尚未完成。

远程控制会让会话流量经过配置的 relay。消息有签名，但端到端 AEAD 加密尚未实现，TLS 终止方可以读取会话流量。评估这一功能时应自托管 relay。使用前请阅读[移动端状态](apps/leveler-mobile/README.md)。

## 文档

- [架构说明](docs/ARCHITECTURE.zh-CN.md)
- [自定义 Agent](docs/AGENT_EXTENSIBILITY.zh-CN.md)
- [发布说明](docs/RELEASE.zh-CN.md)
- [更新日志](CHANGELOG.zh-CN.md)
- [文档目录](docs/README.zh-CN.md)
- [安全策略](SECURITY.md)

运行 `leveler --help` 查看完整命令列表。保持最新版本见[更新](#更新)。

安全漏洞请使用 [SECURITY.md](SECURITY.md) 中的非公开流程，不要提交公开 issue。

项目采用 Apache License 2.0，见 [LICENSE-APACHE](LICENSE-APACHE)。
