# 文档索引（中文）

| 文档 | 读者 | 说明 |
| --- | --- | --- |
| [ARCHITECTURE.zh-CN.md](ARCHITECTURE.zh-CN.md) | 贡献者 | 架构与边界 |
| [ARCHITECTURE.md](ARCHITECTURE.md) | 贡献者 | 英文架构 |
| [leveler-config-example.yaml](leveler-config-example.yaml) | 用户 | 项目 `.leveler/config.yaml` |
| [permissions.example.yaml](permissions.example.yaml) | 用户 | 权限规则 |
| [hooks.example.yaml](hooks.example.yaml) | 用户 | 工具钩子 |
| [../configs/example.yaml](../configs/example.yaml) | 用户 | 全局 + 包配置完整 schema |
| [../README.zh-CN.md](../README.zh-CN.md) | 所有人 | 中文入口 |
| [../README.md](../README.md) | 所有人 | 英文入口 |

## 配置分几层

1. **全局** `~/.leveler/config.toml`；Windows 请设置 `LEVELER_HOME` 后使用
   `%LEVELER_HOME%\config.toml`<br>
   默认模型、API provider、密钥环境变量名、MCP。
2. **包配置** `configs/providers/*.yaml`、`configs/models/*.yaml`<br>
   可提交进仓库的兼容档案；详见 `configs/example.yaml`。
3. **项目** `<repo>/.leveler/config.yaml`<br>
   本仓库的 model / mode / verify / ignore / readonly_roots / limits。
4. **权限 / hooks**<br>
   用户级 `~/.leveler/` 与项目级 `.leveler/` 均可。

## 权限 profile（与代码一致）

| 值 | 含义 | CLI |
| --- | --- | --- |
| `request_approval` | 外部编辑与网络都先问 | `--permission request-approval` |
| `assisted` | 默认；高风险才问 | `--permission assisted` |
| `full_access` | 几乎不限制（慎用） | `--permission full-access` |

兼容旧值（仍可解析，不建议新配置使用）：

- `plan` → `request_approval`
- `workspace_write` → `assisted`

## 项目 config 要点

复制并改写：[`leveler-config-example.yaml`](leveler-config-example.yaml)

- 只要写了 `verify` 里任一命令，就**整段替换**语言自动发现计划。
- `format` 失败**不阻塞**完成；`build` / `test` **阻塞**完成。
- `readonly_roots`：可多读相邻仓库，**不能**写入那些路径。

## 权限规则要点

复制并改写：[`permissions.example.yaml`](permissions.example.yaml)

- 匹配规则优先级：`deny` > `ask` > `allow`。
- 无匹配 → 回落到当前 session 的 permission profile。
- 交互里选「始终允许」会写入**项目**规则；`leveler permissions clear` 只清项目文件。

## Hooks 要点

复制并改写：[`hooks.example.yaml`](hooks.example.yaml)

- `pre_tool_use`：`exit 0` 放行；`exit 2` 与其它非 0 / 启动失败均拒绝。
- `post_tool_use`：只观察，失败忽略。
- 环境变量：`LEVELER_HOOK`、`LEVELER_TOOL`、`LEVELER_TOOL_ARGS_JSON`。
