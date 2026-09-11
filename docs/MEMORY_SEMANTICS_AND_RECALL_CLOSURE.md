# Memory Semantics & Recall Closure

配套阅读 `docs/MEMORY_CURRENT_BEHAVIOR_AUDIT.md`（修改前的只读审计）。
本文件记录这次收口做了什么、为什么，以及证据。

---

## 1. Root cause

用户感觉"记忆没生效"，不是一个原因，是三个独立的：

**R1 — 说了"记住"却毫无反馈。** 显式意图确实被识别并存成候选，但
`Application::enqueue_memory_candidates` 在产生 pending 之后只写一条
`tracing::info!` 就结束了。采纳入口一直存在（协议的 `AcceptMemory`、
`MemoryList` 事件里的 `pending`、TUI 的 `/memory accept <id>`），但没有任何东西
把用户指向那里。存了 = 看不见 = 等于没存。

**R2 — 长期偏好依赖词法命中。** 召回只有一条 BM25 lane。CJK bigram 让不少中文
paraphrase 仍能命中，但无共同字的同义和跨语言 query 稳定漏掉：已保存
「用户偏好紧凑的终端信息输出」，用户说「能不能精简一点」时得分为 0。一条长期
偏好的适用性本来就不取决于本轮用词，所以这是形状错了，不是分数没调好。

**R3 — 一个 capability 两个 owner。** `exposed_capabilities` 只决定工具是否注册；
`memory_index`、`memory_root` 与 prompt 里的 `## Memory` 一节各走各的路。结果
Economy 轮次把 index、召回正文和整节 guidance 全都送进模型，而工具没注册 ——
prompt 在教模型调用它拿不到的 `remember`。

另有两条在审计中被证据推翻、必须单独记下的：

**R4 — 模型能给自己的记忆签名。** assisted 下同意门生效并把提案 park 成候选，
随后模型用 `run_command` 执行 `leveler memory accept <id>`，被沙箱拒绝后加
`escalate.filesystem = unrestricted` 重试并成功。park 消息里那句
`run `leveler memory accept <id>`` 是写给人的，模型读了照做。

**R5 — 直接写入会静默覆盖。** CLI 的 `memory remember` 用的是按 id upsert 的
`store.remember`，同名标题第二次写入直接顶掉第一条，与 `base.md` 承诺的
"remember does not overwrite" 相反。

---

## 2. 修改前的真实流程

```
用户提交消息
  → enqueue_memory_candidates                    (app/lib.rs)
      → collect_turn_candidates
          → propose_from_user_text  → pending/
          → propose_package_manager → pending/   ← 仓库事实被记忆
      → tracing::info! 然后结束                   ← R1
  ── 用户必须自己想到敲 /memory 才会发现 ──
  → Executor 组装
      → load_memory_index(...)                   ← R3 不 gate
      → memory_root: Some(...)                   ← R3 不 gate
      → BASE_PROMPT 含 ## Memory                  ← R3 不 gate
      → relevant_memory_injection
          → store.search  (BM25 单 lane)          ← R2
          → render_recall_block（无 entry id）
```

---

## 3. 修改后的写入状态机

```
用户直接写入（CLI / 未来的 /remember）
    ↓  remember_deduplicated（永不覆盖）
  active + 明确反馈「✓ remembered [id]: title」

系统推断（显式意图）
    ↓
  pending
    ↓  Notification：「发现 N 条可能值得记住的内容，等待确认：[id] title」
  用户可见
    ├─ /memory accept <id> → active
    └─ /memory reject      → suppress

Agent 提议（remember 工具）
    ↓  resolve_policy
    ├─ full-access            → 直接写入（有意的产品取舍）
    └─ assisted / request-approval
          ↓  ask() → 真人
          ├─ 批准 → active
          └─ 无人  → park 成 pending，消息不含可执行的采纳指令
    ✗ leveler memory accept|reject|remember|forget 经任何工具执行 → 拒绝
```

最后一行是新增的授权闸门，放在权限规则**之前**，所以一条 `run_command` 的
standing allow 规则也无法重新打开它。shell 包装会先被
`proven_executed_commands` 展开，`sh -c "leveler memory accept x"` 和 `&&` 链
都算。

## 4. 修改后的召回状态机

```
turn 开始
    ↓
exposed.memory（selection ∩ availability，唯一答案）
    ├─ false → 无工具、无 index、无 guidance、无召回
    └─ true
         ↓
     standing preferences（active、非派生、声明为 preference，≤ 8 条，确定序）
         +
     query recall（store.recall = search 去掉派生事实，top-K，分数门槛）
         ↓
     按 id 去重（standing 优先）
         ↓
     共享字符预算，超出则报告省略条数
         ↓
     临时 system 块，带 entry id，插在当前 user 消息之前
         ↓
     不持久化（run_conversation 过滤 System），下一轮重新生成
```

---

## 5. 四类知识的边界

| 类别 | 权威来源 | 本次处理 |
| --- | --- | --- |
| Project Rules | `AGENTS.md`、`.leveler` 指令、版本化规则 | 不复制成 memory |
| Durable Memory | 用户批准的偏好 / 决策 / 非显然约束 | advisory，可过期，可忘记，永不作为权限或完成凭据 |
| Derived Repository Context | 工作树本身 | 不再写成 memory；`package_manager_from_root` 保留，按需读取 |
| Session / Goal Continuity | Engine / lifecycle | 未触碰 |

## 6. 为什么 Memory 不属于 Engine

`leveler-engine` 在整条链上不出现任何 memory 语义。这次收口所有落点都在 app 的
组合根、agent 的注入层、memory 的 domain 内部与 execution 的授权层。

有一处刻意没有走 Engine：pending 通知本可以塞进 `observer: &mut dyn
FnMut(EngineEvent)`，但那会把 memory 语义放进 Engine 的事件词汇。改为把候选从
`enqueue_memory_candidates` 返回，由持有客户端连接的 `InteractiveRuntime` 用现成的
`RuntimeEvent::Notification` 发出。`ENGINE_CHANGED=NO`。

## 7. UserExplicit / SystemInferred / AgentProposed

| 来源 | 授权 | 落点 |
| --- | --- | --- |
| UserExplicit | 命令本身即授权 | 直接 active，`remember_deduplicated` |
| SystemInferred | 必须用户同意 | pending + 可见通知，accept/reject |
| AgentProposed | ToolHost 审批 | full-access 直写（取舍）；否则真人批准或 park |

## 8. capability selection / availability / exposed

`exposed = selection ∩ availability`，现在这一个值同时决定：`memory` /
`remember` / `forget` 是否注册、index 是否加载、guidance 是否进 prompt、
recall root 是否设置、standing 与 query 召回是否注入。

用户经 CLI 或 `/memory` 管理记忆属于用户控制面，不受 `exposed.memory` 影响 ——
关掉模型的记忆能力不等于关掉用户自己的记忆。

## 9. standing preference 与 query recall 的区别

| | standing | query |
| --- | --- | --- |
| 触发 | 无条件 | 与本轮请求的词法相关性 |
| 入选 | `kind == "preference"` 或 `preference` 标签 | 任意非派生 active |
| 上限 | 8 条 | top-4，分数 ≥ 0.1 |
| 无标签的旧条目 | 不入选（不靠猜测提升） | 正常参与 |

## 10. token-vector 的真实能力边界

`vector_search` 是 hashed token bag + cosine，**不是** semantic embedding，也
**不参与**自动召回（`relevant_memory_injection` 只用 BM25，代码与注释一致）。本次
没有引入任何外部 embedding 依赖，也没有改它的命名 —— 它既然不在自动路径上，
改名不属于本次必要范围。`EXTERNAL_EMBEDDING_DEPENDENCY=NO`。

## 11. legacy package-manager memory 的处理

不删除任何用户 active 数据。`is_derived_fact`（`kind` 或 `key` 命中
`package_manager`）把这类条目排除在 `recall` 之外，但 `search` 仍返回它们，
所以 `/memory` 与 doctor 照旧可见可管理。新的候选不再产生，
`propose_package_manager` 与 `detect_package_manager`（候选工厂）已删除，
`package_manager_from_root`（纯检测）保留。

`LEGACY_DERIVED_MEMORY_DATA_LOSS=NO`。

## 12. 数据兼容

`CandidateKind::PackageManager` 保留，旧 JSON 仍能反序列化。`MemoryEntry` 字段
未变。`ExecutorFactory` 新增 `memory_expose: bool`（进程内类型，无线协议影响）。
未改 schema、未改 `protocol.gen.ts`、未改任何 client 协议类型。

## 13. 测试

新增 / 改写：

| 位置 | 覆盖 |
| --- | --- |
| `leveler-cli` | 直接写入永不覆盖（ASCII 重复标题 + 同秒双中文标题） |
| `leveler-memory` | legacy 派生条目 search 可见、recall 不可见；lockfile 不提议；用户意图仍提议；standing 只取声明的 preference；standing 有界且确定 |
| `leveler-execution` | self-consent 命令识别（含路径前缀、选项前置、`sh -c`、`&&` 链）；普通命令不误伤 |
| `leveler-agent` | recall block 带 entry id；省略条数诚实；零词法重合时 standing 仍注入；两 lane 去重只注入一次；park 消息带候选 id 且不含可采纳命令；capability 关闭时无 guidance 无 index |
| `leveler-app` | enqueue 返回待确认候选（可通知） |

闸门：

```
FMT=PASS
CLIPPY=PASS（workspace --all-targets，0 告警）
TESTS=PASS（153 个测试二进制，0 failed）
```

## 14. 真实 Dogfood

真实模型（deepseek-v4-flash）、真实仓库、真实 PTY TUI。

**Dogfood 1（写入可见，PTY 真机 TUI）** 输入 `记住：提交前一定要先跑 pnpm lint`，
屏幕上出现：

```
发现 1 条可能值得记住的内容，等待确认：[cand-pnpm-lint] 偏好：提交前一定要先跑 pnpm lint。用 /memory 查看…
```

随后 `/memory` 列出 `[cand-pnpm-lint] 偏好：提交前一定要先跑 pnpm lint`。

**Dogfood 2（零词法重合召回）** active 存
「用户偏好紧凑的终端信息输出，不希望看到冗长的模型过程。」，
query 为「读 src/theme.ts 和 src/InvoiceManagement.tsx，说明它们的关系」。
前置验证 `leveler memory search` 对该 query 命中数为 0。该轮真实
`context_snapshot` 中：

```
Project memory for this turn
Memory is advisory and records what was true when it was written. …

Lasting preferences (apply unless this turn says otherwise):
- [mem-2026-09-11T134119Z] 输出偏好: 用户偏好紧凑的终端信息输出，不希望看到冗长的模型过程。
```

块紧贴 user 消息之前，带 entry id，`Retrieved as possibly relevant` 一节不存在
（query lane 确实无命中）。

**Dogfood 3（仓库事实）** 仓库含 `pnpm-lock.yaml` 与
`package.json#packageManager`，跑一轮普通任务后
`active=0 pending=0 archived=0`，`REPO_DERIVED_MEMORY_CREATED=0`。

**Dogfood 4（Economy）** 同一个 store、同一个 query，`--work-mode economy` 的真实
prompt 中 `Project memory for this turn`、`Lasting preferences`、entry id、
记忆正文、`Project memory index`、`## Memory`、`remember` 全部不存在。改前全部存在。

## 15. 后续 extension seam（本次不实现）

`MemoryScope::User` / `MemoryScope::ProjectUser` 是合理方向。当前 store 根来自
`Layout::memory_dir()`，即 project 作用域；引入全局作用域只需在 store 打开处多一个
根并在召回时合并，`is_standing_preference` 与 `is_derived_fact` 两个谓词不变。
本次没有扩 scope。
