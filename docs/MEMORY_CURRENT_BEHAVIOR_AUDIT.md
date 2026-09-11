# Memory 当前行为审计

审计基线 `BASE_HEAD=0fe03710fa82ae350edb7ec8e096bc39325aed82`（= `origin/main`）。
`WORKTREE_CLEAN=NO`：审计开始时工作树有 26 个修改文件，分属两条并行工作
（本次 TUI presentation closure 的 8 个 `leveler-tui` 文件，以及另一条
verification/eval 线的 18 个文件）。本审计**没有修改任何已有文件**。

本文档只记录当前 HEAD 的真实行为，不提出实现。每条结论都标注证据来源：
代码位置、真实运行观测，或"仅凭阅读无法确认"。

---

## 1. 审计时的存量数据

在 dogfood 测试仓库（`scratchpad/dogfood3`）上观测：

```
CURRENT_MEMORY_ROOT=~/.leveler/state/projects/<project-hash>/memory
                    （active/ + pending/ + archive/，见 layout.memory_dir()）
CURRENT_MEMORY_ACTIVE_COUNT=5
CURRENT_MEMORY_PENDING_COUNT=0
CURRENT_MEMORY_ARCHIVED_COUNT=0
```

本仓库自身（`codeleveler`）的 store 为 `active=0 pending=0 archived=0`，
即这个功能在本项目的真实开发中从未积累过数据。

---

## 2. 已证实缺陷

### D1. 用户显式"记住"只进 pending，产生时不通知任何人

`parse_explicit_remember_intent`（`leveler-memory/src/candidates.rs:149`）能识别
`记住：…` / `请记住…` / `remember: …` / `always use …` / `以后都…`，产生
`CandidateKind::Preference` + `CandidateSource::UserExplicit` 的候选。

但候选进入 `MemoryStore::propose`，结果是 `ProposeOutcome::Pending`
（`pipeline.rs:45`；`pipeline_tests::accept_explicit_intent_then_search_and_index_hit`
明确断言 `list_active()==0 && list_pending()==1`）。

而 `Application::enqueue_memory_candidates`（`leveler-app/src/lib.rs:725`）在产生
pending 之后**只写一条日志**：

```rust
if pending > 0 {
    tracing::info!(pending, "enqueued memory candidates (await user accept)");
}
```

没有 `RuntimeEvent`，没有 client-protocol 通知，TUI 与 Web 都不会知道。

**采纳入口是存在的**，这一点不能说过头：

| 面 | 现状 |
| --- | --- |
| `ClientCommand::AcceptMemory` | 存在（`command.rs:128`，注释明确它就是 K36 要求的同意，永不可被模型调用） |
| `RuntimeEvent::MemoryList` | 已含 `pending: Vec<UiMemoryEntry>`（`event.rs:323`），注释写明"没有它 TUI 就无法显示有东西在等" |
| `ListMemory` handler | 已调用 `pending_entries(&store)` 并一起下发（`interactive.rs:1733`） |
| TUI | `/memory` 列出 active + pending，`/memory accept <id>`、`/memory forget <id>` 均已实现（`reducer/submit.rs:685`） |
| CLI | `leveler memory pending / accept / reject` 均已实现 |

**因此真正缺的是主动推送，不是入口**：候选产生的那一刻没有任何提示，
用户必须自己想到去敲 `/memory` 才会发现有东西待确认。

**后果**：用户说"记住 X"，系统确实存了一个候选，但当场毫无反馈，
看起来就像什么都没发生。这是"用户感觉没生效"的第一根因。

**对改动范围的影响**：修这一条**不需要协议变更** —— `MemoryList` 与
`AcceptMemory` 都已就位，只需在 `enqueue_memory_candidates` 产生 pending 时
发一个已有的 `RuntimeEvent::Notification`。

### D2. AutoApprove 直接批准了模型的 `remember` 写入

`prompts/base.md:46` 声明 "Project memory is consent-gated: `remember` raises an
approval prompt"。`executor/host.rs:291` 也为 `DeniedUnattended` 准备了把提案
park 到 `pending/` 的路径。

但真实运行（`leveler run --permission full-access --auto-approve`）观测到：

```
→ remember {"body":"本项目所有金额展示必须通过 formatAmount 工具函数渲染…","title":"金额渲染一律用 formatAmount"}
  ✓ remember: Remembered [formatamount]
```

写入直接成功进入 `active/`，`leveler memory list` 随后确认 `active` 增加一条。
park 路径没有被走到，因为 AutoApprove 在它之前就返回了 `Allowed`。

**后果**：无人值守运行可以在没有任何人同意的情况下写入 durable memory。

### D3. capability gating 有两个 owner

`leveler-app/src/lib.rs:630` 起的组合根里：

```rust
let mut registry = model_surface(
    self.exposed_capabilities(work_profile, model).await,  // gated
    &capabilities,
);
…
let memory_index = load_memory_index(&self.layout.memory_dir());  // NOT gated
…
memory_index,                                                      // NOT gated
memory_root: Some(self.layout.memory_dir()),                       // NOT gated
```

`exposed_capabilities` = selection ∩ availability，其中
`capability_selection(Economy) == CapabilityPacks::NONE`（`lib.rs:546`），
所以 Economy 下 memory **工具**不注册。但 `memory_index` 与 `memory_root`
绕过了这个判断，无条件传进 `ExecutorFactory`。

`Executor::relevant_memory_injection`（`leveler-agent/src/executor.rs:1555`）
只读 `self.memory_root`，不查任何 capability：

```rust
let root = self.memory_root.as_ref()?;
let store = MemoryStore::open(root).ok()?;
let hits = store.search(request, RECALL_K).ok()?;
```

**真实验证**：在 5 条 active memory 的仓库上跑一轮
`leveler run --work-mode economy`，从持久化的 `context_snapshot` 取出真实
prompt，检测结果：

| 探针 | Economy 轮次中是否存在 |
| --- | --- |
| `Project memory index` | 是 |
| `Relevant memory (retrieved for this turn)` | 是 |
| 记忆正文（`状态色约定` / `搜索框过滤范围` / `formatAmount`） | 是 |
| `## Memory` guidance 整节 | 是 |

契约 `EXPOSED_MEMORY = SELECTED ∩ AVAILABLE` 因此只覆盖了工具暴露，没有覆盖
index 注入、recall root 与 recall 注入。

### D4. `## Memory` prompt 一节完全不 gate

该节硬编码在 `crates/leveler-agent/prompts/base.md:44`，随 `BASE_PROMPT`
无条件进入每一轮。Economy 下工具没注册，而 prompt 仍在教模型
"`remember` raises an approval prompt … propose it"，即指示模型调用一个
它拿不到的工具。

### D5. recall 只有一条词法 lane，没有 standing preference

`executor.rs:44` 的常量与注释说明当前只有 query-conditioned 注入：

```
RECALL_K = 4
RECALL_FLOOR = 0.1
RECALL_CHAR_BUDGET = 1500
```

`relevant_memory_injection` 用的是 `store.search`（BM25 + CJK bigram），
注释明确 "Uses the real BM25 `search` (not the pseudo-vector path)"，
即 `vector_search` **不参与**自动召回。

**真实验证**：已保存 active preference
「用户偏好紧凑的终端信息输出，不希望看到冗长的模型过程。」，
用 `leveler memory search` 观测词法命中：

| query | 命中 | 说明 |
| --- | --- | --- |
| `这个终端为什么看起来这么啰嗦？` | 1.532 | CJK bigram 靠「终端」重合 |
| `界面信息太多了怎么收敛` | 1.532 | 靠「信息」重合 |
| `输出太冗长` | 2.820 | 靠「输出」「冗」重合 |
| `能不能精简一点` | 0 | 无共同字 |
| `别唠叨` | 0 | 无共同字 |
| `少说废话` | 0 | 无共同字 |
| `把 UI 弄干净些` | 0 | 无共同字 |
| `verbose terminal output` | 0 | 跨语言 |
| `terse` | 0 | 跨语言 |

结论需要比"BM25 会漏掉 paraphrase"更精确：**中文 query 对中文记忆，
bigram 让相当多的 paraphrase 仍能命中**；真正稳定漏掉的是
(a) 无共同字的同义表达，(b) 跨语言 query。

长期偏好这类记忆的适用性本来就不取决于本轮 query 的用词，所以它依赖
词法命中这件事本身是设计缺口，而不是分数调参问题。

### D6. recall block 不带 memory id

`render_recall_block`（`executor.rs:60`）只输出 `- {title}: {body}`。真实
prompt 中确认：

```
## Relevant memory (retrieved for this turn)
These were retrieved by relevance to the current request. …
- 搜索框过滤范围: 表格搜索框只按 payer 字段过滤，大小写不敏感并 trim。…
- 状态色约定: 本项目状态色一律在本文件内用模块级 STATUS_COLOR 映射表实现，…
```

没有 entry id，因此无法机械证明某一轮引用了哪一条记忆，也没有
`omitted_count` 之类的诚实缺省计数。

### D7. repository-derived fact 仍在被写成 Memory

`collect_turn_candidates`（`pipeline.rs:217`）在每一轮同时调用：

```rust
store.propose_from_user_text(user_text)?      // 用户显式意图
store.propose_package_manager(root)?          // 仓库派生事实
```

`detect_package_manager`（`candidates.rs:250`）从 lockfile / `package.json#packageManager`
推断，产生 `CandidateKind::PackageManager` 候选。包管理器可以从当前仓库机械读出，
把它固化成 durable memory 会形成第二事实源，并在仓库从 pnpm 换成 bun 之后过期。

### D8. 纯中文标题的记忆 id 会碰撞并静默覆盖

`slugify`（`leveler-memory/src/lib.rs:368`）只保留 ASCII 字母数字，其余字符
一律替换成 `-`；纯中文标题因此被压成空串，退化到
`format!("mem-{}", now_rfc3339())`，而 `now_rfc3339` 用的是
`SecondsFormat::Secs`（`lib.rs:447`）—— 秒精度。

**已复现**：同一秒内写两条纯中文标题的记忆，两条拿到同一个 id
`mem-2026-09-11T111559Z`，后写覆盖前写，`active=1`，无任何提示。
间隔 1.2 秒重写同样两条则得到 `…651Z` / `…652Z`，`active=2`。

ASCII 标题不受影响（id 为 `a`/`b`）；中英混合标题也安全
（`金额渲染一律用 formatAmount` → `formatamount`）。

这不是"不支持中文"的设计取舍：同一文件里的 `tokenize`（`lib.rs:405`）专门为
中文做了 bigram 分词，注释写明 "Chinese queries actually match（recall +
`/memory` 都曾因此坏掉）"。slugify 是遗漏的那一处。

---

## 3. 设计取舍（不是缺陷）

- **recall block 不持久化**。`run_conversation` 过滤掉 `Role::System`，所以
  注入块不进 durable transcript，下一轮重新生成，不会累积。符合期望契约。
- **memory index 只含 title、不含 body**（K37）。`prompt.rs:148` 的模板固定、
  无 body，属于 cache-stable prefix。
- **recall 有预算**。`RECALL_K=4`、`RECALL_CHAR_BUDGET=1500` 确实有界。
  缺的是"被省略了多少"这个诚实计数，不是预算本身。
- **`vector_search` 不参与自动召回**。`relevant_memory_injection` 只用 BM25，
  所以 "token-hash cosine 被当成 semantic embedding" 这个风险在**自动召回
  路径上不存在**；命名与文档是否准确是另一件事（见下）。

---

## 4. 仅凭阅读无法确认、需要测试的部分

- `store.search` 是否严格只搜 `active/`（pending 与 archived 是否可能进入
  recall）。代码路径未逐行确认。
- `vector_search` 的对外命名与文档是否在别处被表述为 semantic embedding。
- 混合消息（`记住我的输出偏好，然后修复这个测试`）是否会把整句当成记忆正文，
  以及是否影响该轮的 coding task。`extract_remember_body` 会把
  `我的输出偏好，然后修复这个测试` 整段作为 body，但它只产生 pending，
  是否吞掉 task 需要真实运行确认。
- 引用/否定形式的误判边界。前缀锚定（`strip_prefix` / `idx == 0`）看起来能挡住
  `不要记住这个` 与 `解释一下"remember: foo"`，但 `以后都…` 是软匹配，边界未测。
- 存量 `kind=package_manager` 的 active entry 在 recall / UI / doctor 中的实际表现。
- Web 端（`leveler-web`）的 Memory 展示面。TUI 侧已确认（见 D1 表格）；Web 侧
  `store.tsx` / `Inspector.tsx` 含 memory 字样但未逐行读，且这两个文件正被另一条
  工作线修改，本次未展开。

---

## 5. 调用链现状

```
用户提交消息
  → Application::enqueue_memory_candidates            (leveler-app/src/lib.rs:725)
      → collect_turn_candidates                       (leveler-memory/src/pipeline.rs:217)
          → propose_from_user_text  → pending/
          → propose_package_manager → pending/        ← D7
      → tracing::info! only                           ← D1（产生时无推送）
  ── accept 入口已存在但需用户主动发起： ──
  ──   TUI `/memory` → ListMemory → MemoryList{active, archived, pending} ──
  ──   TUI `/memory accept <id>` → AcceptMemory → active/ ──
  ──   CLI `leveler memory pending / accept / reject` ──
  → 下一轮 Executor 组装
      → load_memory_index(memory_dir)                 ← D3（不 gate）
      → memory_root: Some(memory_dir)                 ← D3（不 gate）
      → relevant_memory_injection(request)            (executor.rs:1555)
          → MemoryStore::search (BM25 + CJK bigram)   ← D5（仅词法）
          → render_recall_block                       ← D6（无 id）
      → Message::text(Role::System, recall) 插在 user 消息前
      → run_conversation 过滤 System → 不持久化        （符合契约）
```

`leveler-engine` 在这条链上不出现任何 memory 语义，当前分层在这一点上是干净的。

---

## 6. 架构归属现状

| crate | 当前是否越界 |
| --- | --- |
| `leveler-memory` | 未依赖 TUI / Web / ModelRuntime / Engine / provider。干净 |
| `leveler-tools` | 只做工具适配（`tools/memory.rs`）+ 注册。干净 |
| `leveler-agent` | 拥有"哪条记忆与本轮相关"与注入位置。符合目标分层 |
| `leveler-app` | 拥有路径、capability 组合、候选入队。符合目标分层，但 gating 漏了（D3） |
| `leveler-engine` | 无 memory 语义。干净 |

因此本次收口**不需要**修改 `leveler-engine`：所有缺陷都落在 app 的组合根、
agent 的注入层、memory 的 domain 内部。
