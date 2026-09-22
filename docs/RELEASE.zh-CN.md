# CodeLeveler 1.0.5

英文版：[`RELEASE.md`](RELEASE.md)

1.0.4 之上的首次启动与空闲体验更新。已安装的稳定版会自动更新，也可以用 `leveler update` 或 `/update`。

## 新增

- 没有配置时直接运行 `leveler` 会进入与 `leveler login` 相同的首次设置引导，第一步是显式选择语言（English / 中文），选择结果会以 `lang` 写入配置
- 全新安装默认开启启动自动更新；用包管理器安装的用户可以通过 `[update] auto_update = false` 关闭
- `leveler login` 和 `leveler init` 写出的配置会为每个设置附带中英双语注释
- TUI 会在每轮结束后预测用户下一句话，作为临时提示显示；打开空会话时则依据仓库上下文生成一条起步建议。Tab 采纳，Esc 忽略，它不会被保存，也不会自行提交
- 对话有一定内容后，空闲三分钟会在 TUI 中显示一条弱化的回顾，说明工作进展到哪一步、接下来做什么
- 客户端协议新增 `RequestPromptSuggestion` / `RequestAwaySummary` 命令和 `PromptSuggestion` / `AwaySummary` 事件（协议 minor 1.12）；远程控制面会拒绝这两个命令

## 变更

- 持久记忆遵循 Claude Code 式的边界：提取时会把每条候选归类为 user、feedback、project、reference、derived 或 task_state，仓库里已经能查到的事实和短期任务状态会被拒绝保存
- 记忆确认改为每批一句话，不再罗列候选 id；Conversation 中的记忆变更提示也改成自然语句
- 全部完成的计划清单在回合结束后不再作为对话历史保留

## 修复

- 用户已经开始输入、或新一轮已经开始后，迟到的模型提示和回顾不会再出现
- 空闲回顾只触发一次：已经消费的截止时间不会重复触发，任务运行中或已输入内容时也会被拒绝

## 已知限制

- 预编译包没有苹果或微软的官方签名。macOS 或 Windows 第一次运行时可能会拦截，需要你允许一次
- Windows 还不能按命令断开网络。需要断网隔离的命令会被拒绝，而不会在网络仍开放时继续跑
- Windows 没有本地 daemon 套接字。会话和 `resume` 仍然可用
- Windows 上受隔离的 `!command` 结束后才打印输出，不会边跑边刷

安装与使用见 [README](../README.zh-CN.md)。
更新见 [README · 更新](../README.zh-CN.md#更新)。
系统怎么分层见 [架构说明](ARCHITECTURE.zh-CN.md)。
