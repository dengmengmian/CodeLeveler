# EDIT_FAILURE_RECOVERY_AUDIT

Audit-only（2026-08-07，分支 `feat/coding-real-task-completion-c1`）。来源：真实 dogfooding——一个约 8 行的 Markdown 文件，改正文开头一句 + 改文件名，Agent 连续失败两次（hunk mismatch → read → malformed patch）才完成。本文全部结论逐条对应真实代码位置；本轮零生产代码改动。

---

## 1. Incident Reconstruction

| 步骤 | 现象 | 代码路径 | 失败类别 |
| --- | --- | --- | --- |
| 1 | `✗ failed to apply hunk` | `patch/apply.rs:147 hunk_not_found` | **B：semantic/context mismatch**（合法 patch，old/context 行在文件里不存在） |
| 2 | `✓ 读取文件`，发现"标题只在文件名里，不在内容里" | `read_file` | 恢复动作正确——错误信息里的 "Read the file before patching it" 起了作用 |
| 3 | `✗ invalid patch: line 5: update hunk line must start with a marker` | `patch/parse.rs:219` | **A：patch DSL syntax error**（与文件内容无关，parse 先于任何 IO） |

为什么会发生（三个独立原因叠加）：

1. **第一次失败是模型凭空假设**：把文件名里的标题当成了正文第一行，没读文件就构造 hunk。staleness 守卫（`apply_patch.rs:337`）只能拦"读过之后文件变了"，拦不住"从来没读过"。all-or-nothing 保证了这只是一次响亮失败而非静默错改——机制正确。
2. **任务里的"改文件名"把选择进一步推向 apply_patch**：只有 apply_patch 有 `*** Move to:`；`replace` 不能改名。合并成一个动作时 apply_patch 是唯一选项。
3. **Markdown 是 marker DSL 的病态内容类**：hunk 每行首字符是语义位（`' '`/`-`/`+`），而 Markdown 正文天然有裸行和以 `-` 开头的 bullet。上下文行 `- item` 忘写前导空格会被解析成"删除 ` item`"（语义翻转，随后表现为 mismatch）；裸正文行直接 parse error——这正是第二次失败的形态。在 prose 文件上重建 patch，一个空格写错就再失败一次。

---

## 2. Current Edit Tool Selection

模型看到的全部选择依据（按注入顺序）：

| 来源 | 内容 | 效果 |
| --- | --- | --- |
| `crates/leveler-agent/prompts/base.md:17` | "Make changes **ONLY via the apply_patch tool**… Do NOT edit files with shell commands" | 头条规则，把 apply_patch 定为默认编辑工具 |
| `base.md:18` | "For a **rename or any repeated find-and-replace**, use the replace tool with replace_all=true… Use apply_patch for structural edits" | 把 replace 框定为"改名/批量替换"工具 |
| `replace.rs DESCRIPTION` | "Use this only for a literal rename **or exact text copied verbatim from a recent read**" | 其实覆盖本次场景，但被 base.md 的头条规则压制 |
| `apply_patch.rs DESCRIPTION` | 完整 DSL 语法 + 示例 | 无选择性指引 |
| executor/drive | 只在 plan-repair 时强制 `update_plan`（drive.rs:468）；对编辑工具选择零干预 | — |

**结论：不存在 "single exact replacement → replace / structural edit → apply_patch" 的明确策略。** base.md 的 "ONLY via apply_patch" + replace 被窄化为 rename 工具，使"8 行 Markdown 只替换一句"顺理成章地走了 apply_patch。GAP。

---

## 3. Failure Path

**Patch Parse Failure（第二次失败）**：

```
ApplyPatchTool::execute (apply_patch.rs:250)
→ parse_patch(&input.patch)            parse.rs:73
→ hunk 行首字符 ∉ {' ','-','+','@@'}   parse.rs:195-224
→ PatchError::Parse("line N: update hunk line must start with a marker — … {行内容:?}")
→ ToolOutput::error("invalid patch: …")  apply_patch.rs:254
```

要点：parse 先于一切文件 IO——**此时没读过文件、文件也没变**。错误信息带行号 + 原文 + marker 规则，但没有说"这是语法错、文件没问题、不必重新探索"。

**Hunk Match Failure（第一次失败）**：

```
execute → parse OK → workspace.resolve → read_to_string
→ file_state.is_stale 守卫              apply_patch.rs:337（本次未触发）
→ apply_update(&existing, &chunks)      apply.rs:9
→ seek_sequence(old_lines) 找不到       apply.rs:68
→ retry_without_trailing_blank 兜底     apply.rs:127
→ hunk_not_found                        apply.rs:147
→ "failed to apply hunk to {path}: could not find expected lines (searched from line N):
   <expected lines>
   + real_text_at_anchor 命中 → 真实文件文本（≤3 处，可逐字复制）
   | 未命中 → 'None of those lines exist in the file. Read the file before patching it.' + 文件摘录"
```

---

## 4. Recovery Behavior Today

**错误如何返回模型**：`ToolOutput { content: String, is_error: true, metadata: Null }`（tool.rs:206）→ `dispatch_raw`（host.rs:509）原样进 tool result。证据质量本身不差（mismatch 给真实文本，parse 给行号原文），但**类别只存在于散文里**。

**drive loop 对连续编辑失败的处理**（逐个机制核对）：

| 机制 | 位置 | 对本模式是否生效 |
| --- | --- | --- |
| loop_guard | gates.rs:91 | ✗ 需要**同 tool+同 args+同结果 ×3**；每次 patch 内容都不同 → 永不触发 |
| 搜索预算 | gates.rs:74 | ✗ 只管 search 类工具 |
| ObserveThrash | drive.rs:2062 | ✗ 只管纯读轮 |
| AllRefused | drive.rs:2096 | ✗ 且注释明说 "executed-but-failed tools still count as progress" |
| stagnation 守卫 | drive.rs:2122 | ✗ 只在 `command_ran_this_round` 时累加；"Rounds with no command are neutral (goal/plan/**edit** work is never penalized)" |
| round 上限 | MAX_TURN_ROUNDS=100 / case max_rounds | ✓ 唯一的最终闸 |

**回答第 4 节问题**：`apply_patch mismatch → read_file → apply_patch malformed → apply_patch mismatch` 这个序列，当前**没有任何机制会主动干预**——既不是主动恢复，也到不了 loop guard（args 一直在变），只能靠 round 预算硬停或模型自己走出来。本次事故里是模型第三次自己走出来的。这是一类**无守卫的 thrash**。

**工具选择是否随失败类型变化**：否。没有任何代码或提示在编辑失败后建议换 `replace`。

---

## 5. Root Causes（按影响排序）

1. **工具选择策略缺失**（base.md:17-18）：replace 被窄化为 rename/批量工具，"精确单点替换 → replace" 不存在；prose/markdown 文件上这直接把最不适配的 DSL 推给了最病态的内容类。
2. **失败类型不进入恢复决策**：parse / match / stale / commit 四类失败全部折叠为 `ToolOutput::error(String)`（metadata 恒为 Null，tool.rs:221）——模型只能靠读散文分类，harness（eval_signals.rs:146）也只能数无类型的 `edit_failures`。parse error 后模型重新 read_file 探索，正是上一条错误信息 "Read the file before patching it" 的过度泛化——没有反向提示"语法错不必重读"。
3. **编辑失败 thrash 无守卫**（第 4 节表）：设计上"编辑轮不惩罚"是为保护正常 fail→fix 迭代，但代价是变着花样连续失败的编辑序列不喂任何 streak。
4. **marker DSL 与 Markdown 内容类碰撞**（第 1 节）：不是 parser 的错——严格是对的——但没有任何指引说"prose 文件优先 replace"。
5. **Eval 零覆盖**（第 7 节）：以上任何一条退化都测不出来。

（对照 prompt 里的示例清单：确认 "wrong tool choice / no failure-type-aware recovery / no deterministic replace fallback / insufficient eval coverage" 四条属实；"weak patch syntax adherence" 属模型侧固有，宜靠选择策略绕开而非放宽 parser。）

---

## 6. Minimal C1 Fix Proposal（只提案，不实现）

优先级从上到下，全部不改 patch 语义、不放宽 parser：

**P1 — 工具选择指引（base.md + 两个 tool DESCRIPTION，纯文案）**
- base.md:17-18 改写为明确策略：
  - 单点精确替换（old 文本可从近期 read 逐字复制），尤其 prose/Markdown → **replace**；
  - 结构性修改（多行重塑/多 hunk/Add/Delete/Move）→ **apply_patch**；
  - rename/批量替换 → replace + replace_all（保留现规则）；
  - shell 改文件仍然禁止。
- 锁定方式：沿用现有 prompt 断言测试风格（prompt.rs:323）。

**P2 — 结构化失败提示（只改错误字符串结尾，parser 行为不动）**
- parse error 尾部追加："This is a patch **syntax** error — the file was not read and has not changed. Fix the patch text and resend; do not re-read the file."
- hunk mismatch 尾部追加："If the change is a single exact substitution, call `replace` with the exact text shown above."
- 效果：失败类型第一次变成模型可见的**行动指令**，成本一行文案。

**P3 — 错误类型元数据（ToolOutput.metadata，模型不可见，机器可见）**
- `apply_patch`/`replace` 的错误路径填 `metadata: {"failure_kind": "parse"|"match"|"stale"|"commit"}`（成功路径已有 metadata 先例：`modified_files`）。
- 消费方：eval_signals 可分型统计 edit_failures；为将来 drive 层类型感知恢复留缝。不改变模型可见内容。

**P4 —（缓做）drive 层重复失败纠偏**
- 同一 path 连续 ≥2 次编辑工具 is_error → 注入一次性纠偏 nudge（有 P3 则按类型给建议）。只 nudge 不硬停，不动现有 progress 语义。先落 P1/P2 并用 eval 度量，不够再做 P4。

**明确不做**：放宽 parser / 模糊匹配 / 自动猜 marker（Patch failure 比 silent wrong edit 安全）；silent auto-fallback 到 replace（工具语义要显式）；重构 ToolHost。

---

## 7. Eval Cases Needed（C1.1b 落，本轮不实现）

现状：38 个自动 case 中**六类场景零覆盖**（`recovery-compile-fail` 只覆盖编译失败恢复；eval_signals 已有 edit_attempts/edit_failures 信号和 Editing 归因，是"有仪表、无考题"）：

| # | 缺失场景 | 建议 case 形态 |
| --- | --- | --- |
| 1 | malformed apply_patch 后不重读、只修语法 | scripted-model 确定性 drive 测试（真模型无法确定性诱发） |
| 2 | hunk mismatch 自恢复 | 近重复上下文/typographic 标点的对抗性文件，真模型 case |
| 3 | read-after-failure 用上刚读到的文本 | 同上，断言 edit_failures ≤ N 且最终 passed |
| 4 | fallback 到 replace | **dogfood 镜像 case：Markdown 单句修改 + 文件改名**，度量编辑失败次数 |
| 5 | repeated edit failure 有界 | 断言 per-case edit-failure streak 上限 |
| 6 | successful self-recovery 计数 | 报表补 `passed && edit_failures>0` 的恢复率指标 |

---

## 8. Scope Decision

**C1 completion reliability。** 两个工具的能力（精确替换、结构 patch、Move、CAS、staleness、fuzzy 容错）都已存在且实测行为正确；缺的是**可靠选择 + 失败类型感知的恢复**——这正是 completion rate 的直接杠杆，不是 C3 工具扩展，不推迟。
