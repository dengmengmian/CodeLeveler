# C2.3 — Code Navigation Baseline (Audit)

日期：2026-08-09。分支 `feat/coding-context-efficiency-c2`。**本文档为纯审计，未改动任何生产代码。**

数据来源：`prompts/base.md`、`executor/gates.rs`、`executor/drive.rs`、`executor/dispatch.rs`、`leveler-tools/src/tools/*`，加上三次真实仓运行的持久化事件日志（yq×2、ripgrep×1）。

---

## 0. 结论前置

CodeLeveler 当前**没有代码导航范式**。它有一整套能力齐备的导航工具，但：

1. **base prompt 里的导航指导只有一行**，且只点名了三个工具中最弱的组合。
2. **符号级工具在三次真实仓运行中使用次数为 0** —— 不是因为不可用，是因为没人告诉模型它们存在。
3. **强制计划闸门在 2 轮探索后掐断导航**，三次运行全部被它拦截过。
4. **全仓没有任何一处提到 caller / impact surface / consumers** —— 影响面扩展机制不存在。

Agent 的实际范式是：`grep 一两次 → 被迫产出 plan → 大范围 read → 边改边发现漏了什么`。

---

## 1. 当前 Locate 策略

`prompts/base.md` 中与定位相关的**全部**内容：

```
- Directories: use `list_files` (never `read_file` on a directory).
  Files: use `read_file` on a concrete file path.
- Read before you edit. Locate the relevant code with grep/list_files/read_file.
```

一行。它指定的三个工具里，`list_files` 和 `read_file` 根本不是定位工具 —— 真正的定位只剩 `grep`。

**从未被点名的定位工具**（全部已实现、已注册、可用）：

| 工具 | 能力 | LSP 依赖 | 真实运行中使用次数 |
| --- | --- | --- | ---: |
| `find_symbol` | 找符号**定义**位置 | 有 LSP 更精确，**无 LSP 降级为快速扫描** | **0** |
| `find_references` | 找符号**引用**位置 | 有 LSP 更精确，**无 LSP 降级为全词扫描** | **0** |
| `read_symbol` | 按符号读取而非按行 | — | **0** |
| `blast_radius` | 按跳数分组的影响面估计（工具描述里明写 "Use before a refactor"） | LSP | **0** |
| `locate_hint` | — | — | **0** |

**三次真实仓运行的导航工具分布**：

| Run | `read_file` | `grep` | `list_files` | 符号类工具 |
| --- | ---: | ---: | ---: | ---: |
| yq（C1 基线） | 26 | 7 | 1 | **0** |
| yq（C2.2 复跑） | 32 | 5 | 1 | **0** |
| ripgrep | 41 | 24 | 4 | **0** |

**关键**：这不是能力缺失。`find_symbol` 与 `find_references` 都有不依赖语言服务器的降级路径（分别是快速扫描与全词扫描），在 Go / Rust 仓里都能直接工作。它们从未被调用，唯一合理的解释是**提示词里没有它们**。

`blast_radius` 尤其讽刺 —— 它的工具描述已经写着 "Use before a refactor; use find_references for just the direct sites"，但没有任何上层引导会让模型走到那一步。

---

## 2. 当前 Read 策略

**没有策略。** 提示词对读取范围只字未提：不提 `start_line` / `end_line`，不提大文件先读相关区域，不提证据不足时扩读。

`read_file` 工具描述末尾有一句 "optional inclusive line range"，这是模型能看到的关于范围读取的**全部**信息。

后果，实测：

| Run | 读取次数 | 唯一路径 | 读取 token | 形态 |
| --- | ---: | ---: | ---: | --- |
| yq | 24 | 24 | 21,140 | 每个文件整读一次，无重复；成本来自**广度** |
| ripgrep | 39 | 25 | 72,657 | **4 次 ≥400 行的读取占全部读取 token 的 52%（37,755）** |

ripgrep 的具体形态（C2.1/C2.2 已逐条核实）：

| 路径 | 读取序列 | 后果 |
| --- | --- | --- |
| `crates/core/flags/hiargs.rs` | 1–1471（12,288 tok）→ 700–880 → 85–105 | 大范围那一读里 **~10k token 再没被回看** |
| `crates/core/flags/defs.rs` | 1–7070 of 7675（12,289 tok，撞 256KB 上限）→ 6400–6500 → 1217–1350 | 同上 |

即 **C2.1 已量化的"42.5% 的读取 token 是大范围读取中从未回看的部分"**。这是"先整读、后窄读"范式的直接代价，而这个范式正是提示词沉默的产物 —— 模型不知道还能先窄读再扩。

**注意**：这不等于"应该少读"。`defs.rs` 那次读取撞上了 256KB 上限却仍未覆盖全文，说明在真实大文件上"整读"既贵又**不完整**。问题是缺少 progressive read 纪律，不是读得多。

---

## 3. 当前 Impact Expansion 策略

**不存在。**

`prompts/base.md`、`executor/nudges.rs`、`executor/gates.rs` 三处全文检索 `caller` / `impact surface` / `consumers` / `before editing`：**零命中**。

也就是说：模型找到第一个实现点之后，**没有任何机制促使它继续检查影响面**。是否去查 caller、是否去看 trait 实现、是否去同步测试，完全取决于模型自发的习惯。

现有的相关能力全部是**被动的**：

| 机制 | 位置 | 性质 |
| --- | --- | --- |
| `blast_radius` 工具 | tools/blast_radius.rs | 模型必须自己想起来调用 |
| `find_references` 工具 | tools/find_references.rs | 同上 |
| 验证门禁 + repair 回合 | verifier | **事后**才发现漏改，靠编译/测试失败倒逼 |

当前唯一真正起作用的"影响面保障"是**验证门禁**。也就是说：CodeLeveler 目前靠"改错了再被测试打回来"来补影响面，而不是靠"改之前先看清"。C1 的高完成率有相当一部分是这条事后回路撑起来的 —— 它有效，但代价是轮次和 token，且在没有强测试的仓上会直接漏。

---

## 4. 当前进入 Edit 的条件

没有证据充分性判据。真正约束"何时开始改"的是两条**与证据无关**的机制：

### 4.1 强制计划闸门（`plan_gate` + `PLAN_EXPLORE_ROUNDS`）

```rust
// drive.rs:111
const PLAN_EXPLORE_ROUNDS: u32 = 2;
```

任务被判定为"多步骤"时：

1. 只读探索工具在**前 2 轮**放行；
2. 第 3 轮起，`plan_gate` 拒绝一切非豁免工具，返回 `"Explore budget used. Call update_plan..."`；
3. 更强的是 `drive.rs:461` —— 此时请求的**工具表被裁剪到只剩 `update_plan`**，并强制 `tool_choice = Tool("update_plan")`。模型在物理上无法继续导航。

**实测：三次真实仓运行全部撞到了这个闸门。**

| Run | `plan_gate` 拒绝 | `explore_budget` 拒绝 |
| --- | ---: | ---: |
| yq（C1） | 1 | **2** |
| yq（C2.2） | 1 | **2** |
| ripgrep | 1 | **1** |

被拒的是什么调用？C2.2 逐条核实过：`read_file(cmd/constant.go)`、`read_file(cmd/evaluate_sequence_command.go)`、`read_file(crates/core/flags/defs.rs)` —— **全部是真实的导航读取**。

> 附带影响：这些 29 token 的拒绝消息在事件日志里长得和读取结果一样，正是它们把我在 C2.1 的冗余分析带偏，让"整文件读了两次"的假象成立。

**这是当前最严重的结构性缺陷。** 在一个 477 文件的陌生 Go 仓里，2 轮只读探索连"相关实现在哪里"都回答不了，模型却被强制在此时产出计划。之后它只能一边执行一个基于薄弱证据的计划、一边补探索 —— 这正好解释了 C1.4 起就反复观察到的签名：

| Run | first relevant | first edit | 间隔 |
| --- | ---: | ---: | ---: |
| yq | r2 | r23 | 21 轮 |
| ripgrep | r3 | r45 | **42 轮** |

"定位极早、承诺编辑极晚"从来不是模型犹豫，是**它在计划闸门之后才被允许真正把影响面看完**。

### 4.2 连续搜索上限（`search_budget_gate`）

`max_search_calls_per_step` 默认 **0 = 关闭**（`policy_resolver.rs:116`），本批运行中未生效。记录在此以免误判 —— 它**不是**当前的瓶颈。

---

## 5. 为什么模型倾向"找到候选文件 → 直接大范围 read"

四个原因叠加，全部是我们自己造成的：

1. **提示词只给了 `grep`**。grep 返回 `path:line:text`，模型拿到 `path` 之后，工具箱里被点名的下一步只有 `read_file(path)` —— 没有范围指导，最省事的就是整读。
2. **符号工具不存在于模型的认知里**。`find_symbol` 本可以直接给出定义位置从而支撑窄读，但它没被提及。
3. **探索预算只有 2 轮**。在预算即将耗尽时，"多读一点以防万一"是理性选择 —— 闸门本身在**奖励**大范围读取。
4. **grep 结果不包含足以支撑窄读的定位信息**（工具描述：`Returns matching lines as path:line:text`）。没有符号边界、没有所属函数、没有建议区间，模型无从判断该读哪一段。

---

## 6. 明显缺口清单

| # | 缺口 | 证据 | 严重度 |
| --- | --- | --- | --- |
| **G1** | 探索预算 2 轮，耗尽后工具表被裁到只剩 `update_plan` | 三次运行全部撞到；被拒的都是真实导航读取 | **高** |
| **G2** | 提示词无影响面纪律，全仓零处提及 caller/consumers | 三处文件全文检索零命中；靠验证门禁事后兜底 | **高** |
| **G3** | 符号级工具零使用（`find_symbol`/`find_references`/`read_symbol`/`blast_radius`） | 三次运行合计 0 次调用；且它们都有非 LSP 降级路径 | **高** |
| **G4** | 无 progressive read 纪律 | ripgrep 52% 读取 token 来自 4 次大范围读取，其中 42.5% 从未回看 | 中 |
| **G5** | 搜索结果不携带支撑下一步窄读的定位信息 | `grep` 只返回 `path:line:text` | 中 |
| **G6** | 无 Location Confidence 概念 | 已知路径与未知路径走同一条无指导流程 | 中 |

---

## 7. 与 §12 候选改动的对应

| §12 候选 | 对应缺口 | 优先级判断 |
| --- | --- | --- |
| A. Agent base navigation guidance | G2 / G3 / G4 / G6 | **最高**：一处改动同时覆盖四个缺口，且是纯提示词改动 |
| E. 进入 edit 前的 impact-awareness guidance | G2 | 高 |
| D. `find_symbol` / `find_references` 使用策略 | G3 | 高（与 A 同批） |
| F. 定位完成后的 exploration convergence | G1 | **需先解 G1**：当前不是"探索太久"，是"探索被掐断" |
| C. 搜索结果增加定位信息 | G5 | 中（工具层改动，先看 A 的效果） |
| B. 工具描述改进 | G3 / G4 | 中 |

**G1 不在 §12 的候选列表里，但它是最硬的一条**：无论提示词怎么写，模型在第 3 轮就会被物理拦截。任何导航纪律都必须先有执行它的回合数。

---

## 8. 外部对照可用性（§8）

| 产品 | 状态 |
| --- | --- |
| **Codex** | **NOT AVAILABLE** —— `~/.codex/` 配置目录存在，但二进制不在 PATH，也不在 `/opt/homebrew/bin`、`/usr/local/bin`、`~/.local/bin`、`~/.bun/bin` 或 npm 全局包中。不猜测其行为 |
| **Grok Build** | 可用（`~/.grok/bin/grok` v1.0.0），支持无头模式 `-p --output-format streaming-json`，可导出工具调用序列 |

Grok Build 的同任务对照观察见最终报告。

---

## 9. 本审计未做的事

- 未改动任何生产代码。
- 未修改任何 eval case。
- 未运行外部产品（对照实验在后续步骤进行）。
