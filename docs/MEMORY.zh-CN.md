# 持久记忆

[English](MEMORY.md) · [返回首页](../README.zh-CN.md)

CodeLeveler 可以记住你的偏好、项目约定和已确认的决定，在后续任务中按需使用。自动整理出的推断或冲突内容可能需要你确认。

## 查看和管理

在终端输入 `/memory`，可以搜索、查看、复制、固定或归档记录。`/memory status` 查看记忆开关、待确认记录和最近处理情况。

你也可以直接告诉 CodeLeveler「记住这个项目的测试命令」或「忘记这条约定」。提议写入不等于已经生效；需要确认的内容会留在待确认列表。

## 保存位置

| 范围 | 位置 | 用途 |
| --- | --- | --- |
| 全局 | `~/.leveler/memory/` | 多个项目共用的偏好 |
| 项目 | `.leveler/memory/` | 可以随 Git 分享的项目约定 |
| 本地 | `.leveler/local/memory/` | 当前项目的个人记录 |

记忆以 Markdown 保存。提交项目记忆前，请检查其中是否有个人信息或私密内容；本地记录也应通过 Git 忽略规则排除。

## 开关

在项目的 `.leveler/config.yaml` 中配置：

```yaml
memory:
  enabled: true
  user_enabled: true
  experience_enabled: false
```

`enabled` 是总开关；`user_enabled` 控制用户事实；实验性的经验记录由 `experience_enabled` 单独开启，默认关闭。关闭功能不会删除已有记录。

## 隐私与限制

相关记忆可能随请求发给你配置的模型服务商。不要保存密码、Token 或其他凭据。

记忆可能重复、过时或未被检索到。重要约束请在当前任务中明确说明，并定期检查已有记录。

遗忘不能清除 Git 历史、备份或服务商日志中的副本。曾经泄露的凭据仍需轮换。记忆存储目前只在 macOS 验证，不支持网络文件系统。
