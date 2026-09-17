# 自定义 Agent

英文版：[`AGENT_EXTENSIBILITY.md`](AGENT_EXTENSIBILITY.md)

一个 Agent 就是一个目录，里面两份文件。`agent.yaml` 说明它可以做什么；`instructions.md` 说明它该怎么做。CodeLeveler 解析当前项目能用哪些 Agent，主 Agent 在任务需要时挑选一个（你也可以直接点名），运行时强制执行定义里的每一条边界。新增 Agent 不需要改代码，也不需要重新编译。

## 怎么创建

| 你是 | 这样做 |
| --- | --- |
| 在对话里 | 说「给这个项目创建一个只读的安全审查 Agent」。CodeLeveler 会提出定义并展示给你；未经确认不会写入。 |
| 用 Web UI | 打开一次对话，然后 设置 → Agents → 创建 Agent。 |
| 自己改文件 | 创建 `.leveler/agents/<name>/agent.yaml` 和 `instructions.md`。 |
| 一个团队 | 把项目的 `.leveler/agents/` 目录提交进 Git。 |

查看实际解析结果：`leveler agents list`（或 `--json`）、`leveler agents show <name>`、TUI 里的 `/agents`，或 Web UI 的 设置 → Agents。

## 目录布局

```text
<repo>/.leveler/agents/<name>/agent.yaml         项目 Agent
<repo>/.leveler/agents/<name>/instructions.md
~/.leveler/agents/<name>/agent.yaml              用户 Agent，对每个项目生效
~/.leveler/agents/<name>/instructions.md         （设置了 $LEVELER_HOME 时用 $LEVELER_HOME/agents）
```

内置 Agent 随二进制发布：运行时角色 `default`、`explorer`、`worker`、`reviewer`，以及人格 `code-explorer`、`code-architect`、`code-reviewer`。

目录名就是 Agent 名。以 `.` 开头的条目会被忽略——store 用它们做暂存——普通文件也会被忽略。更早构建里那种单文件 `.leveler/agents/<name>.md` 格式已经不再读取。恰好是这种形状的文件（`<name>.md`，带 `---` frontmatter，直接放在 agents 目录里）会在每次列表和 `leveler doctor` 里被标成未加载，并给出应该迁到的目录。系统不会替你迁移、重命名或删除。

## 优先级

同名时，项目 Agent 盖住用户 Agent，用户 Agent 盖住内置：

```text
项目  >  用户  >  内置
```

被盖住的定义仍会报告出来（`shadows user`、`shadows builtin`）。

两条规则避免优先级把人绕进去：

- **坏定义不会回退。** 项目里的 `foo` 无效，`foo` 就是无效：不会改用用户的 `foo`。Spawn 会被拒绝，并给出原因。
- **运行时角色名是保留的。** `default`、`explorer`、`worker`、`reviewer` 在运行时有结构含义（Worker 预先声明的范围、Harness 拉起的 Reviewer），因此这些名字的目录会被报告为问题并且不加载。人格是普通定义，可以被覆盖。

## `agent.yaml`

```yaml
version: 1                      # 必填；当前构建只读 version 1
name: security-reviewer         # 必填；必须等于目录名
description: Reviews Rust changes for exploitable security issues.
capability: read_only           # 必填：read_only | writer | scoped_writer
model: deepseek/deepseek-v4-pro # 可选；省略 = 父 Agent 的模型
reasoning_effort: high          # 可选：minimal|low|medium|high|xhigh|max
skills: [rust-security]         # 可选；已安装的 skill 名
tools: [read_file, grep, git_diff, find_files]   # 可选，收窄工具集
workspace:                      # 可选；仅 writer 类
  write_roots: [crates/leveler-web/web]
budget:                         # 可选
  max_rounds: 40
  max_duration_secs: 900
```

| 字段 | 规则 |
| --- | --- |
| `version` | 必填，且必须是 `1`。缺了就拒绝，不会猜。 |
| `name` | `[a-z][a-z0-9-]*`，最多 64 字符，不能以 `-` 结尾，不能是 Windows 设备名。只允许小写，这样 `Foo` 和 `foo` 不会在一个文件系统上是两个 Agent、在另一个上又是同一个。 |
| `description` | 一行，最多 200 字符。主 Agent 每一轮选人时读的就是它。 |
| `capability` | 这个 Agent 跑在哪一类运行时契约下——见下。 |
| `model` | `provider/model`。本机必须已配置，否则这个 Agent 不可用；永远不会被换成别的模型。 |
| `reasoning_effort` | 必须是该模型恰好提供的档位，否则不可用。运行时平常那种就近取整在这里不适用。 |
| `skills` | 每个 skill 必须已安装（项目、用户或内置），否则不可用。 |
| `tools` | 收窄该类的工具集。每个名字必须是已知工具；`read_only` Agent 只能列只读工具；不能列 MCP 工具。`[]` 是错误——要全集就省略这个字段。列出的每一个工具都是必需的：当前 session 缺一个（例如没有搜索 key 时的 `web_search`），这个 Agent 就不能在这里跑。 |
| `workspace.write_roots` | 仅 writer 类。仓库相对路径的目录或文件，用 `/` 分隔，不要 `..`，不要 glob，不要仓库根。 |
| `budget.max_rounds` | 1–1000。 |
| `budget.max_duration_secs` | 1–1200（运行时对子 Agent 的墙钟上限）。 |

未知字段在每一层都会让定义失败。把 `workspace: { wirte_roots: [...] }` 或 `api_key: ...` 这种拼错写进去，Agent 会直接无效，而不是被静默忽略——也没有任何字段可以放凭据。

## 能力类

定义不会创造一种新的 Agent。它跑在运行时已有的子 Agent 契约下，声明的每一条边界都只能收窄这份契约：

| `capability` | 按谁跑 | 可以写什么 |
| --- | --- | --- |
| `read_only` | Explorer 契约 | 什么都不能写。可变工具根本不会出现，包括 `run_command`。 |
| `writer` | Default 子 Agent 契约 | 只有读完代码后用 `claim_write_scope` 声明的路径；若设了 `write_roots`，还只能在那些根里面。 |
| `scoped_writer` | Worker 契约 | 只有 spawn 时拿到的独占 `files`——若设了 `write_roots`，这些文件必须落在里面。 |

```text
agent maximum   = capability class ∩ tools ∩ write_roots   (agent.yaml)
spawn request   = agent + files                              (spawn_agent 调用)
effective       = 运行时准入实际允许的 ⊆ agent maximum
```

Spawn 可以重复 Agent 自己的类（`read_only` Agent 用 `role="explorer"`），但不能改类：`spawn_agent(agent="security-reviewer", role="default")` 会被拒绝。

子 Agent 永远拿不到 MCP 工具和 Agent 编写工具，也不能再 spawn 自己的子 Agent。

## `instructions.md`

角色相关的指引：看什么、报什么、怎么报。必填，必须是非空 UTF-8 文本，最多 64 KiB。

Spawn 时，子 Agent 的第一条系统消息按这个顺序拼：

```text
base prompt → 项目规则（AGENTS.md、.leveler/instructions.md）→ 能力类文本
→ Agent instructions → 每一个绑定的 skill
```

子 Agent 的任务仍然是它自己的用户消息。主 Agent 只看到名字和描述（每轮目录上限 4 KiB）；完整 instructions 只到子 Agent。

**Instructions 不是权限。** 它们塑造 Agent 怎么工作，不改变它持有哪些工具、可以写哪些文件，子 Agent 也会被告知这一点。不要把运行时规则、工具 schema 或权限机制写进去——那些由运行时强制——也**不要把凭据放进 Agent instructions**：它们会被发给模型。

## 有效性与可用性

每次列表都会给每个 Agent 一个状态：

| 状态 | 含义 | 能否 spawn |
| --- | --- | --- |
| `available` | 有效，并且它点名的东西在这里都已配置。 | 能 |
| `unavailable` | 有效，但模型未配置、缺 skill，或模型不提供这个 reasoning effort。 | 不能 |
| `invalid` | 文件本身就不对：YAML、schema、名字、缺 instructions、符号链接。 | 不能 |

一个坏 Agent 不影响其他 Agent，也不会阻止 CodeLeveler 启动。`leveler doctor` 按名字报告无效定义。

`available` 是配置检查：模型已配置、effort 该模型提供、skill 已安装。它不检查这个模型的 provider 有没有可用的 API key；钉死的模型缺 key 时，失败发生在子 Agent 第一次请求，错误来自 provider。因此 Web UI 把这个状态标成「已配置」，而不是「available」。

## 使用一个 Agent

主 Agent 可以为任务的一部分挑选一个 Agent；出现在列表里只表示它可用，并不表示它会被用。你也可以直接点名：「让 security-reviewer 检查这次改动」。名字解析失败是错误，并列出当前可用的 Agent——永远不会回退到默认子 Agent。

Harness 独立拉起的 Reviewer 不是你可以点名的 Agent。名字里带 "reviewer" 也不会获得特殊权威：它的发现和其他子 Agent 的结果一样。

## 正在跑的子 Agent 沿用 spawn 时的定义

Agent 被 spawn 时，解析后的定义会记在子 Agent 的持久化启动记录上（名字、来源、指纹、能力类、effort、skills、write roots、budget、model、tools），instructions 和 skills 也进入它自己的第一条系统消息。重启后的子 Agent 严格按这份快照继续跑，不再读文件：

- 编辑一个 Agent 只影响**之后**的 spawn；
- 删除一个 Agent 不会停掉正在跑的子 Agent，已结算的历史仍显示这个 Agent 的名字；
- 以只读 spawn 的子 Agent，即使文件后来改成 `writer`，仍然只读。

**指纹**（`sha256:…`）覆盖所有塑造行为的字段和 instructions，不含位置，所以把同一个目录复制到另一个仓库，指纹相同。它用于检查和调试，不是签名。

Registry 在需要时重读（每一批 spawn、每一轮目录、每一次列表）；没有文件监视器，也不需要重启。

## 安全地改 Agent

所有界面都走同一个 store：

- **create** 先把两个文件写进隐藏暂存目录，再 rename 就位——Registry 看不到半成品；
- **update** 暂存整个新目录，换进去，再删旧的；内容没变的文件按字节原样带过去，所以只改 instructions 时手写的 `agent.yaml` 能保住注释，什么都没变就不写盘。UI 保存如果改了 manifest，会按 schema 字段顺序重写；
- **delete** 先把目录 rename 走，再删除。

没有 rename 操作：用新名字 create，再 delete 旧的。内置 Agent 不能编辑；复制一份（`leveler agents show code-reviewer`，或 Web UI 里的「复制」）到另一个名字的项目或用户 Agent。

Agent 自己的编辑工具不能原地改 `.leveler/agents/`（`write_file` / `apply_patch` 在这里会被拒绝，和 `.leveler/hooks.yaml` 一样），所以从对话里做的修改一定走这条路径。shell 命令不受这条拒绝覆盖——和 hooks 文件同一个限制——那样写出来的定义加载时仍然会校验。拒绝按大小写不敏感匹配目录，所以 `.LEVELER/Agents/…` 也会被拒绝（在 macOS 和 Windows 上这就是同一个目录）。

从对话里，`save_agent` 和 `delete_agent` 先校验提案——只读 reviewer 却要 `apply_patch` 这种自相矛盾，会直接拒绝、不问你——然后在每一种权限模式（包括完全访问）下都要你确认。确认里会展示范围、能力类、写入边界、工具、模型、effort、skills 和 instructions。一次 session 批准只覆盖那份精确提案。无人值守的 run 不能写 Agent。写 Agent 永远不会变成一条 standing permission，所以批准提示也不会给它（以及 `remember` / `forget`）提供「始终允许」。

## Trust

**项目 Agent 是仓库控制的 instructions。** 和 `AGENTS.md` 一样，它们来自写这个仓库的人。对不信任的仓库，先审查 `.leveler/agents/`。无论 Agent 的 instructions 写了什么，它的权威都是上面的能力类和边界；instructions 不能授予工具或写入范围。

用户 Agent 在仓库外面。仓库内容不能创建它们：从对话写用户 Agent 需要你明确确认，Web UI 只在你操作时写入。远程（移动端）客户端能看到正在跑的子 Agent 名字，但不能列出或修改 Agent。

## 例子

只读审查：

```yaml
version: 1
name: security-reviewer
description: Reviews changes for exploitable security issues and reports only high-confidence findings.
capability: read_only
reasoning_effort: high
tools: [read_file, grep, find_files, git_diff, git_status]
```

```markdown
Review the change you are given for vulnerabilities an attacker could use:
injection, path traversal, authorization gaps, secrets in code or logs.
Report each with file:line, the input that triggers it and the consequence.
Report nothing you are not confident in.
```

限定在一个子树里写前端：

```yaml
version: 1
name: frontend-worker
description: Implements small React/TypeScript changes under the web app.
capability: writer
workspace:
  write_roots: [crates/leveler-web/web/src]
budget:
  max_rounds: 60
```

## 这一版没有的东西

市场或远程安装、签名或打包的 Agent 包、代码或 WASM 插件、能 spawn 或互相通话的 Agent、定义之间的继承（`extends:`）、按 Agent 的生命周期 hooks、远程 instructions URL、Agent 自定义工具、移动端编辑器、文件监视器。
