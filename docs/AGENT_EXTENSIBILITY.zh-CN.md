# 自定义 Agent

[English](AGENT_EXTENSIBILITY.md) · [文档索引](README.zh-CN.md)

如果你经常请 CodeLeveler 做同一种工作，可以把要求写进一个自定义 Agent。例如，给安全审查 Agent 写好检查范围，以后在对话里点名调用。

## 创建

在 Web 界面打开「设置 → Agents」，或直接在对话里请 CodeLeveler 创建。通过对话保存或删除时，界面会先展示要改的内容，等你确认。

也可以自己建目录。每个 Agent 需要两个文件，`agent.yaml` 填设置，`instructions.md` 写工作要求。

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

名字需与目录名一致，使用小写字母、数字和连字符，并以字母开头。工作说明会发给模型，请把凭据留在配置引用或环境变量里。

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

例如，在对话里输入「让 security-reviewer 检查这次改动」。如果报错说定义有问题、找不到模型或技能，用 `leveler doctor` 排查。API Key 缺失也可能到调用模型时才报错。

修改定义只影响之后启动的 Agent。删除定义不会停止已经运行的任务。

打开陌生项目时，先读一下仓库里的 Agent 定义。工作说明只能在已有的工具与写入权限内生效。
