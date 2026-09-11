# Memory Final Product Closure

接 `docs/MEMORY_SEMANTICS_AND_RECALL_CLOSURE.md`（Phase 1，部分收口）与
`docs/MEMORY_CURRENT_BEHAVIOR_AUDIT.md`（改动前的只读审计）。

本轮把剩下的控制面、同意语义、数据安全、召回预算和可观察性一次收完，并用真实
PTY TUI、真实 runtime、真实模型、真实浏览器取证。

---

## 1. 最终写入状态机

```
用户直写（/remember · Web 面板 · CLI · 严格自然语言命令）
    ↓  domain validation（拒绝凭据）
  MemoryStore::activate
    ↓
  active + 明确反馈「已保存记忆 [id]：title」
  ✗ 不产生 pending    ✗ 不发模型请求    ✗ 不开 agent turn

系统推断（"我通常希望…" / "always use …"）
    ↓
  pending（source=SystemInferred）
    ↓  立即通知，列表里带正文、kind、来源
  ├─ /memory accept <id> · Web「接受」 → active
  └─ /memory reject <id> · Web「忽略」 → suppress，同信号不再骚扰

Agent 提议（remember / forget 工具）
    ↓  resolve_policy：三种 profile 一律 NeedApproval
  ├─ 有人可达 → 真人批准 → active / archive
  └─ 无人可达 → remember 保留为 pending；forget 直接拒绝
  ✗ ApproveAlways 不为 memory 写入生成永久规则
  ✗ leveler memory accept|reject|remember|forget 经任何工具执行 → 拒绝
```

严格自然语言命令的分类发生在 `InteractiveRuntime::handle_direct_memory_message`，
即 `stage_turn` 之前，所以 TUI、Web、移动端和远程走同一条语义 —— 放在某个客户端
的 reducer 里会变成"一个端保存、另一个端把同一句话交给模型"。

## 2. 最终 consent matrix

| 发起者 | RequestApproval | Assisted | FullAccess |
| --- | --- | --- | --- |
| `/remember`、Web 面板、CLI、严格自然语言命令 | 直接 active | 直接 active | 直接 active |
| 系统推断的软信号 | pending | pending | pending |
| Agent `remember` / `forget` | 真人批准 | 真人批准 | **真人批准** |

最后一格是本轮的产品决定：full access 是对这台机器和这个工作区的执行授权，不是
"可以改写未来会话会读到什么"的授权。一条错的长期记忆会在之后每一轮被静默读回，
远比一条错的命令更难发现。

## 3. 严格命令 vs 软信号

直写只认锚定在首字符的命令式前缀：`记住：` / `记住:` / `请记住：` / `请记住:` /
`remember:` / `please remember:`。

拒绝直写的情形，每一条都有测试：

| 输入 | 为什么不直写 |
| --- | --- |
| `不要记住这个` / `别记住：使用 npm` | 前缀不在首字符，否定句 |
| `解释一下“记住：使用 npm”是什么意思` | 引号 —— 在讨论这句话 |
| 含 `` ` `` 或 ``` 的消息 | 代码，不是命令 |
| `记住输出要简洁，然后修复 tests/a.rs` | 带第二个任务 |
| `我通常希望输出短一点` / `always use pnpm` | 是描述习惯，不是下命令 |

分类保守是故意的：漏存一条用户可以再敲一次 `/remember`，而吞掉真实任务不可恢复。

## 4. RecallPlan

`MemoryRecallPlan`（`leveler-agent/src/executor.rs`）拥有整个决定：capability
gate、standing 选择、query 召回、derived/sensitive 排除、按 id 去重、顺序、字节
上限、渲染元数据、结构化 trace。它记录 `standing` / `queried` / `selected` /
`omitted` / `truncated` / `rendered_bytes`，所以"这一轮用了哪几条、为省空间丢了
什么"是可查的事实，不再靠从字符串里猜。

预算是硬的：`RECALL_BLOCK_MAX_BYTES = 2048` 覆盖整块（header、小节标题、id、
title、body、省略标记）。旧检查用 `&& used > 0` 豁免了第一条，所以单条超长记忆
可以独自冲破任何上限。现在第一条也会被按 UTF-8 字符边界截断并显示 `…`，id 和
title 保留（能查得到的条目比静默消失有用），装不下的条目计入 `omitted` 并在块尾
诚实报出。

trace 只记 id 和计数，且 id 限量限长；title 和 body 永不进日志。

## 5. Catalog 的角色

不再是"全部 active 标题"。catalog 只收 active、非 sensitive、非 derived、且
kind ∈ {decision, note, legacy-unknown} 的条目，最多 16 条，按 `updated_at` 降序
（cap 不能把刚做的决定挤掉）。preference 不进 catalog —— 它已经在每轮 tail 里
带正文注入，列在 prefix 里等于为同一条记忆付两次钱。

catalog 的唯一用途是发现：本轮 query 的词法对不上时，模型仍能凭标题找到一条
decision/note，再用 `memory` 工具读正文。

## 6. Secret 边界

`validate_entry` 是唯一拒绝点，所有创建 active 的路径都经过它：`/remember`、
Web 面板、CLI、被批准的 agent 提议、accept。UI、CLI 和工具都不各自实现一份判断。

存量敏感数据不删除，但从每条自动路径撤出：不进 standing、不进 query recall、
不进 catalog，并在 `/memory` 与 Web 列表里标注「敏感内容，不提供给模型」，用户
可自行归档。

## 7. 协议与端一致

新增 `RememberMemory`、`RejectMemory`、`UiMemoryKind`、`UiMemoryCandidate`
（带 body / kind / source，因为只看标题批准不算知情同意）。`UiMemoryEntry` 增加
`kind` 与 `sensitive`。schema 与 `protocol.gen.ts` 已重新生成并通过 drift check。

| 面 | 直写 | 接受 | 拒绝 | 归档 |
| --- | --- | --- | --- | --- |
| TUI | `/remember [--kind …]` | `/memory accept <id>` | `/memory reject <id>` | `/memory forget <id>` |
| Web | Memory 面板输入框 + kind 下拉 | 「接受」 | 「忽略」→ RejectMemory | 「遗忘」 |
| CLI | `memory remember … --kind` | `memory accept` | `memory reject` | `memory forget` |

`ForgetMemory` 收到 pending id 不再静默无事发生，而是明确指向拒绝。远程配对拒绝
全部 memory 命令，包括这两个新命令。

## 8. FullAccess 的准确边界

机械关闭的是**官方同意控制面的绕过**：`leveler memory accept|reject|remember|
forget` 经任何工具执行都被拒绝，判断在权限规则之前，且 shell 包装（`sh -c`、
`&&` 链、`env`、`command`、`cmd /C`）会先被展开。

**没有**关闭的是：FullAccess 本来就意味着任意宿主文件写入。一个命令级的识别不是
操作系统级隔离，用不断加长的 denylist 假装解决同一用户权限下的任意代码执行是不
诚实的。所以：

```
Official consent control-plane bypass = mechanically closed
Arbitrary host-file mutation under FullAccess = outside this command-level invariant
```

## 9. 归属

`leveler-engine` 在整条链上不出现任何 memory 语义。pending 通知本可以塞进
`observer: &mut dyn FnMut(EngineEvent)`，但那会把 memory 语义放进 Engine 的事件
词汇；改为把候选从 `enqueue_memory_candidates` 返回，由持有客户端连接的
`InteractiveRuntime` 用现成的 `RuntimeEvent::Notification` 发出。

## 10. token-vector 的真实边界

`vector_search` 是 hashed token bag + cosine，**不是** semantic embedding，也
**不参与**自动召回。本轮没有引入任何外部 embedding 依赖，也没有实现全局用户
Memory（`MemoryScope::User` 只是记下的 extension seam）。

## 11. 真实 Dogfood 证据

见本文件末尾的最终报告字段。所有 PASS 由真实二进制、真实 PTY、真实 runtime、
真实模型与真实浏览器支撑，取证自 memory store 文件、`sessions.db` 的
`model_requests` / `turns` / `context_snapshot`，以及 WebSocket 实际发出的帧。
