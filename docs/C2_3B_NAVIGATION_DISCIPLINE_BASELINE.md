# C2.3B — Navigation Discipline Baseline (Audit)

日期：2026-08-09。分支 `feat/coding-context-efficiency-c2`，基线 HEAD `7bb1647`（C2.3A 已合入）。**纯审计，未改动生产代码。**

审计对象：`crates/leveler-agent/prompts/base.md`（130 行）、`crates/leveler-agent/src/prompt.rs`（组装顺序与条件注入）、`executor/drive.rs`（C2.3A soft nudge）、六个导航工具的 `description()`。

---

## 1. 生产注入点（§17 的答案）

```
PromptBuilder::build()                       prompt.rs:140
 ├─ base_instructions ?? BASE_PROMPT         ← prompts/base.md（include_str!）
 ├─ "## Project memory index"（有记忆时）
 ├─ turn_context.render()  ├─ operating_rules
 │                         └─ "Project rules:"（AGENTS.md）
 ├─ "Progress narration: …"（无条件）
 └─ if require_explicit_plan:
      ├─ "Before calling any tool, decide if this is multi-step work …"
      └─ "For tasks where you edit files, do NOT declare … until build/tests pass"
```

**唯一符合 §17 要求（harness baseline、模型无关、非 eval-only、非 provider-specific）的注入位置是 `prompts/base.md`。**

- `base_instructions` 是 per-model profile 覆盖，写在那里只对单个模型生效 —— 排除。
- `turn_context` / `operating_rules` 承载权限与环境事实，不是行为纪律 —— 排除。
- `require_explicit_plan` 块是条件注入（`TurnPolicy::minimal` 下不生效）—— 导航纪律不能挂在计划开关上 —— 排除。

结论：**导航纪律进 `base.md`**；`require_explicit_plan` 块里与之冲突的措辞需要就地修正（见 §3）。

---

## 2. 四项纪律现状

| 纪律 | 现状 | 依据 |
| --- | --- | --- |
| **Search First** | **CONFLICTING** | `base.md:9` 只有一行且不含优先级；`prompt.rs:180` 把探索称作 "optional"、把 plan 称作 "first substantive action" |
| **Progressive Read** | **MISSING** | 全文零处提及 `start_line` / `end_line` / 区间 / 先窄后扩 |
| **Impact Expansion** | **MISSING** | `base.md` + `nudges.rs` + `gates.rs` 检索 `caller`/`consumer`/`impact`：零命中 |
| **Exploration Convergence** | **MISSING** | 无任何基于信息增量的停止条件 |

### 2.1 Search First — CONFLICTING

`base.md:9` 全文：

> `- Read before you edit. Locate the relevant code with grep/list_files/read_file.`

三个问题：

1. **没有优先级**。三个工具并列，其中 `list_files` 与 `read_file` 不是定位工具。
2. **符号工具缺席**。`find_symbol` / `find_references` / `read_symbol` / `blast_radius` 从未被点名 —— C2.3 baseline 实测三次真实仓运行**调用次数为 0**。
3. **门槛极低**。"Read before you edit" 一次读取即满足。

`base.md:8` 进一步把读取引向整读：

> `- Directories: use list_files … Files: use read_file on a concrete file path.`

模型能看到的关于"怎么读"的唯一指导就是"给一个具体路径"。**这正是无目的 broad read 的直接诱因。**

### 2.2 Progressive Read — MISSING

`read_file` 的 `description()` 末尾有 "optional inclusive line range"，这是区间读取在模型认知里的全部存在感。没有任何指导说明何时该用、何时该扩。

C2.3 已量化后果：ripgrep 最终请求中 **4 次 ≥400 行的读取占全部读取 token 的 52%**，其中 **42.5% 的读取 token 属于大范围读取里模型再没回看的部分**。

### 2.3 Impact Expansion — MISSING

零指导。当前唯一起作用的影响面保障是**验证门禁事后倒逼**：改错 → 编译/测试失败 → repair 回合。有效，但代价是轮次，且在弱测试仓上直接漏。

### 2.4 Exploration Convergence — MISSING

`base.md` 里两条**看似相关但都不是**的规则：

| 行 | 内容 | 为什么不是收敛纪律 |
| --- | --- | --- |
| 22 | "do not repeat a failing approach: if the same tool call fails the same way twice…" | 针对**失败重试**，不针对成功但零信息增量的导航 |
| 49 | "Once the request is fully handled, STOP. Do not re-open earlier questions or re-run exploratory tools for a ceremonial audit." | 针对**任务完成后**，不针对编辑前的探索阶段 |

也就是说：模型知道"改完了别再瞎看"，但不知道"证据够了就该动手"。

---

## 3. 冲突指导清单（§21 要求）

| # | 位置 | 原文 | 可能导致 |
| --- | --- | --- | --- |
| **C1** | `prompt.rs:180` | "Before calling any tool, decide if this is multi-step work … after **optional** read-only explore, your **first substantive action** must be `update_plan`" | **premature edit / premature plan**。把导航贬为 "optional" 的前奏、把 plan 抬为"第一个实质动作"。C2.3A 已在**引擎侧**拆掉硬墙，但**提示词侧的同一压力仍在** |
| **C2** | `base.md:8` | "Files: use `read_file` on a concrete file path" | **broad read before localization**。唯一的读取指导就是"给路径"，没有区间概念 |
| **C3** | `base.md:9` | "Read before you edit." | **premature edit**。一次读取即满足，无影响面要求 |
| **C4** | `base.md:47` | "When unsure whether a recollection is still current, **re-read before asserting**." | **repeated confirmation**。本意是防止凭记忆断言（正确），但缺少 §15 的对偶条款（"patch 成功后不要为确认而回读"），实际鼓励了防御性重读 |
| **C5** | 全局缺失 | — | **caller omission**、**endless exploration** 两种失败模式**没有任何指导覆盖** |

**C1 是最关键的一条**：C2.3A 删掉了 `tools=[update_plan]` 与 forced `ToolChoice`，但提示词仍在说"探索是可选的、计划才是实质动作"。引擎已经放行，提示词还在踩刹车。

---

## 4. 工具描述审计（§19）

| 工具 | 现有描述要点 | 缺口 |
| --- | --- | --- |
| `grep` | "Search files … Returns matching lines as `path:line:text`. Uses ripgrep when available." | 未说明它是**位置未知时的入口**；返回的 `line` 未被引导用于后续窄读 |
| `find_symbol` | "Find where a symbol is DEFINED … Complements `grep`, which matches every mention." | 描述本身**已经很好**（明确了与 grep 的分工）。问题不在描述，在 base prompt 从不点名它 |
| `find_references` | 找引用位置 | 同上；且未被关联到"影响面" |
| `read_symbol` | 按符号读取 | 未被关联到"已知位置时的精确读取" |
| `find_files` | 按名字找文件 | — |
| `read_file` | "…Returns content with 1-based line numbers; **optional inclusive line range**." | 区间能力被放在最末尾一个从句里，没有"何时用"的语义 |
| `blast_radius` | "…grouped by hop distance … **Use before a refactor**; use `find_references` for just the direct sites." | 描述已含影响面语义，但**没有上层引导会让模型走到这一步** |

**结论：工具描述基本合格，缺的是 base prompt 的调度纪律。** 只需对 `grep` 与 `read_file` 做小范围澄清（§19 允许），不改 schema、不加省 token 措辞、不暗示"永远不要整读"。

---

## 5. C2.3A 之后仍然存在的行为压力

| 压力源 | 状态 |
| --- | --- |
| 引擎：探索 N 轮后裁剪工具表 | ✅ C2.3A 已删除 |
| 引擎：forced `ToolChoice = update_plan` | ✅ C2.3A 已删除 |
| 引擎：`plan_gate` 阻断导航工具 | ✅ C2.3A 已改为无条件放行 |
| **提示词：探索 = "optional"、plan = "first substantive action"** | ❌ **仍在**（C1） |
| **提示词：无区间读取纪律** | ❌ **仍在**（C2） |
| **提示词：无影响面纪律** | ❌ **仍在**（C5） |
| **提示词：无收敛纪律** | ❌ **仍在**（C5） |

C2.3A 拆掉了墙，C2.3B 需要补上路。

---

## 6. 本轮生产改动范围（据此确定）

| # | 改动 | 对应 | 类型 |
| --- | --- | --- | --- |
| 1 | `base.md` 新增 `## Code navigation` 段：Location Confidence / Search First / Progressive Read / Impact Surface / Convergence / 编辑后不回读 / 并行导航 / 验证失败后重定位 | §4–§16、§33 | 提示词 |
| 2 | `prompt.rs` 修正 C1 的措辞（不恢复任何强制，不改 C2.3A 语义） | C1 | 提示词 |
| 3 | `grep` / `read_file` 描述小范围澄清 | §19 | 工具描述 |

**不做**（§20 明令）：不动 C2.3A plan gate、不恢复 forced `update_plan`、不 cap `read_file` 行数、不改 `MAX_BYTES`、不做 compaction / supersession / estimator / progressive disclosure / repo graph / RAG / verifier / runtime / provider 专属 hack。

**不做**（§22）：不建 N1–N8 平行 benchmark，复用现有 eval framework 与 C2.3 已落地的 coverage 指标。
