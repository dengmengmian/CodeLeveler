# 多 Agent 委派

CodeLeveler 可以对**彼此独立**的调查或**互不重叠**的改动并行跑多个**子 agent**。
父 agent 保留对话并汇总子结果；子 agent 之间不互聊（星型拓扑）。

## 何时会跑

模型调用注入工具 `spawn_agent`。在**同一轮** assistant 回复里发出**多个**
`spawn_agent`，它们会**并发**执行。

产品侧会在这些情况**引导**模型使用委派：

- 你明确要求并行 / 多 agent（如「并行 review」「分头调查」）。
- 任务有可拆分的独立面（如架构 + 稳定性 + 工具审查）。
- 请求文案匹配时，主机注入一次性提示（`## Multi-agent delegation`）。

**不会**在没有模型 `spawn_agent` 调用的情况下静默 spawn。一行级小改应留在父 agent。

## 角色

| `role` | 行为 |
| --- | --- |
| `explorer` | 只读工具集（不能改工作区）。 |
| `worker` | 可写；必须提供独占的 `files`。 |
| `default` | 全工具（未指定时）。 |

并行 worker 的 `files` 必须**互斥**，避免改同一路径。

## 硬限制

| 限制 | 默认 |
| --- | --- |
| 嵌套深度 | **1**（子 agent 不能再 spawn） |
| 并发子 agent | **4** |
| 单次 top-level 总 spawn 次数 | **6** |
| 单个子 agent 最长墙钟 | **15 分钟**（同时受父预算剩余约束） |

## 如何强制触发

用明确话术引导，例如：

- 「并行开三个 explorer：架构 / 稳定性 / 工具，查完汇总。」
- 「Use multi-agent: spawn explorers for architecture and security in parallel.」

## 如何关闭

**项目**（`.leveler/config.yaml`）：

```yaml
agents:
  delegation: false
```

**全局**（`~/.leveler/config.toml`）：

```toml
[agents]
delegation = false
```

项目与全局为 **与** 关系：任一侧关掉则不向模型广告 `spawn_agent`。

## TUI 上看到什么

并发子 agent 以**树**展示。运行中每个子节点显示运行时上报的**真实**最近工具/步骤
（如 `list_files`），不编造统计。token 用量仍来自 `SubAgentProgress`（若有）。

## 浏览器

本轮产品以协议 + TUI 为主；协议事件
`sub_agent_updated` / `sub_agent_progress` / `sub_agent_activity`
可供 Web 等客户端绑定。
