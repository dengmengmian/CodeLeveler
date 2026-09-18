# CodeLeveler 1.0.1

英文版：[`RELEASE.md`](RELEASE.md)

1.0.0 之上的修复版。已安装的 1.0.0 会自动更新，也可以用 `leveler update` 或 `/update`。

## 修复

- 旧的本地 runtime 还在忙时启动 TUI，不再等 10 秒后失败：客户端会等旧 runtime 做完，再交接给新的
- 另一个客户端已经拉起替代 runtime 时，交接不再卡住
- 开发构建能认出自己启动的 runtime，不会每次启动都重启一个相同的 runtime
- 没有客户端连接、也没有任务在跑时，空闲的后台 runtime 会自行退出
- TUI 里每一行可见的工具记录都写明执行了什么
- 任务运行中输入的消息，会在 runtime 就绪后按顺序自动作为下一轮发送，且只发往写它时所在的会话；排队等待不再显示为"状态未知"
- 提问、选择器和批准里的选项带编号，移动光标或从 9 到 10 时标签不再错位

## 已知限制

- 预编译包没有苹果或微软的官方签名。macOS 或 Windows 第一次运行时可能会拦截，需要你允许一次
- Windows 还不能按命令断开网络。需要断网隔离的命令会被拒绝，而不会在网络仍开放时继续跑
- Windows 没有本地 daemon 套接字。会话和 `resume` 仍然可用
- Windows 上受隔离的 `!command` 结束后才打印输出，不会边跑边刷

安装与使用见 [README](../README.zh-CN.md)。
更新见 [README · 更新](../README.zh-CN.md#更新)。
系统怎么分层见 [架构说明](ARCHITECTURE.zh-CN.md)。
