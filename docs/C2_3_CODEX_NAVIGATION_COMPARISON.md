# C2.3 — Codex Navigation Comparison (Source Study)

日期：2026-08-09。分支 `feat/coding-context-efficiency-c2`。

**来源：本机 OpenAI Codex 仓库 `/Users/mengmian/Develop/app/other/codex`，HEAD `3aae5d885b`。全部结论逐条引自源码原文，未依赖记忆或文档转述。** 附带一次 Grok Build 的同任务实跑对照。

---

## 0. 结论前置

Codex 的代码导航优势**不来自更好的读取工具** —— 它根本没有面向模型的读取工具。优势来自三件事：

1. **搜索是被提示词点名的默认入口**（`rg` / `rg --files`），读取是定位之后的动作。
2. **每次读取天然有 token 预算**（shell `max_output_tokens` 默认 **10000**），并把截断量告诉模型。
3. **没有任何机制在导航中途打断模型** —— 计划工具是纯提示词建议，不存在强制 `tool_choice`。

CodeLeveler 的结构化 `read_file` 在跨平台、沙箱、指纹、恢复上**明确优于** shell 组合，不应放弃。要学的是**搜索优先的导航纪律**与**有预算的证据获取**，不是 shell 实现。

---

## 1. Codex Navigation Model

**模型可调用的工具处理器全集**（`codex-rs/core/src/tools/handlers/`）：

```
apply_patch          current_time         get_context_remaining
list_available_plugins_to_install         mcp / mcp_resource
multi_agents (+v2)   new_context_window   plan
request_permissions  request_plugin_install                request_user_input
shell                sleep                test_sync
tool_search          unified_exec         view_image
wait_for_environment
```

**没有 `read_file`。** 仓库里出现的 `read_file` 集中在 exec-server / 文件系统基础设施，不是模型工具。

因此 Codex 的导航链路只有一条：

```
shell / unified_exec  ──  rg / rg --files / cat / sed / nl / ls / wc / git log|blame|show
        ↓
   apply_patch
```

`codex-rs/file-search/` **不是模型工具** —— 它是 nucleo 模糊匹配器，消费方全部在 `tui/`（`tui/src/file_search.rs`、`chatwidget.rs`），服务于 TUI 的 `@` 文件提及补全。不要把它当成导航能力来对标。

---

## 2. Codex Search Strategy

提示词把搜索工具**指名道姓**写死（`core/gpt_5_2_prompt.md:250`，`gpt_5_codex_prompt.md:5`，`gpt-5.2-codex_prompt.md:5` 三份提示词一字不差）：

> "When searching for text or files, prefer using `rg` or `rg --files` respectively because `rg` is much faster than alternatives like `grep`. (If the `rg` command is not found, then use alternatives.)"

注意三个设计细节：

| 细节 | 作用 |
| --- | --- |
| 同时给出 `rg`（找文本）与 `rg --files`（找文件） | 覆盖"不知道内容在哪"和"不知道文件叫什么"两种未知 |
| 给出**理由**（"much faster"） | 模型在权衡时有依据，不是盲从 |
| 给出**降级路径**（"If the `rg` command is not found"） | 环境缺失时不会卡死 |

这条指令出现在**每一份**代号提示词的最前面（gpt_5_codex / gpt-5.2-codex 都在 `## General` 第一条）。搜索不是可选项，是默认入口。

---

## 3. Codex Read Strategy

Codex 没有"读取策略"这个概念，因为读取是 shell 命令的自然结果。真正约束读取形态的是**三条机制**：

### 3.1 输出 token 预算（`handlers/shell_spec.rs:55-57`）

```rust
"max_output_tokens".to_string(),
    "Output token budget. Defaults to 10000 tokens; larger requests may be capped by policy."
```

**这是最关键的结构性差异。** 模型执行 `cat huge_file.go` 得到的不是整个文件，而是 10k token 的预算内输出。想要更多必须显式抬预算。于是"先定位、再取相关片段"成了**省事的做法**，而不是需要自律的做法。

对照 CodeLeveler：`read_file` 单次上限 256 KiB（约 64k token）。给一个路径就能拿回 12k token 源码是**最省事的路径**，模型没有理由不这么做。C2.1 实测 ripgrep 的 `hiargs.rs` 一次读回 12,288 token，正是这个结构的产物。

### 3.2 截断量对模型可见（`handlers/shell_spec.rs:284-290`）

输出 schema 显式携带：

```
"original_token_count": "Approximate token count before output truncation."
"output":               "Command output text, possibly truncated."
```

模型知道自己拿到的是全部还是一部分、被砍了多少。截断是**有信息的**，不是静默的。

### 3.3 禁止绕过（`core/gpt_5_2_prompt.md:251`）

> "Do not use python scripts to attempt to output larger chunks of a file."

明确堵死"写个脚本把整文件 dump 出来"的旁路。预算是硬约束，不是建议。

---

## 4. Codex Evidence Expansion

### 4.1 并行读取（`core/gpt_5_2_prompt.md:252`）

> "Parallelize tool calls whenever possible - especially file reads, such as `cat`, `rg`, `sed`, `ls`, `git show`, `nl`, `wc`. Use `multi_tool_use.parallel` to parallelize tool calls and only this."

被点名的七个命令覆盖了完整的证据获取谱系：`cat`（整读小文件）、`sed`/`nl`（取区间、带行号）、`rg`（搜索）、`ls`（结构）、`git show`（历史版本）、`wc`（规模判断，决定该整读还是分段）。

**并行是策略而非优化**：一次发 5 个读取的成本 ≈ 一次发 1 个的延迟，于是"多看几个地方"变得廉价，影响面覆盖的门槛被降低。

### 4.2 历史作为一等证据来源（`core/gpt_5_2_prompt.md:128`）

> "Use `git log` and `git blame` to search the history of the codebase if additional context is required."

放在 coding guidelines 里，与"修根因""保持风格一致"并列 —— 历史调查是编码纪律的一部分，不是高级技巧。

**注意 Codex 也没有显式的 "check the callers" 指令。** 它的影响面覆盖是"搜索优先 + 并行廉价 + 历史可查"三者的**涌现结果**，不是一条被写下的规则。这是 CodeLeveler 可以做得更系统的地方（见 §9 IMPROVE）。

---

## 5. Codex Edit Transition

**没有任何门禁。** 三份提示词与 `handlers/plan.rs` / `plan_spec.rs` / `spec_plan.rs` 全文检索：**零个 `tool_choice` 强制、零个 plan gate、零个探索轮次上限。**

计划工具的全部约束是提示词里的四行建议（`gpt_5_codex_prompt.md:21-25`）：

> "When using the planning tool:
> - Skip using the planning tool for straightforward tasks (roughly the easiest 25%).
> - Do not make single-step plans.
> - When you made a plan, update it after having performed one of the sub-tasks that you shared on the plan."

**这与 CodeLeveler 的 `PLAN_EXPLORE_ROUNDS = 2` 形成最尖锐的对照**：CodeLeveler 在第 3 轮把请求的工具表裁剪到只剩 `update_plan` 并强制 `ToolChoice::Tool("update_plan")`（`drive.rs:461`），Codex 连一条硬约束都没有。

### 编辑后不回读（`core/gpt_5_2_prompt.md:130`）

> "Do not waste tokens by re-reading files after calling `apply_patch` on them. The tool call will fail if it didn't work. The same goes for making folders, deleting folders, etc."

把"工具失败即失败"这个契约明确告诉模型，从而消除防御性回读。CodeLeveler 有 `RepeatedReadGuard`，但它是**事后 nudge**（读了才提示），Codex 是**事前约定**（根本不读）。

---

## 6. Codex Verification Strategy

`core/gpt_5_2_prompt.md:140`：

> "When testing, your philosophy should be to start as specific as possible to the code you changed so that you can catch issues efficiently, then make your way to broader tests as you build confidence. If there's no test for the code you changed, and if the adjacent patterns in the codebases show that there's a logical place for you to add a test, you may do so. However, do not add tests to codebases with no tests."

纯提示词层。**CodeLeveler 在这一项上明确更强**：它有 `Verifier`、`ScopePolicy`（C1.2 显式验证权威）、验证沙箱（C1.3）、工具链隔离（C1.6）、repair 回合、隐藏验收。这一条**不要学 Codex**。

---

## 7. CodeLeveler vs Codex

| 维度 | Codex | CodeLeveler | 判断 |
| --- | --- | --- | --- |
| 定位入口 | 提示词首条点名 `rg` / `rg --files`，带理由与降级 | 一行 "Locate the relevant code with grep/list_files/read_file"，未点名任何符号工具 | **CodeLeveler 弱**：符号工具三次真实运行使用 0 次 |
| 读取工具 | 无；shell 组合 | 结构化 `read_file(path,start,end)` + 指纹 + 分页 + 二进制检测 + stale-write 保护 | **CodeLeveler 强**，不要放弃 |
| 读取预算 | shell 默认 **10k token**，超出需显式抬 | **256 KiB**（~64k token）单次上限 | **CodeLeveler 弱**：整读是最省事路径 |
| 截断可见性 | `original_token_count` 明示截断前规模 | 截断标记已带总行数与续读指引（C1 已做） | 持平 |
| 区间读取 | `sed -n '700,850p'` 是定位后的自然动作 | `start_line/end_line` 已支持，**但模型不用** | **能力有、行为无** |
| 并行读取 | 提示词明确要求，点名 7 个命令 | 有 parallel-safe 工具与并发批，但**未写进导航策略** | **可吸收** |
| 历史证据 | `git log` / `git blame` 写进 coding guidelines | shell 可做，非一等策略 | **可吸收** |
| 编辑后回读 | 事前约定"不要回读" | `RepeatedReadGuard` 事后 nudge | **可吸收（成本极低）** |
| 进入编辑 | **无门禁**，计划纯建议 | **2 轮探索后强制 plan + 裁剪工具表 + 强制 tool_choice** | **CodeLeveler 有害** |
| 影响面 | 无显式规则，靠搜索优先 + 并行廉价涌现 | 无任何规则，靠验证门禁事后兜底 | **两边都弱，CodeLeveler 有机会做得更好** |
| 验证 | 纯提示词"由窄及宽" | Verifier + ScopePolicy + 沙箱 + repair + 隐藏验收 | **CodeLeveler 明确更强，不要学** |

---

## 8. 实跑对照：Grok Build（同一 yq 任务）

Codex 二进制未安装（见 C2.3 Navigation Baseline §8），无法实跑。Grok Build 可用，同任务实跑结果：

| | CodeLeveler（yq，C1 基线） | **Grok Build** |
| --- | ---: | ---: |
| 首次 plan | **r≈3（被强制）** | **第 27 次调用（自主，在导航之后）** |
| plan 之前的连续导航调用 | **2 轮上限** | **26 次，零打断** |
| 首次编辑 | r21–23 | 第 28 次调用 |
| 修改文件数 | 6 | **6 改 + 3 新建（含它自写的测试与验收脚本）** |
| 隐藏验收 | PASS | **PASS**（本机实跑 eval 的 expect 脚本，逐条通过） |
| 工具形态 | read 26–32 / grep 5–7 | read_file 20 / grep 4 / list_dir 1 / shell 2 |

Grok 的导航序列（前 12 次）：

```
list_dir(.) → grep("flag|cobra|doc|help") → find *.go
→ read cmd/root.go → cmd/utils.go → cmd/constant.go → yq.go
→ acceptance_tests/flags.sh → examples/multiple_docs.yaml
→ cmd/evaluate_sequence_command.go → pkg/yqlib/stream_evaluator.go
→ grep("Decode|document|Count") → pkg/yqlib/decoder.go
```

**第 10 次调用时已读完 yq eval 定义的全部三个 `relevant_paths`。** 之后继续 16 次导航才开始动手 —— 它把影响面看完了，才写第一行代码。

值得注意：**Grok 也整文件读**（`target_file` 无区间）。所以"整读"本身不是病；**在不知道该看哪里的时候整读、且被迫在两轮后停止探索**才是病。

---

## 9. 结论：KEEP / ADOPT / IMPROVE / DO_NOT_COPY

### KEEP —— CodeLeveler 现有优势，不得为模仿 Codex 而放弃

| 项 | 理由 |
| --- | --- |
| **结构化 `read_file(path, start_line, end_line)`** | 跨平台一致、沙箱内可控、有整文件指纹（stale-write 保护）、二进制检测、分页与截断指引。shell `sed/cat` 全部不具备。**明确拒绝"Codex 用 shell 所以删掉 read_file"的推论** |
| **Verifier / ScopePolicy / 验证沙箱 / repair 回合** | 比 Codex 的纯提示词验证强一个量级，是 C1 49/49 的支柱 |
| **符号工具族**（`find_symbol`/`find_references`/`read_symbol`/`blast_radius`） | Codex 完全没有等价物；它们有非 LSP 降级路径，是 CodeLeveler 可以超越 Codex 的地方 |
| **durable runtime / 事件日志 / 恢复** | 与导航无关但不可为导航改动牺牲 |

### ADOPT —— 应吸收的行为策略（非实现）

| # | 内容 | 依据 |
| --- | --- | --- |
| A1 | **搜索优先纪律**：位置未知时先定位再读，且**点名具体工具**并给出理由与降级路径 | Codex 三份提示词首条 |
| A2 | **并行读取**作为导航策略写入提示词，而非仅作为引擎优化 | `gpt_5_2_prompt.md:252` |
| A3 | **`git log` / `git blame` 作为一等证据来源** | `gpt_5_2_prompt.md:128` |
| A4 | **编辑后不回读**的事前约定（"工具失败即失败"） | `gpt_5_2_prompt.md:130` |
| A5 | **验证由窄及宽** —— CodeLeveler 的 base prompt 已有等价表述，保持 | `gpt_5_2_prompt.md:140` |

### IMPROVE —— CodeLeveler 可以做得比 Codex 更系统

| # | 内容 | 为什么 CodeLeveler 有优势 |
| --- | --- | --- |
| I1 | **显式影响面纪律**（callers / consumers / interfaces / config / tests） | Codex 靠涌现，没写成规则。CodeLeveler 有 `find_references` 与 `blast_radius` 两个专用工具，可以把"改之前看影响面"变成可执行的一步，而不是靠模型自觉 |
| I2 | **Location Confidence 分级**（KNOWN / LIKELY / UNKNOWN） | Codex 无此概念，"总是先 rg"在用户已指名文件时是浪费。分级可以两头都不亏 |
| I3 | **Progressive Read**（先相关区间、证据不足再扩） | Codex 靠 10k 预算被动实现。CodeLeveler 的 `start_line/end_line` 更精确，可以主动表达 |
| I4 | **模型无关**：把导航纪律做进 Harness 而非依赖模型熟练使用 `rg/sed/git` | Codex 的导航质量强依赖模型的 shell 熟练度；CodeLeveler 的结构化工具让 DeepSeek/GLM/Grok/Claude 获得相近行为 |

### DO_NOT_COPY —— 明确不学

| # | 内容 | 理由 |
| --- | --- | --- |
| D1 | **shell-only 导航架构 / 删除 `read_file`** | 会同时丢掉跨平台一致性、沙箱可控性、指纹与 stale-write 保护、恢复期可重放性、可观测性。Codex 是因为没有结构化读取才这么做，不是因为它更好 |
| D2 | **把 `read_file` 硬性截到 N 行** | C2.3 §5 明确禁止。真正该改的是**默认行为与引导**，不是能力上限 |
| D3 | **纯提示词验证** | CodeLeveler 的 Verifier 明确更强 |
| D4 | **照搬 10k 硬预算** | Codex 的 10k 是 shell 通用预算；CodeLeveler 的 `read_file` 有精确区间能力，更适合"引导先窄读"而非"一刀切上限" |
| D5 | **`file-search` 作为导航能力对标** | 它是 TUI 的 `@` 补全器，不是模型工具 |

---

## 10. 对 C2.3 生产改动的直接输入

按本次源码对照，生产改动的优先级应为：

| 优先级 | 改动 | 对应 | 性质 |
| --- | --- | --- | --- |
| **1** | **放宽 / 重构强制计划闸门**（`PLAN_EXPLORE_ROUNDS = 2`） | Baseline G1；Codex 零门禁、Grok 26 次导航后才 plan | 引擎（最小改动） |
| **2** | **Navigation guidance 写入 base prompt**：搜索优先 + 点名符号工具 + Location Confidence + Progressive Read + 影响面 + 并行 + 编辑后不回读 | A1/A2/A4 + I1/I2/I3 + Baseline G2/G3/G4/G6 | 纯提示词 |
| **3** | 搜索结果携带更利于下一步窄读的定位信息 | Baseline G5 | 工具层，先看 1+2 的效果 |

**#1 必须先做**：无论提示词写得多好，模型在第 3 轮就会被物理拦截，任何导航纪律都没有执行它的回合数。
