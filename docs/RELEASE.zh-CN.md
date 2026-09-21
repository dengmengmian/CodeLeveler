# CodeLeveler 1.0.3

英文版：[`RELEASE.md`](RELEASE.md)

1.0.2 之上的 TUI 易用性更新。已安装的稳定版会自动更新，也可以用 `leveler update` 或 `/update`。

## 变更

- 成功的 `wait_task` 和 `get_task` 轮询不再反复占用 Conversation 行；后台任务仍由命令行、底栏和详情页唯一呈现
- 后台命令状态改为“后台运行” / “running in background”，直接描述当前状态

## 修复

- 后台任务等待失败时仍会显示错误和任务详情，不会随成功的调度轮询一起隐藏
- 会话回放与实时会话使用相同的后台轮询显示规则，同时保留完整、持久化的工具调用历史
- 自更新现在兼容新旧两种 `--version` 输出格式，拒绝版本探测失败，并允许旧版本安装重新构建的 v1.0.3

## 已知限制

- 预编译包没有苹果或微软的官方签名。macOS 或 Windows 第一次运行时可能会拦截，需要你允许一次
- Windows 还不能按命令断开网络。需要断网隔离的命令会被拒绝，而不会在网络仍开放时继续跑
- Windows 没有本地 daemon 套接字。会话和 `resume` 仍然可用
- Windows 上受隔离的 `!command` 结束后才打印输出，不会边跑边刷

安装与使用见 [README](../README.zh-CN.md)。
更新见 [README · 更新](../README.zh-CN.md#更新)。
系统怎么分层见 [架构说明](ARCHITECTURE.zh-CN.md)。
