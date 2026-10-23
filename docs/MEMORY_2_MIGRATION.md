# CodeLeveler Memory 2.0 旧数据处置与 Cutover 记录

状态：**迁移已取消；旧 JSON 记忆已按用户决策精确删除；Markdown Memory 2.0 已正式成为唯一实现（2026-10-23）。**

本文不再是待执行的迁移计划。它记录最终决策、真实的删除边界、执行结果与回归证据，并明确哪些内容永远不会再发生（备份副本、Shadow Compare、双 backend、反向迁移）。

设计见 `docs/MEMORY_2_DESIGN.md`，实施记录见 `docs/MEMORY_2_IMPLEMENTATION.md`。

---

## 1. 最终决策

| 项目 | 决策 |
| --- | --- |
| 旧 JSON 记忆 | 不迁移。按用户明确决定删除 |
| 备用 Backend | 不保留。Legacy JSON backend 代码已删除 |
| Shadow Compare | 不做。没有两个实现可以对比 |
| 迁移工具 | 不开发。`leveler memory migrate` 不存在，也不会存在 |
| 旧数据备份 | 本轮不创建新的迁移副本、影子副本或内容备份 |
| 双写 | 从不允许。任何阶段都没有两套 Memory 同时接受写入 |
| 凭据 | 不复制、不记录正文；删除不等于凭据失效，需要时单独轮换 |

旧记忆因此**不可恢复**。这是用户知情后的产品决策，不是实现遗漏。

## 2. 唯一架构（Cutover 结果）

```text
用户 /remember · 模型 Memory Tool · 自动 User Memory Extractor · Agent Experience
TUI/Desktop Memory 管理 · CLI Memory Commands · Headless / Resume
                        │
                        ▼
                  MemoryService            唯一业务入口（actor → authority）
                        │
                        ▼
                Markdown current           唯一事实来源（YAML frontmatter）
                        │
        ┌───────────────┼────────────────┐
        │               │                │
   Journal/Receipt   Lifecycle/CAS   SQLite FTS5（可重建派生索引）
```

| 层 | Owner | 事实性质 |
| --- | --- | --- |
| Markdown current（`<repo>/.leveler/memory`、`<repo>/.leveler/local/memory`、`~/.leveler/memory`） | Markdown store | 唯一权威内容 |
| 私有 state（`<state>/memory-v2/<scope>/`：journal、history、pending、attic、tombstone、receipt） | Markdown store | 事务与历史，不进 Git |
| SQLite FTS5 | 派生索引 | 可删除、可重建；只提供候选 |
| `MemoryContextBuilder` | agent 运行时 | 唯一召回/预算/渲染 owner |
| Injection receipt | agent 运行时 | 实际注入证据（id/revision/scope/mode/digest） |

不存在第二个 `MemoryStore`、第二个 context builder、第二个 session log，也没有 TUI 直接读写 Markdown 的路径（`./dev arch` 结构性守卫会拦截）。

## 3. 旧数据的实际位置与边界

旧实现只在两个位置写 JSON 记忆：

| 路径 | 说明 |
| --- | --- |
| `<state>/projects/<project>/memory/` | 项目旧 store：`active/`、`archive/`、`pending/`、`suppress/`、`pending_history.jsonl`、`.lifecycle.lock` |
| `<state>/no-workspace/memory/` | 无 workspace 时的旧 store |

`<state>` 在本机是 `~/.leveler/state`。

**不在删除范围内**（只读校验确认未受影响）：

- Markdown current：`<repo>/.leveler/memory`、`<repo>/.leveler/local/memory`、`~/.leveler/memory`
- 私有 V2 state：`<state>/memory-v2/<scope>/`（journal、history、pending、index、tombstone、receipt）
- Session 与对话记录：`<state>/sessions.db`、events、artifacts、boots
- 配置：`~/.leveler/config.toml`、项目 `.leveler/config.yaml`、`permissions.yaml`
- 仓库文件与 `.git`
- Dogfood 证据与其他 JSON/TOML/YAML 文件

## 4. 删除工具契约

旧的清理方式（`rm -rf ~/.leveler`、`find … -name '*.json' -delete`）被明确禁止：它们无法证明删除范围。

实际实现是 `leveler memory purge-legacy [--apply]`（`crates/leveler-project/src/legacy_memory_purge.rs`）：

1. **候选发现**：只枚举 `<state>/projects/*/memory` 与 `<state>/no-workspace/memory`，且必须是真实目录（符号链接的 state root 直接拒绝）。
2. **路径校验**：`current`、`state_root` 都 canonicalize；候选必须位于 canonical state root 之内，父级必须是 project state 目录或 `no-workspace`。
3. **内容白名单**：只允许 `active|archive|pending|suppress/` 下的 `.json` 文件，以及根目录的 `pending_history.jsonl`、`.lifecycle.lock`。任何未知目录、未知文件、`.md` 记录、符号链接都会让整个 store 变成 `BLOCKED`，不删除。
4. **默认 dry run**：只输出 `路径 + 文件数 + 字节数`；`--apply` 才删除。
5. **幂等**：第二次运行输出 `no legacy store found`。
6. **失败可见**：不可读目录返回错误，不静默跳过；阻塞项以非零退出码结束并列出原因。

输出只包含元数据（路径、数量、字节），不打印任何记忆正文或凭据。

## 5. 执行结果（2026-10-23）

| 项目 | 结果 |
| --- | --- |
| 候选旧 store | 116 |
| 阻塞项（BLOCKED） | 0 |
| 删除文件 | 133 |
| 删除字节 | 21,716 |
| 删除后复查 | `leveler memory purge-legacy` → `no legacy store found` |
| `~/.leveler/state` 其他内容 | `leveler.db`、`no-workspace/`、`projects/`（122 个项目目录）、`update.json` 保持完整 |
| 代表性项目目录 | CodeLeveler 项目目录中 `artifacts/`、`boots/`、`browser/`、`sessions.db*`、`daemon.log`、`permissions.yaml` 等仍在，只有 `memory/` 消失 |
| Markdown current | 无项目存在旧的或新的 Markdown 记忆目录（本机此前未启用 Markdown），因此没有可被误删的 V2 数据 |
| V2 私有 state | 本机此前未初始化（`memory-v2/` 数量为 0），删除未影响任何 V2 状态 |

注：删除前存在的旧 store 多为测试/dogfood 临时项目，有真实内容的两个项目分别包含 10 个文件 / 4,950 字节与 13 个文件 / 7,422 字节。正文未读取、未复制、未记录。

## 6. 回归证据

隔离文件系统测试（`crates/leveler-project/src/legacy_memory_purge.rs` 测试模块，7 个）：

| 测试 | 锁定的行为 |
| --- | --- |
| `a_legacy_store_is_removed_and_neighbours_survive` | 只删 `memory/`；同级 `sessions.db`、`config.json`、`memory-v2/`、Markdown 目录保留；dry run 不写；第二次运行幂等 |
| `a_missing_state_root_is_not_an_error` | 从未存在的 state root 不是错误 |
| `an_unknown_entry_blocks_the_whole_store` | 未知目录存在时整个 store 不删除并报告原因 |
| `a_case_variant_namespace_is_not_treated_as_a_legacy_store` | 大小写变体不被当成旧 store（仅在大小写敏感文件系统可判定） |
| `a_markdown_record_inside_the_store_blocks_deletion` | store 内出现 `.md` 记录 → 拒绝删除 |
| `a_symlink_never_extends_the_delete_scope` | store 内符号链接不删除、不跟随；符号链接的 state root 直接拒绝 |
| `an_unreadable_store_reports_the_failure` | 目录不可读时报告失败；若特权用户可读则必须是完整删除，不允许部分删除 |

此外：旧数据缺失不影响 V2 启动（fresh user 测试：隔离 HOME 下 `memory status` / `memory list` 正常，创建 Markdown current 与 state 路径，不产生任何旧 JSON）。

## 7. 保留的 schema 事实

V2 record 仍接受 `source_refs.legacy_*` 字段，用于将来导入外部记录时保留来源。当前产品**不写入**该字段：没有迁移路径，也没有代码会生成它。读取时只做只读展示。

## 8. 已失效的旧计划（供审计对照）

以下内容曾经写在本文中，现在明确作废：

- 备份到 `~/.leveler/backups/memory-v1-*`、`BACKUP_COMPLETE` 标记与 manifest。
- 隔离目录转换、schema/链接/secret/index 校验流程。
- Shadow Compare 的 query corpus 与 top-k 对比。
- `leveler memory migrate inventory|backup|convert|validate|shadow|cutover|rollback`。
- 冻结写窗口、cutover 配置切换、legacy 只读观察期。
- 反向迁移与“写后回滚”演练。

回滚现在只有一个含义：恢复到本阶段之前的提交与二进制。旧 JSON 记忆无法恢复。
