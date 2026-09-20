# CodeLeveler 1.0.2

英文版：[`RELEASE.md`](RELEASE.md)

1.0.1 之上的功能与兼容性更新。已安装的稳定版会自动更新，也可以用 `leveler update` 或 `/update`。

## 新增

- 原生支持智谱 BigModel Coding Plan 的 `glm-5.3` 和 `glm-5.3-flash`，包含推理、视觉和 1M 上下文模型配置
- 支持当前 DeepSeek 模型 `deepseek-flash` 和 `deepseek-v4-pro`，并按模型声明视觉与并行工具能力
- 统一的 Skills 注册表、检查命令和受保护的 Skill 创建流程
- TUI 后台活动、详情页和计划视图
- 持久化语义记忆提取，支持受限推理、异步批处理和生命周期恢复

## 变更

- 会话续接、取消和恢复在 CLI、TUI、主机与远程控制之间共用一套明确的 runtime 协议
- `leveler login` 和默认 `leveler init` 会写入完整的内置模型能力，不再使用通用占位配置
- 退役的 `deepseek-v4-flash` 配置由 `deepseek-flash` 替代

## 修复

- DeepSeek 思考模式在携带工具时会为全部历史 assistant 消息回传 `reasoning_content`，避免多轮工具请求被拒绝
- DeepSeek 强制工具选择会关闭思考模式，但不会静默丢弃调用方显式提供的 temperature
- 不再把 DeepSeek 动态峰谷价和缓存价格错误表示为固定美元价格

## 已知限制

- 预编译包没有苹果或微软的官方签名。macOS 或 Windows 第一次运行时可能会拦截，需要你允许一次
- Windows 还不能按命令断开网络。需要断网隔离的命令会被拒绝，而不会在网络仍开放时继续跑
- Windows 没有本地 daemon 套接字。会话和 `resume` 仍然可用
- Windows 上受隔离的 `!command` 结束后才打印输出，不会边跑边刷

安装与使用见 [README](../README.zh-CN.md)。
更新见 [README · 更新](../README.zh-CN.md#更新)。
系统怎么分层见 [架构说明](ARCHITECTURE.zh-CN.md)。
