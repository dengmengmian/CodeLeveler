# 更新日志

英文版：[`CHANGELOG.md`](CHANGELOG.md)

发布说明：[`docs/RELEASE.zh-CN.md`](docs/RELEASE.zh-CN.md)。

## [1.0.0] - 2026-09-18

第一个稳定版。从这版起 CodeLeveler 遵循语义化版本。

### 新增

- 自升级：启动时检查最新的**稳定版** GitHub Release，校验 SHA-256，替换当前二进制并重启。失败不会阻止启动
- `leveler update`（别名 `upgrade`）手动检查与安装，支持 `--check`、`--force`、`--version <tag>`
- TUI 中的 `/update`，任务运行期间会被拒绝
- `~/.leveler/config.toml` 新增 `[update]`：`auto_update`、`check_interval_hours`

### 变更

- 发布产物固定为 `leveler-v<version>-<target>.tar.gz|zip` 及其 `.sha256`；发布工作流拒绝与工作区版本不一致的 tag

## [0.1.0-beta.1] - 2026-09-17

面向 macOS、Linux 和 Windows 的公开 beta。

### 这一版有什么

- 终端界面（`leveler tui`）、网页界面（`leveler web`）和命令行（`leveler run`）
- 会话保存在本机，之后可以继续
- 自定义 Agent：一个目录，里面是 `agent.yaml` 和 `instructions.md`
- 移动端，以及宿主机上的远程桥（`leveler remote`）
- 写文件和执行命令需要批准
- 命令隔离：macOS 用 Seatbelt，Linux 用 bubblewrap，Windows 用 Low integrity

### 已知限制

- 预编译包没有苹果或微软的官方签名。macOS 或 Windows 第一次运行时可能会拦截，需要你允许一次
- 1.0 之前，命令和配置还可能变。`run`、`resume`、`tui` 打算保持稳定；`eval` 和 `remote` 不一定
- Windows 还不能按命令断开网络。需要断网隔离的命令会被拒绝，而不会在网络仍开放时继续跑
- Windows 没有本地 daemon 套接字。会话和 `resume` 仍然可用
- Windows 上受隔离的 `!command` 结束后才打印输出，不会边跑边刷

安装与使用见 [`README.zh-CN.md`](README.zh-CN.md)。
系统怎么分层见 [`docs/ARCHITECTURE.zh-CN.md`](docs/ARCHITECTURE.zh-CN.md)。
