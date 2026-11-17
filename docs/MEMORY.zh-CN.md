# 跨会话记忆

[English](MEMORY.md) · [文档索引](README.zh-CN.md)

换个会话，你仍可让 CodeLeveler 参考前面记下的项目约定。它会在回合结束后从对话中提取笔记（Capture），再把这些笔记整理为 Markdown 主题（Dream）。后续会话只收到一份简短索引，模型需要哪条再打开哪条。

记忆默认开启。输入 `/memory` 可查看笔记，输入 `/remember <文本>` 可手动记下一条。要关闭自动记忆，把配置设为 `rollout = "off"`。

## 查看与管理

### TUI

输入 `/memory` 打开记忆页。你可以切换全局和工作区，查看笔记、待整理项、归档、删除记录和整理锁。

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

按 `t` 只暂停当前会话。暂停后你仍可查看已存笔记。模型看不到记忆索引，程序也不会认领新的提取任务。暂停期间结束的回合会被跳过，恢复后不会补抓。新会话或重启后的会话按配置启动。全局设为 `off` 时，按键无法开启记忆。

### CLI

```sh
leveler memory status                 # rollout、作用域、计数、文件、租约
leveler memory status --json          # 机器可读
leveler memory show workspace topics/build.md   # 预览；BLAKE3 输出到 stderr
leveler memory remember "用 pnpm，不用 npm"     # 追加全局笔记
leveler memory forget workspace topics/build.md --hash '粘贴 show 输出的 BLAKE3 哈希'
```

会话内命令：`/memory`、`/remember <文本>`、`/dream`、`/flush`。

## 配置

在全局配置 `~/.leveler/config.toml`（设置了 `LEVELER_HOME` 时用 `$LEVELER_HOME/config.toml`）里修改 `rollout`。未写 `[memory]` 时默认是 `active`。

```toml
[memory]
rollout = "active"
```

| `rollout` 值 | 行为 |
| --- | --- |
| `off` | 不捕获、不整理、不读取，也不创建存储。 |
| `record_only` | 把提取出的笔记写成文件，暂停整理和向模型提供记忆。 |
| `shadow` | 提取笔记，生成整理计划，主题文件保持原样。 |
| `active` | 提取并整理笔记，也允许模型在后续会话里读取。默认值。 |

其余键均可省略：

```toml
[memory]
rollout = "active"
model = "deepseek/deepseek-flash"   # 提取和整理使用的模型，省略时沿用会话模型
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

配置文件更新后，程序会自动重读，也可以输入 `/reload` 立即重读。记忆页和后续的提取、整理任务会使用新设置。即使设为 `off`，也可以用 `/remember` 手动写一条笔记，等再次开启时读取。

## 数据位置

记忆文件保存在 CodeLeveler 的数据目录里，独立于 Git 工作区。

```text
<LevelerHome>/memory/
  global/                         # 所有工作区共享的笔记
  workspaces/<slug>-<hash8>/      # 每个仓库身份一个目录
    MEMORY.md                     # 自动生成的主题索引
    topics/*.md                   # 整理后的 Markdown 主题
    observations/_inbox/*.md      # 待整理的笔记
    archive/                      # 已整理或过期的笔记
    memory_state.sqlite           # 范围、任务、租约、墓碑、审计
    index.sqlite                  # 内部全文索引
```

配置了同一 Git 远端地址的两个克隆会共享工作区记忆。没有远端时，程序用规范化后的本地路径区分工作区。临时工作目录只能使用全局记忆。

程序根据主题生成 `MEMORY.md`。索引有长度上限，每个主题只列路径和一行说明。模型需要详情时用 `read_file` 读取对应文件。

## 模型能看到什么

仅在 `rollout = "active"` 时，系统提示词中的 `## Cross-session memory` 才会列出主题索引、全局记忆目录和工作区记忆目录。模型需要用当前工作区核对记忆里提到的路径、命令和事实，有出入时以当前证据为准。

记忆以 Markdown 保存。模型通过文件工具读取，产品没有独立的检索 API 或向量数据库。

## 捕获（Capture）

每轮结束后，运行时把该会话还未提取的一段已结束回合排入后台队列。提取用 `[memory].model` 指定的模型，未设置时沿用会话模型。这次调用关闭思考，只通过工具提交结构化笔记。

每条笔记是带 YAML 文件头的 Markdown，记录会话、回合范围、类型、主题提示和提示词版本。SQLite 也会登记这次任务。提取失败会记在活动记录里，原回合的结果照常保留。

后台进程有五分钟时间持有任务，进程崩溃后须等租约到期才能重新认领。下次启动会恢复可认领的任务，后续回合会检查是否还有未处理范围。提交相同结果时直接返回已保存的记录。

## 整理（Dream）

Dream 读取待整理笔记，创建或更新 Markdown 主题，然后归档已处理的笔记。同一时间只允许一个进程整理。`memory_state.sqlite` 的 `consolidation_lock` 记录持有者、版本与六小时租约。如果持锁进程已退出，租约立即视为过期。维护、发布索引和处理未完成的删除也用这把锁。

- 输入 `/dream` 或在记忆页按 `d`，整理一次。
- 按 `D` 批量整理。每批受调用次数、字节数和运行时间限制，容量不足时会等待。
- 配置允许时，每次提取结束都会触发自动整理。
- 输入 `/flush` 处理当前会话已排队的提取任务。后台正在处理时会提示忙。

## 遗忘

先用 `leveler memory show` 查看文件和 BLAKE3 哈希，再把这个哈希传给 `forget`。哈希要与当前内容一致，避免删掉预览后已被更新的文件。程序先记下删除记录，再删除文件。如果中途崩溃，下次打开存储时会继续处理。索引、提取和整理都会跳过已删除的路径。

已整理的笔记原文暂存在 `archive/`。用户可在保留期内预览。模型的文件工具不能读归档，搜索也不会返回其中的记录。

归档默认保留 30 天，已结束任务的元数据默认保留 14 天，两者都可配置。

## 与运行时、会话、Goal 的关系

运行时为每个会话记录已经提取到哪一轮。重新打开会话时从那个位置继续。同一工作区的新会话可以读取同一份主题，但各自记录提取进度。关闭 TUI 后，已入队的后台任务仍保存在本地。进程崩溃后的处理见上面的提取和整理说明。

`/goal` 和普通回合一样在结束后提取笔记。Goal 的完成判断、下一步动作、工具和上下文压缩照常运行。记忆只向模型提供索引和 Markdown 文件。

## 诊断

运行 `leveler memory status`。它会按全局和工作区列出各状态的提取任务、待整理笔记、文件、删除记录和整理锁的状态。`--json` 输出相同信息，方便脚本读取。提取失败会计数并按规则重试，直到任务结束。整理锁显示 `active`（仍有效）或 `expired`（已过期）。

TUI 问题提示和会话报告里的 `memory_capture`、`memory_dream` 可追溯到同一份任务或笔记记录。模型服务商和工作区配置出问题时，用 `leveler doctor`。

## 限制

- 记忆不保存完整对话，只保存提取出的观测和整理后的主题。
- 没有向量索引，也没有知识图谱。
- Capture 与 Dream 是模型调用，会消耗 token；会话报告把它们分别列为
  `memory_capture`、`memory_dream`，与 `round` 调用并列。
- 临时工作目录没有工作区记忆；不同仓库之间不共享记忆，除非它们共享同一个 git remote。

## 测试

在仓库根目录运行记忆存储测试。

```sh
cargo test -p leveler-memory
```

下列集成测试覆盖笔记提取、关闭记忆、重启恢复和记录一致性。

```sh
cargo test -p leveler-app --test memory_integration
cargo test -p leveler-app --test memory_disabled_integration
```

记忆页的交互测试使用下列命令。

```sh
cargo test -p leveler-tui --lib memory
```

记忆测试使用临时 home，不会读写 `~/.leveler`。
