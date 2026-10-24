# 持久记忆（Memory）

英文：[`MEMORY.md`](MEMORY.md)

本文是 CodeLeveler 记忆子系统的唯一说明：记录存在哪里、谁有权改、怎样进入模型上下文、有哪些已知限制。它取代原先的 Memory 2.0 设计、实施与迁移三份报告。分层与权限规则见[架构说明](ARCHITECTURE.zh-CN.md)，本文只讲记忆本身。

状态：Markdown 记忆是唯一实现。JSON backend、`memory.backend` 选择器和迁移工具已移除；项目配置里残留的 `memory.backend` 仍能正常加载并给出提示，但它的值不能选择存储。

## 1. 一个事实源，两个派生缓存

```text
用户命令 ─┐
模型工具 ─┼─► MemoryService ─► Markdown current（事实源）
自动抽取 ─┘        │
                   ├─► SQLite FTS5 索引（派生，可重建）
                   └─► catalog 快照（派生，缓存稳定前缀）
```

| 事实 | 权威 | 丢失或损坏 |
| --- | --- | --- |
| 记录正文、状态、revision | Markdown `current` + 私有 `history` | 该 revision 不可恢复 |
| 召回候选 | SQLite FTS5 索引 | 从 Markdown 重建 |
| 注入用的 catalog（id、revision、标题） | `state/catalog/*.json` | 从 Markdown 重算 |

索引命中必须回到 Markdown 复核：索引里存的 `revision`/`digest` 与文件不一致时，该行被丢弃并删除。**索引只能提出候选，只有 Markdown 能确认事实。**

## 2. Scope 与路径

| Scope | current 记录（可编辑、可提交） | 私有状态 |
| --- | --- | --- |
| Global | `~/.leveler/memory/*.md` | `~/.leveler/state/memory-v2/global/` |
| Project | `<repo>/.leveler/memory/*.md` | `~/.leveler/state/projects/<slug>/memory-v2/project/` |
| Local | `<repo>/.leveler/local/memory/*.md` | `~/.leveler/state/projects/<slug>/memory-v2/local/` |

`current` 只放 active 记录，其余全部在私有状态根下，一个目录一件事：

```text
state_root/
  history/<id>/<revision>.md   每个已提交 revision，含已退休版本
  pending/<id>.md              等人工确认的候选
  attic/<id>.md                退休记录的快照
  tombstones/<id>.json         被遗忘记录的最小墓碑（只有 id，无正文）
  receipts/<id>.json           最近一次可信提交写下的信任凭证
  intents/<op>.json            操作意图，用于崩溃恢复
  journal/<op>.jsonl           提交日志，open 时重放
  catalog/<scope>.json         最近一次注入的 catalog
  index/                       SQLite FTS5 数据库
  lock                         跨进程锁（读共享 / 写独占）
  suppressed.jsonl             被拒绝的信号，避免重复打扰
  pending_history.jsonl        pending 状态迁移
```

Project scope 是工作区内容：写进仓库，人可以直接看、改、提交。状态根若会落在仓库内或 `.git` 内，路径层直接拒绝。

## 3. 记录格式

一条记录一个文件，YAML frontmatter + Markdown 正文：

```markdown
---
schema: 2
id: mem_<32 hex>
kind: user_fact | experience
scope: global | project | local
status: active | pending | deprecated | superseded | forgotten
revision: 3
authority: user_explicit | user_approved | user_stated | runtime_observed | model_inferred
title: 发布通道
subject: release.channel             # 可选，人类可读身份
subject_key: k-release-channel       # 可选，规范身份
value: blue                          # 可选，简短机器可读值
statement: assertion | proposal
tags: []
pinned: false
pinned_by: user | model
sensitive: false
expires_at: 2026-12-01T00:00:00Z     # 可选
supersedes: [mem_...]
superseded_by: mem_...
proposal: { operation, target_ids, expected_revision, reason }
experience: { standing, applies_when, invalid_if, evidence }
source_refs: []
created_at: 2026-10-24T00:00:00Z
updated_at: 2026-10-24T00:00:00Z
---

人写的正文。
```

| 字段 | 归属 | 说明 |
| --- | --- | --- |
| `id`、`kind`、`scope`、`revision`、`status`、`authority` | Runtime | 永不接受模型或模型自写 frontmatter 的值 |
| `title`、`body` | 该 revision 的作者 | 空标题与凭据形态文本在 service 入口拒绝 |
| `subject_key` | 写者或由 `subject` 派生 | 只有精确相等的 `subject_key` 才是同一身份 |
| `pinned` / `pinned_by` | `pinned_by=user` 是 standing 约束；模型 pin 只是排序提示 | |
| `experience.*` | Runtime 证据 | 见 §7 |
| `created_at`、`updated_at` | Runtime | |

每个已提交 revision 在 `receipts/` 留下凭证，含 `id`、`scope`、`revision`、`digest`、`status`、`authority`、`pinned_by` 与哈希后的 `subject_key`。与凭证不符的 current 文件不被采纳：正文/标题变了是 `unverified`，revision/status/subject 变了是冲突。这挡住了手工编辑、外部编辑器或模型 payload 的自我提权。

## 4. Authority

`authority` 表达运行时**知道的写入者**，不采信正文的自称。

| 写入者 | 得到的 authority |
| --- | --- |
| 显式用户命令（`/remember`、CLI、TUI 编辑） | `user_explicit` |
| 用户接受 pending 候选 | `user_approved` |
| 自动抽取原样捕获的用户表述 | `user_stated` |
| 运行时观察到 failure → resolution | `runtime_observed` |
| 模型工具调用或模型推断 | `model_inferred` |
| 导入或外部编辑的文件 | `ExternalUnverified`，有效权威封顶 `model_inferred` |

由此得到的规则：

- 模型永远不能覆盖用户已决定的事实，只能为目标产生 pending 候选。
- `Full` 权限模式不创造记忆权威。批准一次工具调用是批准这次动作，不是批准一条关于用户的持久事实。
- `sensitive` 记录永不进入注入，也永不进入 catalog。

## 5. Kind 与 Lane

| Kind | Lane | 存什么 | 默认 |
| --- | --- | --- | --- |
| `user_fact` | user | 用户说过的偏好、决定、纠正、笔记 | 开 |
| `experience` | experience | 绑定证据的经验：失败 → 修复 → 验证 | 关 |

`kind` 是闭集（`user_fact`、`experience`）；`memory.kind` 里的 `preference`、`decision`、`note` 是用户事实的类型变体，不是另外的 lane。

Standing 约束只来自用户事实：`standing_rows` 限定 `kind = 'user_fact'` 且 `pinned_by = 'user'` 或用户权威。Experience 无论权威多高都不可能成为 standing 约束。

项目开关，写在 `.leveler/config.yaml`：

```yaml
memory:
  enabled: true             # false：不抽取、不召回、不暴露记忆工具
  user_enabled: true        # 用户事实
  experience_enabled: false # 实验性经验，需显式开启
```

关掉一个 lane 只停止它的抽取与召回，不删除任何记录。

## 6. 写入路径

```text
/remember、CLI、TUI 编辑 ──► MemoryService（用户命令）─► 提交
模型 remember 工具 ────────► 接纳 agent 候选 ─────────► pending 或提交
回合结束自动抽取 ──────────► 候选 ──► admission ───────► pending 或提交
失败 → 修复 ───────────────► 经验候选 ────────────────► pending 或提交
```

自动抽取在用户回合之后离线运行，产出带证据 span 的候选；它不写 standing 事实。会替换受保护用户决定的候选一律停在 pending；被拒绝的信号写进 `suppressed.jsonl`，不再重复打扰。

Admission 只依据结构化事实判断 `Applied`、`Pending` 或 `Skipped`：调用者、目标权威、statement 类型、revision、subject 身份。没有同义词表、相似度阈值或裁判模型参与。

## 7. Experience

经验记录带 `applies_when`、`invalid_if` 和 `evidence` 链。`standing = verified` 需要同时满足：

1. `evidence` 说明运行时观察到了 resolution；
2. 当前 revision 的 authority 仍是 `runtime_observed` 或 `user_approved`。

用户以 `user_explicit` 改写正文后，`standing` 降级为 `observed`：这段文字已经不是运行时验证过的那段。证据与旧 revision 仍可读、可审计；重新验证要靠新的观察，不是编辑旧文字。

经验抽取默认关闭，必须按项目显式开启。

## 8. Pending 与同意

候选留在各自 scope 的 pending 集合里。review 页把 global、project、local 的 pending 一起列出，逐条接受或拒绝。

| 动作 | 效果 |
| --- | --- |
| 接受 | 以接受者的权威提交该候选 |
| 拒绝 | 移除候选并记录抑制 |
| Headless 运行 | 候选停在 pending，等人工处理，绝不自动接受 |

无法询问的运行时继续干活，把候选留在 pending。不存在模型、headless 运行或某种权限模式替用户批准持久事实的路径。

## 9. 暴露给模型的工具

| 工具 | 表面 |
| --- | --- |
| `memory` | `action`：`search`、`list`、`read`、`update`、`deprecate`、`pin`、`unpin`；`scope`：`project`（默认）、`local`、`global` |
| `remember` | 提议一条持久记录：`title`、`body`、`subject`、`kind`，可选 `value` |
| `forget` | 按 id 归档一条记录，也可恢复 |

`update`、`deprecate`、`pin` 必须带上调用者读到的 `expected_revision`，不匹配是冲突而不是覆盖。`read` 每回合有调用与 token 双重预算，超过时返回 `(truncated: showed bytes a..b of n; continue with offset=b)`，长正文分段读，而不是整段塞进上下文。

模型对用户已决定的记录写入时只会得到 pending，所以工具返回「已提议」不等于记忆已改变。

## 10. 召回与注入

一个回合的 memory 块由三部分组成：

| 段落 | 内容 | 位置 |
| --- | --- | --- |
| Catalog | 每条 active、非敏感用户事实一行 `[scope][authority][id r<rev>] 标题` | 稳定前缀 |
| Standing 约束 | 被用户 pin 或用户权威的用户事实（上限 8 条） | advisory 块 |
| 召回 | 每个 scope 的 FTS5 候选，回到 Markdown 复核后注入 | advisory 块 |
| 召回经验 | experience lane 的教训，去掉与用户决定冲突的 | advisory 块 |

每个 scope 的上限：用户命中 12 条收敛到 8 条候选，经验命中 6 条收敛到 2 条。被召回的正文按数据处理，不是指令；块里带省略计数，让模型知道有内容被丢弃。

当某个 `subject_key` 已被 standing 用户事实或候选用户事实占用时，同 key 的经验不注入，只计入省略——记录仍在库里、仍可 `memory.read`、仍可审计。不同 `subject_key` 的记录保留不确定性。由此形成的优先级完全由结构决定：

```text
当前用户消息
  > standing 用户约束
    > 适用的候选用户事实
      > 未被用户决定遮蔽的适用经验
```

每次注入写下凭证（`ids`、`catalog_ids`、`catalog_version`、`catalog_digest`、块摘要、省略项）并发出 `MemoryRecalled`。凭证摘要是对注入字节算的，因此报告能证明哪次请求带了哪条记忆。

## 11. Catalog 失效

Catalog 属于缓存稳定前缀，因此一个 run 内不得重写。每份 catalog 快照带 `version`；当 Markdown 与已注入快照不一致时，当前 run 只追加 tail delta（`added`、`updated`、`removed`，`removed` 不带标题），下一次 run、resume 或 compaction 重建前缀。

只有用户动作能改变 catalog，而用户无法在一个运行中的回合里操作，所以正常运行下不会出现回合内 live delta。该路径由确定性测试覆盖，不宣称已通过真实 live 验证。

## 12. 索引

`state_root/index/` 是派生 FTS5 数据库：

- `documents`：一条记录一行，含 `id`、`scope`、`kind`、`status`、`revision`、`digest`、`title`、`subject`、`subject_key`、`value`、`tags`、`authority`、`pinned`、`updated_at`、`expires_at` 以及四个词项列。
- `documents_fts`：对 `title_terms`、`subject_terms`、`body_terms`、`tag_terms` 建 FTS5，external content 指向 `documents`，`unicode61` 分词，`bm25(3.0, 2.0, 1.0, 1.5)`。
- `meta`：`schema_version`、`tokenizer_version`、`dirty`。

词项包含中文 n-gram 与标识符拆分，所以中文短语与 camelCase 标识符都能命中。schema 或分词版本不兼容、`PRAGMA quick_check` 失败、或显式重建时，索引被删除并标记 dirty；下一次读取从 Markdown 重建。排序是纯词法的：没有向量召回，词法命中本身永不创建、替换或合并记录。

## 13. 生命周期操作

| 操作 | current | 历史 | 索引 | 可恢复 |
| --- | --- | --- | --- | --- |
| Update | 新 revision | 保留旧 revision | 重新 upsert | 是 |
| Deprecate | 退休到 attic | 保留 | 删除该行 | 是，历史可读 |
| Forget | 删除正文 | 删除 | 删除该行 | 否，只有墓碑 |
| Pin / unpin | 同一 revision | — | 重新 upsert | 是 |

Deprecate 与 Forget 是两个操作，TUI 文案必须区分：`d` 是「标记为过期（保留历史）」，`f` 是「彻底遗忘（删除正文，保留最小墓碑）」。重建索引、重启、Resume、Compaction 都不得让被墓碑覆盖的正文复活。

跨文件提交没有全局原子性。运行时保留 intent 与 journal，open 时重放；持锁的读者只会看到提交前或提交后的完整状态，不会看到半个提交。直接读目录的外部工具可能观察到瞬态窗口，必须接受或检查 journal。

## 14. 使用表面

CLI：

```sh
leveler memory list|status|search|show|remember|pending|accept|reject|propose
leveler memory forget <id>        # 删除受管正文，只留墓碑
leveler memory diagnose          # 只读检查某个 scope，不做任何修复
```

`/remember <正文>` 从输入框直接写一条记录。`/memory` 打开 Home（active 列表、待确认、历史），`/memory review` 直接打开待确认队列，`/memory forget <id>` 与 `/memory accept|reject <id>` 按 id 操作。

TUI 按键：

| 页面 | 按键 |
| --- | --- |
| Home | `↑↓` 选择 · `Enter` 打开 · `Esc` 返回 |
| Active / 历史列表 | `↑↓` 移动 · `Enter` 查看 · `/` 搜索 · `s` 范围 · `t` 类型 · `x` 清除过滤 |
| 待确认 | `↑↓` 移动 · `空格` 选择 · `Enter` 查看 · `a` 接受 · `r` 拒绝 |
| 详情 | `↑↓` 滚动 · `e` 编辑 · `p` pin · `d` 标记过期（再按一次确认） · `f` 彻底遗忘（再按一次确认） · `y`/`Y` 复制正文/id · `Esc` 返回 |

编辑只提交 `title` 与 `body`，并带上打开页面时的 revision。`id`、`kind`、`scope`、`authority`、`status`、`revision`、`pinned`、`source_refs`、`experience`、`expires_at` 归 Runtime 所有。

## 15. 测试与 Dogfood

确定性测试守住契约，真实运行守住可达性。

```sh
cargo test -p leveler-memory          # store、schema、索引、生命周期、崩溃、并发
cargo test -p leveler-tools           # 记忆工具契约
cargo test -p leveler-app             # service 接线、抽取、注入
```

规模基准（默认忽略）：

```sh
MEMORY_SCALE=10000 MEMORY_EXPERIENCE=2000 \
  cargo test --release -p leveler-agent --lib memory_scale -- --ignored --nocapture
```

真实验收在 dogfood 仓库，不在本仓库：

```sh
cd ../dogfood
python3 eval/memory/live_acceptance.py --phase all \
  --binary /path/to/release/leveler --lab /tmp/memory-live
```

它用独立 `LEVELER_HOME`、独立 git 工作区、真实 `leveler tui`（PTY）和配置好的真实 provider 运行，中间加一个回环抓包代理：记录 provider 实际收到的请求字节，同时把请求转发给真实 HTTPS 端点。每条结论都读运行时自己的事实：Markdown current、私有状态根、`sessions.db`、请求字节。provider 不可达时标记 blocked，绝不当作通过。

## 16. 性能

同机、release、10k 合成记录、命中全部记录的热查询（排序的最坏情况）：

| 配置 | p50 | p95 |
| --- | --- | --- |
| 10k，仅 user lane（出厂默认） | 7.3ms | 9.3ms |
| 10k，双 Lane，经验 lane 为空 | 11.3ms | 15.6ms |
| 10k，双 Lane，2000 条经验 | 12.0ms | 24.5ms |

10k 索引重建 0.8s；冷开 store 约 150–350ms（journal 恢复 + 孤儿扫描），warm catalog 亚微秒级。

user lane 达到 p95 < 10ms 的目标，双 Lane 最坏情况未达标。代价是两次互不相交的行集上的词法排序：lane 过滤已经保证每次查询不为另一个 lane 的行打分，而 FTS5 不能在子查询里用 `bm25()`，所以「每个 lane 各取 top-K」必然是两次扫描。要降下来只能把两次排序合并（无法保证每个 lane 的 K）、减小 K、或跳过 Markdown 复核——三者都在拿召回或正确性换延迟，因此都没有实现。

## 17. 已知限制

1. **跨 subject 的近义改写仍可能重复。** 没有 embedding 或裁判模型就无法证明等价；只有精确 `subject_key` 或显式 `supersedes` 会收敛。不同 subject 下正文完全相同时只报 `possible_duplicate`，不合并。
2. **不同 subject 下的矛盾可以并存。** 两条都作为候选注入，由主模型判断，运行时不替用户选一个。
3. **抽取不是证明。** `user_stated` 需要精确用户 span，verified 经验需要运行时凭证。模型把 proposal 误标成 assertion，仍然只能产生 pending 候选。
4. **续跑回合不自动抽取。** 自动抽取由 fresh 用户回合触发；续跑过程中用户的修正只能通过直接命令或模型工具进入记忆。
5. **外部编辑无法自我提权。** 凭证不匹配的文件权威封顶 `model_inferred`。凭证不能防住能写状态根的同权限进程。
6. **FTS5 是词法的。** 中文同义、跨语言改写召回有限，这是明确取舍，本设计不引入向量。
7. **Forget 不是加密擦除。** Git 对象、事件日志、备份、provider 日志与文件系统残留仍可能保留正文。墓碑只证明 CodeLeveler 管理的副本已被删除。曾经进入存储的凭据仍需轮换。
8. **未做跨平台验证。** rename、锁与 fsync 语义只在 macOS 验证；状态根不支持网络文件系统。
9. **跨文件系统没有绝对原子性。** 只保证 journal 恢复与读者隔离。

## 18. 不可回退的约束

- Markdown 是唯一权威，索引与 catalog 是可重建缓存。
- 所有写入与生命周期转换只经过一个 service。
- 模型不能自我提权，也不能覆盖用户决定。
- 权限模式永不创造记忆权威。
- `forget` 删除受管正文，`deprecate` 退休并保留历史。
- 重建、Resume、Compaction、重启都不得让被墓碑覆盖的正文复活。
- 稳定前缀只在 run 之间变化；run 内的变化只能是 tail delta。
