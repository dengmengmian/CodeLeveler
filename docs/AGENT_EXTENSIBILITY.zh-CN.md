# 自定义 Agent

[English](AGENT_EXTENSIBILITY.md) · [返回首页](../README.zh-CN.md)

自定义 Agent 为特定工作提供固定说明，例如只读代码审查或前端开发。

## 创建

可以直接让 CodeLeveler 创建，也可以在 Web 界面的「设置 → Agents」中创建。通过对话保存或删除定义前，会展示内容并要求你确认。

手动创建时，每个 Agent 是一个目录，包含 `agent.yaml` 和 `instructions.md`：

| 位置 | 生效范围 |
| --- | --- |
| `.leveler/agents/<name>/` | 当前项目，可提交到 Git 与团队共享 |
| `~/.leveler/agents/<name>/` | 当前用户的所有项目 |

同名时项目定义优先，其次是用户定义，最后是内置定义。`default`、`explorer`、`worker`、`reviewer` 是保留名称，不能用作自定义 Agent 名。

## 最小示例

创建 `.leveler/agents/security-reviewer/agent.yaml`：

```yaml
version: 1
name: security-reviewer
description: Reviews code for security issues.
capability: read_only
```

在同一目录创建 `instructions.md`：

```markdown
检查用户指定的改动，只报告有充分证据的安全问题。
每条说明影响、触发条件和修复方向；没有发现就直接说明。
```

名字需与目录名一致，使用小写字母、数字和连字符，并以字母开头。工作说明不要放凭据，会随请求发送给模型。

## 常用设置

| 字段 | 用途 |
| --- | --- |
| `capability` | `read_only` 只读；`writer` 可在声明范围内写入；`scoped_writer` 只写被分配的文件 |
| `model` | 可选，填写已配置的 `provider/model`；省略则沿用父 Agent 模型 |
| `reasoning_effort` | 可选，必须是所选模型支持的档位 |
| `skills` | 可选，填写已安装的技能名称列表 |
| `tools` | 可选，限制工具列表；不能为空，也不能授予额外权限 |
| `workspace.write_roots` | 可选，限制可写目录；使用仓库相对路径，不使用 `..` 或通配符 |

例如，限制写入前端目录：

```yaml
version: 1
name: frontend-worker
description: Implements frontend changes.
capability: writer
workspace:
  write_roots: [crates/leveler-web/web/src]
```

## 使用与排错

运行 `leveler agents list` 查看可用 Agent，`leveler agents show <name>` 查看定义，或在终端输入 `/agents`。

在对话中点名即可，例如「让 security-reviewer 检查这次改动」。无效定义、缺失模型或技能会明确报错，可用 `leveler doctor` 排查；缺 API Key 仍可能在实际调用时失败。

修改定义只影响之后启动的 Agent。删除定义不会停止已经运行的任务。

项目 Agent 来自仓库；使用不信任的项目之前，请先检查其定义。工作说明不能扩大工具或写入权限。
