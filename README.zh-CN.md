<p align="center">
  <img src="assets/brand/codeleveler-app-icon.svg" width="88" alt="CodeLeveler 标志">
</p>

<h1 align="center">CodeLeveler</h1>

<p align="center">
  <strong>从编程需求到可审查的 diff，在一个终端工作流里完成。</strong>
</p>

<p align="center">
  <a href="README.md">English</a> ·
  <a href="https://github.com/dengmengmian/CodeLeveler/actions/workflows/ci.yml"><img src="https://github.com/dengmengmian/CodeLeveler/actions/workflows/ci.yml/badge.svg" alt="CI"></a> ·
  <a href="LICENSE-APACHE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="Apache 2.0 License"></a>
</p>

CodeLeveler 是一个终端编程代理，可以理解、修改、运行并验证真实项目。你既可以
在 TUI 里交互，也可以通过 CLI 自动执行单个任务。会话、权限和项目状态保存在本机；
模型请求只发送给你配置的 provider。

Windows、macOS 和 Linux 均纳入 CI。CodeLeveler 目前处于 public beta
（`0.1.x`）。

## 三个专注的工具，一套工作流

**CodeLeveler 负责写代码，ReviewGate 负责代码 Review，AgentGate 负责连接和
适配模型 API。** 三个工具都可以独立使用，也可以配合工作：

| 工具 | 专注于 |
| --- | --- |
| **CodeLeveler** | 在终端中理解、修改、运行并验证代码 |
| [AgentGate](https://github.com/dengmengmian/agentgate-ai) | 通过一个本地网关适配不同模型 API |
| [ReviewGate](https://github.com/dengmengmian/ReviewGate) | 审查代码改动并筛出高置信问题 |

## 为什么选择 CodeLeveler

- **完成整个编程闭环。** 理解仓库、进行聚焦修改、运行项目检查、修复失败，最后
  留下可以审查的 diff。
- **控制权在用户手里。** 类型化工具、审批规则、工作区边界、检查点和平台级命令
  隔离共同约束代理能做什么。
- **随时恢复已保存的工作。** SQLite 会话保存对话、待审批操作、工具结果、diff 和
  验证状态，之后仍可继续执行或审查。
- **模型由你选择。** 支持可配置的 OpenAI 兼容 provider，agent runtime 不绑定
  单一模型厂商。

## 快速开始

### 1. 安装

**方式 A — Homebrew（macOS，推荐）**

```sh
brew install dengmengmian/tap/leveler
```

之后用 `brew upgrade leveler` 升级。

**方式 B — 下载预编译二进制**

从[最新发布页](https://github.com/dengmengmian/CodeLeveler/releases/latest)下载对应
平台的压缩包，解压后把 `leveler` 放到 `PATH`。示例（按平台替换 `V`/`T`）：

```sh
V=0.1.0; T=aarch64-apple-darwin   # 或 x86_64-apple-darwin、x86_64-unknown-linux-gnu
curl -LO https://github.com/dengmengmian/CodeLeveler/releases/download/v$V/leveler-v$V-$T.tar.gz
tar -xzf "leveler-v$V-$T.tar.gz"
sudo mv "leveler-v$V-$T/leveler" /usr/local/bin/
leveler --version
```

Windows 下载 `leveler-v<版本>-x86_64-pc-windows-msvc.zip`，解压后把目录加入
`PATH`。任意平台安装后，用 `leveler upgrade` 升级到新版本。

**方式 C — 从源码编译**

需要 [Rust 1.90+](https://www.rust-lang.org/tools/install) 和 Git。

```sh
git clone https://github.com/dengmengmian/CodeLeveler.git
cd codeleveler
cargo install --path crates/leveler-cli --locked
```

### 2. 配置模型

Windows 先在 PowerShell 中设置持久的 Leveler 主目录，并新建配置文件：

```powershell
$levelerHome = Join-Path $HOME ".leveler"
[Environment]::SetEnvironmentVariable("LEVELER_HOME", $levelerHome, "User")
$env:LEVELER_HOME = $levelerHome
New-Item -ItemType Directory -Force $levelerHome
notepad (Join-Path $levelerHome "config.toml")
```

macOS/Linux 新建 `~/.leveler/config.toml`。在文件中写入：

```toml
default_model = "deepseek/deepseek-chat"

[providers.deepseek]
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"

[models."deepseek-chat"]
provider = "deepseek"
context_window = 131072
max_output_tokens = 8192
streaming = true
tool_calling = true
structured_output = true
```

为当前 shell 设置 API key：

```powershell
# PowerShell
$env:DEEPSEEK_API_KEY = "..."
```

```sh
# macOS / Linux
export DEEPSEEK_API_KEY="..."
```

本地配置也支持明文 `api_key = "..."`。在共享机器上，或配置文件会进入 Git 时，
更建议使用环境变量。

### 3. 检查并启动

```sh
leveler doctor
leveler model probe deepseek/deepseek-chat
cd path/to/your/project
leveler
```

也可以不打开 TUI，直接运行单个任务：

```sh
leveler run "找出测试失败的原因并修复"
```

默认 `assisted` 权限模式会在高风险操作前请求确认。第一次使用建议从干净的 Git
worktree 开始，确保所有修改都能轻松审查或丢弃。

## 一套工作流，多种使用方式

| 需要 | 命令 |
| --- | --- |
| 在 TUI 中交互 | `leveler` |
| 执行单个任务 | `leveler run "给下单接口加校验"` |
| 汇总多个视角 | `leveler discuss "这个测试为什么不稳定？"` |
| 只读分析并生成计划 | `leveler plan "替换缓存实现"` |
| 恢复之前的工作 | `leveler resume <session-id>` |
| 编排较大的任务 | `leveler run "修复失败的测试" --orchestrate` |

在 macOS/Linux 上，长时间交互可以在一个终端运行 `leveler serve`，在另一个终端
运行 `leveler`。TUI 会重连仓库对应的本地 runtime，工作不再依赖某一个终端进程。
Windows 支持持久化会话和 `resume`，但目前还不支持这种 daemon transport。

## 一次任务会经历什么

1. **理解** — 搜索仓库、检查符号和相关文件，复杂任务会先建立计划。
2. **修改** — 在当前权限和工作区边界内，通过类型化文件操作和命令完成改动。
3. **验证** — 自动发现或使用配置的 format、build、test 命令；失败时可以进行有界
   修复。
4. **交付** — 保留 diff、对话、验证结果和会话状态，供用户审查或继续执行。

## 安全与平台支持

CodeLeveler 可以修改文件和执行本地命令，因此安全边界会明确展示，而不是隐含处理。

| 平台 | 进程控制 | 受限命令执行 |
| --- | --- | --- |
| Windows | Job Objects | 能力可用时使用 AppContainer 和 ACL 限制 |
| macOS | 进程组取消 | Seatbelt profile |
| Linux | 进程组取消 | Bubblewrap |

`leveler doctor` 会报告本机实际可用的能力。受限模式缺少必要隔离后端时会
fail-closed；只有进程树控制时不会声称拥有完整沙箱。

权限规则和 hooks 都可以按用户或按仓库配置。可以从[配置指南](docs/README.zh-CN.md)、
[权限示例](docs/permissions.example.yaml)和 [hook 示例](docs/hooks.example.yaml)开始。

## 配置与文档

- [中文文档索引](docs/README.zh-CN.md)
- [项目配置示例](docs/leveler-config-example.yaml)
- [Provider 与模型配置 schema](configs/example.yaml)
- [架构说明](docs/ARCHITECTURE.zh-CN.md)
- [评测工具](evals/README.md)

运行 `leveler --help` 或 `leveler <command> --help` 查看完整命令。使用
`leveler upgrade --check` 检查新版本。

## Public beta

1.0 前命令和配置格式仍可能变化。跨平台 CI 覆盖 Windows、macOS 和 Linux，但
操作系统级隔离仍取决于每台机器实际安装并启用的能力。

## 贡献与安全

欢迎贡献。提交 PR 前请阅读 [CONTRIBUTING.md](CONTRIBUTING.md)。安全漏洞请按照
[SECURITY.md](SECURITY.md) 的私密流程报告，不要发布公开 issue。

## 许可证

Apache License 2.0。见 [LICENSE-APACHE](LICENSE-APACHE)。
