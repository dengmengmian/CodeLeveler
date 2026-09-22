# CodeLeveler 1.0.6

英文版：[`RELEASE.md`](RELEASE.md)

1.0.5 之上的 WebUI 打包与 Apple Terminal 兼容性修复。已安装的稳定版可以
自动更新，也可以执行 `leveler update` 或 `/update`。

## 修复

- Release 现在会先构建 WebUI、再编译 Rust 二进制；`leveler web` 会提供已
  嵌入的前端，不再返回“WebUI assets are not built”
- 发布契约会在缺少前端安装/构建步骤，或前端晚于 Rust 编译时失败
- 源码安装说明补齐了 `cargo install` 前所需的 Node.js 前端构建步骤
- macOS 26 之前的 Apple Terminal 现在使用最接近的 xterm-256 色板，避免
  RGB 颜色显示异常，同时保留深色/浅色主题极性与对比度计算
- 活动目标标题的详情入口现在会与终端右边缘保留一列间距

## 已知限制

- 预编译包没有苹果或微软的官方签名。macOS 或 Windows 第一次运行时可能
  会拦截，需要你允许一次
- Windows 还不能按命令断开网络。需要断网隔离的命令会被拒绝，而不会在
  网络仍开放时继续跑
- Windows 没有本地 daemon 套接字。会话和 `resume` 仍然可用
- Windows 上受隔离的 `!command` 结束后才打印输出，不会边跑边刷

安装与使用见 [README](../README.zh-CN.md)。
更新见 [README · 更新](../README.zh-CN.md#更新)。
系统怎么分层见 [架构说明](ARCHITECTURE.zh-CN.md)。
