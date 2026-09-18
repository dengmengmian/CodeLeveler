# CodeLeveler 1.0.0

英文版：[`RELEASE.md`](RELEASE.md)

第一个稳定版。支持 macOS、Linux 和 Windows。

从 1.0.0 起，CodeLeveler 遵循 [语义化版本](https://semver.org/lang/zh-CN/)：`1.0.x` 是修复，`1.x.0` 是向后兼容的新功能，`2.0.0` 才是破坏性变更。已安装的版本会自动检查并安装更新的稳定版——启动时自动进行，也可以用 `leveler update` 或 `/update`。

## 这一版有什么

- 终端界面（`leveler tui`）、网页界面（`leveler web`）和命令行（`leveler run`）
- 会话保存在本机，之后可以继续
- `/develop` 工作流：分析 → 编码 → 验证 → 验收
- 自定义 Agent：一个目录，里面是 `agent.yaml` 和 `instructions.md`
- 移动端，以及宿主机上的远程桥（`leveler remote`）
- 写文件和执行命令需要批准
- 命令隔离：macOS 用 Seatbelt，Linux 用 bubblewrap，Windows 用 Low integrity
- 从 GitHub Release 自升级，安装前先校验 SHA-256

## 已知限制

- 预编译包没有苹果或微软的官方签名。macOS 或 Windows 第一次运行时可能会拦截，需要你允许一次
- Windows 还不能按命令断开网络。需要断网隔离的命令会被拒绝，而不会在网络仍开放时继续跑
- Windows 没有本地 daemon 套接字。会话和 `resume` 仍然可用
- Windows 上受隔离的 `!command` 结束后才打印输出，不会边跑边刷

安装与使用见 [README](../README.zh-CN.md)。
更新见 [README · 更新](../README.zh-CN.md#更新)。
系统怎么分层见 [架构说明](ARCHITECTURE.zh-CN.md)。
