# 跨会话记忆

[English](MEMORY.md)

CodeLeveler 可以跨会话记住持久事实。记忆是运行时原生能力：一轮结束后，Capture 把
事实提取到观测收件箱；Dream 把观测整理成 Markdown 主题；每轮提示里带一份有界的主题
索引，模型按需读取。

记忆**默认关闭**。在启用 rollout 之前，不捕获、不打开数据库、也不创建目录。

## 启用

在全局配置（`~/.leveler/config.toml` 或 `$LEVELER_HOME/config.toml`）中设置：

```toml
[memory]
rollout = "active"
```

| Rollout | 行为 |
| --- | --- |
| `off` | 不捕获、不整理、不读取。默认值。 |
| `record_only` | 只把观测写成文件，不暴露给模型，也不运行 Dream。 |
| `shadow` | 运行 Capture 与 Dream 计划，但不提交主题变更。 |
| `active` | 完整流水线；只有该值会把记忆暴露给模型。 |

其余键均可省略：

```toml
[memory]
rollout = "active"
model = "deepseek/deepseek-flash"   # Capture/Dream 路由；不设置则复用会话模型
capture_enabled = true
automatic_dream_enabled = true
manual_dream_enabled = true
batch_dream_enabled = false
batch_dream_max_run_secs = 1800
batch_dream_max_calls_per_batch = 6
batch_dream_max_batch_note_bytes = 98304
compact_index_enabled = false
file_writes_enabled = true
archived_retention_days = 30
job_retention_days = 14
```

rollout 修改对之后启动的会话生效。唯一例外是 `/remember`：即使 rollout 为 `off`，
它也按用户要求写入那条笔记，确保之后启用的记忆能读到它。

## 数据位置

全部位于 CodeLeveler home 下，绝不写进仓库：

```text
<LevelerHome>/memory/
  global/                         # 所有工作区共享的笔记
  workspaces/<slug>-<hash8>/      # 每个仓库身份一个目录
    MEMORY.md                     # 生成的、有界的索引（不要手动编辑）
    topics/*.md                   # 整理后的 Markdown 主题
    observations/_inbox/*.md      # 捕获的观测
    archive/                      # 已整理或过期的观测
    memory_state.sqlite           # 范围、任务、租约、墓碑、审计
    index.sqlite                  # 内部全文收敛索引
```

工作区身份优先取 git remote，因此同一仓库的两个 clone 共享记忆；没有 remote 时使用
规范化路径。临时工作目录没有工作区作用域，不会积累记忆，只能使用全局作用域。

`MEMORY.md` 由主题集重新生成，并受预算限制：只列主题路径和一行描述，避免大记忆库挤占
对话。模型在依赖某个主题前，会用普通的 `read_file` 读取它。

## 模型能看到什么

rollout 为 `active` 时，每轮系统上下文会带一个 `## Cross-session memory` 段，包含
生成的索引和两个作用域根目录。提示明确说明：记忆是历史上下文，不是当前计划；召回
的路径、命令和仓库事实必须用实时工具验证，记忆与工作区冲突时以当前证据为准。记忆文件
就是普通 Markdown，没有独立的检索 API，也没有向量数据库。

## 与运行时、会话、Goal 的关系

记忆归运行时所有，不属于某一轮对话或模型本身。捕获以会话为单位：每个会话维护自己的
游标，一个任务覆盖该会话中一段已结束的回合。恢复会话会恢复其捕获游标；同一工作区里的
新会话共享主题，但有自己的待捕获范围。Capture 与 Dream 作为带租约的后台工作运行，
关闭 TUI 不会丢失已排队的任务，进程崩溃由上面的租约规则处理。

`/goal` 回合与普通回合一样在结束后被捕获。记忆不参与 Goal 完成门禁、不决定下一步，
也不会把模型自由文本变成控制流：它只提供索引和普通 Markdown 文件，由模型决定是否读取。
关闭记忆时，会话、Goal、工具和压缩仍按原行为运行，只是没有捕获、Dream 和提示索引。

## 诊断

首先运行 `leveler memory status`：它报告 rollout、每个作用域的 pending/running/retry/
failed/completed 捕获任务、待处理观测、文件列表、墓碑数和 Dream 租约状态。`--json`
提供同一份投影供脚本读取。失败的捕获任务会保持计数并重试，直到进入终态；过期 Dream
租约显示为 `expired`，活跃租约显示为 `active`。TUI 的 Issue 行和会话报告中的
`memory_capture`/`memory_dream` 计数都指向同一份持久记录，因此界面上的征兆总能追溯到
具体任务或观测文件。

`leveler doctor` 覆盖配置、Provider 和工作区问题，不是记忆专用命令。

## 测试

记忆引擎的持久语义在自身 crate 中测试：

```sh
cargo test -p leveler-memory
```

宿主集成（应用运行时中的捕获、关闭 rollout、重启与对账）在 app 的记忆测试中：

```sh
cargo test -p leveler-app --test memory_integration
cargo test -p leveler-app --test memory_disabled_integration
```

TUI 记忆页的行为测试在 TUI crate 中：

```sh
cargo test -p leveler-tui --lib memory
```

记忆测试使用临时 home，不会读写 `~/.leveler`。

## 捕获（Capture）

每轮用户回合结束后，运行时会为该会话尚未捕获的范围排队。一个捕获任务覆盖一段连续的
已结束回合。提取调用使用 `[memory].model` 指定的路由或会话模型，关闭思考并强制工具
调用，只产出结构化观测，不会产出自由文本控制指令。

观测写成带 YAML 头的 Markdown 文件（会话、回合范围、类型、主题提示、提示版本），并在
SQLite 中登记。Capture 不修改模型的对话，也不阻塞回合：记忆失败会作为活动状态报告，
用户的工作已经成功。

任务持久化。worker 以五分钟租约认领任务，因此崩溃进程的任务在租约到期前不会被重启后的
进程抢走；该会话的下一轮会对齐缺口并完成任务。提交是幂等的：已经用相同结果提交过的
任务返回已存结果，不再写一遍。

## 整理（Dream）

Dream 读取待处理观测并编辑主题集：创建或更新主题、保持描述简短、归档已整理的观测。
`memory_state.sqlite` 里的 `consolidation_lock` 行跨进程串行化 Dream：单一 owner、
generation 和六小时租约。owner 进程已死的租约按过期处理，因此 Dream 中途崩溃不会阻塞
后续 Dream 数小时。维护、索引提升和墓碑对账都使用同一套租约规则。

触发方式：

- `/dream` 或记忆页上的 `d` 运行一次整理。
- 记忆页上的 `D` 运行批量 Dream：多轮有界整理，带容量延迟、调用预算和字节预算。
- Capture 结算后，若 rollout 允许，会自动运行 Dream。
- `/flush` 立即排空当前会话的捕获队列（worker 已占用时报告 busy）。

## 查看与管理

### TUI

`/memory` 打开跨会话记忆页，显示两个作用域、文件列表、待处理观测、归档、墓碑和 Dream
租约状态。

| 按键 | 操作 |
| --- | --- |
| `↑` `↓` / `j` `k` | 移动选择 |
| `←` `→` / `h` `l` | 切换全局 / 工作区作用域 |
| `Tab` / `BackTab` | 切换窗格 |
| `/` | 过滤文件 |
| `Enter` / `Ctrl+F` | 全屏阅读选中文件；`Ctrl+F`、`Enter` 或 `Esc` 返回列表 |
| `PageUp` / `PageDown` | 翻页详情 |
| 连按两次 `x` | 遗忘选中文件（第二次确认） |
| `y` | 复制选中路径 |
| `t` | 暂停 / 恢复本会话的记忆，不修改配置 |
| `r` | 刷新 |
| `d` | 运行 Dream |
| `D` | 运行批量 Dream |
| `f` | 刷新索引 |
| `Esc` | 返回 |

`t` 只作用于当前会话：配置里的 rollout 仍是权威，页面不能把 `off` 提升为开启，也不写回
`config.toml`。暂停后不再注入提示索引，也不再认领新的捕获任务；恢复后补捕获暂停期间
已结束的回合。暂停只存在于本进程：新会话、以及重启后重新打开的原会话，都按配置启动。

### CLI

```sh
leveler memory status                 # rollout、作用域、计数、文件、租约
leveler memory status --json          # 机器可读
leveler memory show workspace topics/build.md   # 预览；BLAKE3 输出到 stderr
leveler memory remember "用 pnpm，不用 npm"     # 追加全局笔记
leveler memory forget workspace topics/build.md --hash <BLAKE3>
```

会话内命令：`/memory`、`/remember <文本>`、`/dream`、`/flush`。

## 遗忘

遗忘绑定 `memory show` 打印的精确文件哈希。运行时先写墓碑行，再删除文件；两者之间崩溃
会在下次打开存储时对账。墓碑和审计账本保留删除记录，该路径会同时从索引、Capture 和
整理中排除，因此新会话无法从观测收件箱中复活已遗忘的事实。

整理后的观测归档在 `archive/` 下，作为 Dream 收敛与审计的运行时证据。模型的文件工具
不能读取该目录：工具读取会被拒绝，匹配的搜索结果会被省略，即使被遗忘的事实仍暂存在
那里，遗忘对后续会话依然生效。用户自己的预览在保留期内可以打开归档文件。

保留期可配置：归档默认保留 30 天，终态任务元数据默认保留 14 天。

## 限制

- 记忆不保存完整对话，只保存提取出的观测和整理后的主题。
- 没有向量索引，也没有知识图谱。
- Capture 与 Dream 是模型调用，会消耗 token；会话报告把它们分别列为
  `memory_capture`、`memory_dream`，与 `round` 调用并列。
- 临时工作目录没有工作区记忆；不同仓库之间不共享记忆，除非它们共享同一个 git remote。

