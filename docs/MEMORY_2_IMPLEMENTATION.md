# CodeLeveler Memory 2.0 实施计划

状态：Stage 3.1（Store 生命周期性能修复、`/memory` 管理页、召回引用可见性）已于 2026-10-23 实现，见 §3.1；真实多轮 Catalog Delta / Prefix Cache / Lane 关闭 Dogfood 仍未执行。Stage 1 已实现并完成 Phase C 验证；Stage 2（统一 Memory Service、运行时接线、多 scope Pending review）已于 2026-10-23 落地，Stage 2.1 正确性收口随本轮实现。Stage 3（FTS5 derived index、Memory Context Builder、Logged Injection）已于 2026-10-23 实现，验证结果见该轮交付报告。Stage 4 未开始。本文给出 Stage 1–4 的代码范围、依赖、测试/Dogfood、停止条件、完成标准、回滚，以及决策表和 10 个问题的结论。Stage 1 的实际测试与验证结果见 §1.5，Stage 2.1 的改动与未验证项见 §2.11（设计结论见 §7）。

前置文档：

- `docs/MEMORY_2_DESIGN.md`：架构、schema、service、tool、context、index、安全。
- `docs/MEMORY_2_MIGRATION.md`：盘点、备份、映射、校验、cutover、回滚。

---

## 0. 实施总原则

1. **先测试后实现**：每个行为变化先写可失败的测试；失败原因必须是目标行为缺失，不是编译/fixture/环境错误。
2. **v2 与 legacy 隔离**：Stage 1 只在 `leveler-memory::v2` 增加新模块，不改旧 store 行为；Stage 2 起通过配置选择 backend，默认仍是 legacy，直到 Stage 4 cutover 由用户批准。
3. **不一次性改完所有 memory 模块**：Stage 1 只做 store/schema/journal/lifecycle；Stage 2 才接 service/tool/auto；Stage 3 才接 index/context/event；Stage 4 才迁移。
4. **不新增语义判断**：不加同义词/停用词/主题词表/文本相似度/LLM Judge；FTS5 只做词法召回；身份只由结构化 `subject_key` 决定。
5. **不双写**：legacy 与 v2 不同时接受写；迁移期 legacy 只读。
6. **每个阶段可停止**：阶段验收不通过就停，不靠下一阶段“顺便修”。
7. **不复用未跟踪 WIP 的所有权**：`crates/leveler-memory/tests/e3_lifecycle.rs` 是并行 WIP，不修改；Stage 1/2 在自己的测试文件里移植等价契约。

---

## Stage 1：Markdown Store Foundation

### 1.1 目标

建立 v2 Markdown store 的持久化与生命周期基础，可独立测试，不接运行时：

- 一条逻辑 Memory = 一个 Markdown 文件 + frontmatter。
- stable id、revision、status、scope、kind、authority、proposal、experience、source_refs。
- 原子写、父目录 fsync（Unix）、history、tombstone、per-scope lock、journal、崩溃恢复。
- 纯生命周期决策：create/update/supersede/deprecate/forget/pin/unpin/pending review。
- 读/list（不含检索；检索在 Stage 3 用 FTS5 实现）。

### 1.2 代码范围

新增 `crates/leveler-memory/src/v2/`：

| 文件 | 职责 |
| --- | --- |
| `v2/mod.rs` | 导出；不改变 legacy `lib.rs` 的公开行为 |
| `v2/record.rs` | `MemoryRecord`、`MemoryKind`、`MemoryScope`、`MemoryStatus`、`MemoryAuthority`、`SourceRef`、`Proposal`、`Experience`；schema 校验；status 转换 |
| `v2/frontmatter.rs` | YAML frontmatter 解析/序列化；顶层重复键扫描；未知字段保留；大小限制 |
| `v2/lifecycle.rs` | 纯决策：身份、authority、冲突、supersede、pending、expire、pin；输入 record view + caller + intent，输出 decision；不写文件 |
| `v2/journal.rs` | journal 类型、prepared/committed/blocked、stage 路径、恢复计划 |
| `v2/store.rs` | `MarkdownStore`：open/read/list/effective_active/admit/update/supersede/deprecate/forget/pin/expire/history/pending review/recover；per-scope lock；CAS；OpId intent；目录 fsync；不含检索 |
| `v2/path.rs` | `ScopeRoots { scope, current, state, repo_root }`、scope root、id 白名单、文件名、canonicalize、symlink 拒绝、current/state 不嵌套、state 在 repo 外、local/project gitignore 检查 |
| `v2/receipt.rs` | 受信 receipt：服务写入的 revision/digest/status/authority/pin/subject_key；外部编辑检测；`ExternalUnverified`/`ExternalConflict` 分类；不存正文 |

修改：

- `crates/leveler-memory/src/lib.rs`：仅 `pub mod v2;` 和必要的 re-export；不改 legacy 函数。该文件有并行 WIP，只做最小单行插入。
- `crates/leveler-memory/Cargo.toml`：增加 `serde_yaml`（workspace，serde_yaml_ng 0.10）、`sha2`（workspace）、`uuid`（workspace，v4）。`tempfile`、`fs2`、`chrono`、`serde_json` 已有。
- 不增加 `leveler-core` 依赖：v2 的 ID 用本模块 newtype，secret scan 复用同 crate 的 `candidates::looks_like_secret`，路径由调用方传入 `ScopeRoots`。

明确不做：

- 不接 `leveler-agent`/`leveler-app`/`leveler-tools`。
- 不改 `Layout::memory_dir()`。
- 不写 FTS5、不写 context builder、不写 event。
- 不迁移真实数据。
- 不删除/重构 legacy `MemoryStore`。
- 不做生产级 Tool/Extractor 去重（Stage 2）；Stage 1 只提供 OpId/identity/CAS 接口。

### 1.3 关键接口

```rust
pub struct ScopeRoots {
    pub scope: MemoryScope,
    pub current: PathBuf,
    pub state: PathBuf,
    pub repo_root: Option<PathBuf>, // required for Local
}

pub struct MarkdownStore { /* roots, scope, lock */ }

pub struct OpId(String);
pub struct Cas { pub revision: u64, pub digest: String }

pub enum Caller {
    UserCommand { invocation: String },   // 只在可信进程内构造
    UserReview { invocation: String },
    Model { tool_call: String },
    Runtime { task: String },
    Migration { run: String },
    External,
}

pub struct AdmitRequest {
    pub op_id: OpId,
    pub caller: Caller,
    pub intent: MemoryIntent,            // subject_key/value/body/relation/evidence
    pub expected: Option<Cas>,
}

pub enum Admission {
    Applied { id: MemoryId, revision: u64, operation: Operation },
    Replayed { id: MemoryId, revision: u64, operation: Operation },
    Pending { id: MemoryId, reason: PendingReason },
    Skipped { existing: MemoryId, reason: SkipReason },
    Conflict { target: MemoryId, current: Cas, expected: Cas },
    Rejected { reason: RejectReason },
}

impl MarkdownStore {
    pub fn open(roots: ScopeRoots) -> Result<Self, StoreError>;
    pub fn scope(&self) -> MemoryScope;
    pub fn paths(&self) -> &StorePaths;
    pub fn read(&self, id: &MemoryId) -> Result<ReadRecord, StoreError>;
    pub fn history(&self, id: &MemoryId, revision: u64) -> Result<MemoryRecord, StoreError>;
    pub fn list(&self, query: ListQuery) -> Result<Vec<MemorySummary>, StoreError>;
    pub fn effective_active(&self) -> Result<Vec<EffectiveRecord>, StoreError>;
    pub fn admit(&self, request: AdmitRequest) -> Result<Admission, StoreError>;
    pub fn update(&self, op: OpId, caller: Caller, id: &MemoryId, patch: RecordPatch, expected: Cas) -> Result<Admission, StoreError>;
    pub fn supersede(&self, op: OpId, caller: Caller, new: NewRecord, targets: &[MemoryId]) -> Result<Admission, StoreError>;
    pub fn deprecate(&self, op: OpId, caller: Caller, id: &MemoryId, expected: Cas, reason: Reason) -> Result<Admission, StoreError>;
    pub fn pin(&self, op: OpId, caller: Caller, id: &MemoryId, pinned_by: PinOwner, expected: Cas) -> Result<Admission, StoreError>;
    pub fn unpin(&self, op: OpId, caller: Caller, id: &MemoryId, expected: Cas) -> Result<Admission, StoreError>;
    pub fn forget(&self, op: OpId, caller: Caller, id: &MemoryId, expected: Cas) -> Result<ForgetOutcome, StoreError>;
    pub fn expire_due(&self, now: DateTime<Utc>) -> Result<Vec<MemoryId>, StoreError>;
    pub fn list_pending(&self) -> Result<Vec<MemoryRecord>, StoreError>;
    pub fn review(&self, op: OpId, caller: Caller, pending: &MemoryId, decision: ReviewDecision) -> Result<Admission, StoreError>;
    pub fn recover(&self) -> Result<RecoveryReport, StoreError>;
}
```

- `CommitPlan` 是 lifecycle 决策 + store 写入的唯一桥；Stage 1 不暴露任意 `write_file`。
- `ReadRecord` 带 `record`、`digest`、`effective_authority`、`trust: Trusted|ExternalUnverified|ExternalConflict`、`cas`。
- `Caller` 不实现 `Deserialize`；测试可以构造所有变体，生产接线由 Stage 2 的可信路径映射。Stage 1 文档必须明确 store 信任调用方，不能把 `Caller` 暴露给模型 payload。
- `OpId` 由调用方显式传入；store 不按 turn 合成，不把 body 当幂等键。
- Stage 1 不实现检索；`MarkdownStore` 只提供 list/read/effective_active。检索的唯一生产路径在 Stage 3 的 `MemoryService::recall` + FTS5，避免先造一套临时 BM25 再删。

### 1.4 持久化协议

按设计文档 §6.6：

1. 获取 scope 排他锁。
2. 恢复未完成 journal；失败即停止。
3. 读目标 + receipt，校验 CAS。
4. lifecycle 决策（纯函数）。
5. secret scan（title/subject/subject_key/value/tags/body/source_refs.evidence_span/message_digest/extra/proposal/experience/extra）；命中则拒绝，不落盘。id/session/turn/legacy id 等标识符按结构校验，不套用自由文本 secret 规则，避免把 `k-apikey` 这类合法键名误判为凭据。
6. 准备事务文件：新版本写入 `<current_root>/.stage/<op_id>.md`；history、receipt、intent、pending、attic、tombstone 全部写为 state root 内的 `.tmp-` 文件。所有文件 fsync，目录 fsync。
7. 写 `<state_root>/journal/<op_id>.json` prepared + fsync（含 targets/expected/stage digest/state temp digest）。在 journal 落盘前不改 current。
8. 安装新文件：stage 文件 rename 进 current（同 FS），fsync current 目录。
9. 退役目标：重读旧 target 并校验 digest 未变；确保旧 revision 已有 history；删除 current 文件。
10. 落定 state 文件：state temp rename 为 history/receipt/intent/pending/attic/tombstone，fsync 目标目录。
11. 追加 suppressed/pending_history，删除已覆盖的 pending 文件。
12. 删除 forget/清理要求删除的目录。
13. journal 标记 committed + fsync。
14. 清理残留 stage/temp 文件，删除 journal，fsync journal 目录。
15. 释放锁。

实现细节：

- `write_atomically`：同目录 temp、`write_all`、`sync_all`、`rename`、`sync_dir(parent)`。Unix 用 `File::open(parent)?.sync_all()` 实测；非 Unix 的目录 fsync 不受支持，`fsync_dir` 是明确的 no-op，只保证 rename 原子性，不声明目录项持久性（Windows 认证见后续阶段，标记 UNVERIFIED）。
- `.stage/` 在 current root 内，只用于同文件系统 rename；绝不把 state 里的文件 rename 到 current。
- history 文件名 `{revision:04}.md`，内容不可变；重放时同 revision 同 digest 跳过，不同 digest → `RecoveryConflict`。
- receipt 在每次成功服务写入后更新：记录 revision/digest/status/authority/pin/subject_key，不存正文；receipt 缺失/不一致 → `ExternalUnverified`/`ExternalConflict`。
- tombstone 只写 `{id, kind, scope, subject_key_sha256, last_revision, content_sha256, forgotten_at}`；不含 title/body/value/evidence。
- `lock` 在 state root，fs2 排他写、共享读；超时 5s、10ms backoff（测试时间行为除外）；不 sleep 同步。
- `list` 不修改文件；pending 的读取/清理分开，读路径不删除。
- `External` 记录只读展示，不进入 `effective_active`；store 不提供任何“外部文件自动信任”入口。

### 1.5 测试

不改 `e3_lifecycle.rs`。新增 8 个测试文件，截至 2026-10-23 在 macOS 上共 69 个 active 测试（其中 1 个 Unix-only）、1 个 ignored helper：

| 测试文件 | 数量 | 实际覆盖 |
| --- | --- | --- |
| `tests/memory_v2_schema.rs` | 10 | frontmatter round-trip/未知字段、顶层重复键、anchor/alias、BOM/CRLF、缺 frontmatter/超限、schema 版本、全字段 secret scan（body/value/source_refs.extra）、ID 路径字符、Local 需 repo root、symlink root |
| `tests/memory_v2_store.rs` | 12 | create/read/CAS/update/history、OpId 重放与复用冲突、显式 supersede、deprecate、pin/unpin（模型 unpin → Pending）、TTL 重开、forget 正文副本+tombstone、forget 幂等、forget 残留无法删除时报错、list 过滤、无关文件忽略、experience 证据等级 |
| `tests/memory_v2_lifecycle.rs` | 7 | 模型伪造 authority 仍 Pending、review accept/reject（含 suppression）、accept 时 target 已变 → Conflict、Global/Runtime 需用户命令、External 拒写、Migration 不提权、未解析 experience 降级 |
| `tests/memory_v2_e3_contracts.rs` | 10 | E3 11 个契约的 store-level 版本；U2 拆成“同 body 不同 subject 保留并报告 possible_duplicate”和“同 subject 单当前值”，其余逐条标注 store-level；service-level 不伪装通过 |
| `tests/memory_v2_external.rs` | 7 | 无 receipt 伪造 user_explicit/pinned 不提权；正文/value 外改 → ExternalUnverified；revision/status/subject 外改 → ExternalConflict；外部删除 → ExternalConflict；合法写入恢复 Trusted |
| `tests/memory_v2_git_security.rs` | 8 | Local sentinel + `git check-ignore`、repo 根 `.gitignore` 不被创建、Project 仅忽略 `.stage/`/`.tmp-*`、state 在 repo 内被拒、repo 根不能作 Local root、非空目录不自动 ignore、body/value secret 不落盘 |
| `tests/memory_v2_crash.rs` | 10 | 构造 prepared journal 后恢复到各阶段（新文件已装、旧文件已退役、state 已落定）、第三次状态变更 → Blocked 并全拒绝、forget 不复活、tombstone 缺失可重建、target 变化阻止 forget、stage/temp 孤儿清理 |
| `tests/memory_v2_concurrency.rs` | 5 + 1 ignored | 4 线程 20 个 subject 不丢；6 写者同 subject 单 active；同 OpId 竞争一次 Applied 一次 Replayed；1 写 × 40 supersede + 3 读 × 200 次始终一个完整 body；跨进程 4 写者经 `current_exe` helper 串行化（helper 默认 ignored） |

测试事实与限制：

- 本轮的 Red→Green 只有两处真实行为缺口：pending suppression 的 fingerprint 只覆盖内部 relation、`Caller::Migration` 的 keyless 用户事实在 `admit_create` 被无条件要求 subject key。其余测试在写时针对既有实现，不能宣称全部 test-first。
- crash 测试没有 abort 子进程，也没有生产 failpoint；它按协议在磁盘上构造 prepared journal 的各个阶段，再由新 store 实例执行 `recover()`。这覆盖恢复矩阵，但不等同于真实进程崩溃注入。
- 读一致性测试放在 `memory_v2_concurrency.rs`，没有单独的 `memory_v2_read_consistency.rs`；读者共享锁 + 写者排他锁覆盖零 active/半提交。
- 跨进程写者使用 `std::env::current_exe()` 启动自身，不靠 sleep 同步。
- 全部测试只在 macOS 运行；Windows 的目录 fsync、rename、锁行为和跨文件系统组合均未验证。
- `cargo test -p leveler-memory` 与 `./dev arch` 通过；`cargo fmt --all -- --check` 仍因并行 WIP 文件失败，本任务拥有的 v2 文件经 `rustfmt --check` 通过。
- Stage 1 不接运行时，没有真实 CodeLeveler Dogfood；测试绿不能包装成产品验收。

E3 11 个契约的分层标注：

| E3 契约 | Stage 1 覆盖层 | 说明 |
| --- | --- | --- |
| U1 同一直接事实双入口单 active | store-level | 直接命令由可信 caller 写；extractor caller 同 subject 只能 Skipped/Pending |
| U2 同 body 不同 subject | store-level（刻意变更） | v2 不合并，锁“两条独立 + possible_duplicate”；与旧 WIP 测试预期相反，不修改旧文件 |
| U2 同 subject 复用单当前值 | store-level | identity + supersede |
| U3 keyless 修正 | store-level（partial） | 迁移 keyless 数据不在 Stage 1；store 能表达 legacy keyless + successor，生产迁移未验证 |
| U5 新会话只见当前值 | store-level（list/effective_active） | 不同进程读同一 roots 由 concurrency/read consistency 覆盖；真实 Session 未接 |
| U6a proposal 不能替换正式决定 | store-level | 受保护目标 + Model caller → pending |
| U6a hypothetical 不能替换 | store-level | statement 不含 hypothetical；提取层 Stage 2 |
| U6a confirmed correction 可替换 | store-level | User caller 直接 supersede |
| pending 被覆盖后清理 | store-level | pending 文件 + disposition |
| pending 不同 value 不被同 subject commit 删除 | store-level | pending 独立存放，不随 current 扫描 |
| 新 confirmed fact 成为 current | store-level | admit + receipt |
| 直接命令与 extractor 的生产接线 | service-level | Stage 2，NOT_VERIFIED in Stage 1 |
| 真实 TUI/Agent 自动提取链 | production-chain | Stage 2+，NOT_VERIFIED in Stage 1 |

### 1.6 停止条件

出现任一即停，不进入 Stage 2：

- 任何一个 **store-level** v2 E3 契约失败，且失败来自行为而非测试 fixture；service-level/production-chain 未覆盖要标 NOT_VERIFIED，不能伪装通过。
- crash/concurrency 测试在恢复后仍出现半文件、同 subject 两个 active、丢记录，或恢复只能靠人工。
- 读者能观察到零 active/半提交 supersede，或必须靠 sleep 才能稳定。
- current root 出现非 active/pending/history/journal 文件；Git 隔离测试失败。
- 外部编辑可以在无 receipt 的情况下提升 authority，或覆盖受保护用户事实。
- schema 需要靠猜测旧字段才能通过；需要新增语义词表/相似度。
- `./dev arch` 对新增 v2 代码报语义构造；例外只能登记已证明的文件名/协议事实（`.md`/`.json` 后缀、`.tmp-` 前缀），不能为自然语言猜测加例外。
- 引入第二个 SQLite 或需要第二个 store 才能完成 Stage 1。
- Windows 测试无法通过基本 round-trip；只能标记 UNVERIFIED 的部分不阻塞 Stage 1，但必须写清。

### 1.7 完成标准

- v2 schema 全部字段有测试；未知字段保留。
- create/update/supersede/forget/pin/pending 的 journal 恢复矩阵全覆盖。
- store-level E3 契约全绿；service-level/production-chain 明确列出未验证；legacy 测试仍全绿。
- 同一 scope/kind/subject_key 最多一条 active（keyless 单独报告）；恢复后无重复 active。
- 读一致性测试证明：读者始终看到完整旧状态或完整新状态，无零 active/半提交。
- Git 隔离测试证明：project current 仅 active + `.stage` + 窄 `.gitignore`；local 全 ignore；state 在 repo 外；`git status` 不出现内部状态。
- 外部信任测试证明：伪造 `authority`/`pinned`/`status` 不提权；受保护目标不能被模型直接覆盖；CAS 拒绝外部覆盖。
- Forget 测试证明：current/history/pending/index 正文副本删除，tombstone 无正文，崩溃不复活，重试幂等。
- 父目录 fsync 在 Unix 实测；Windows 明确标注未验证。
- 没有修改 legacy store 行为；没有接运行时；没有迁移真实数据。
- `cargo test -p leveler-memory` 通过；`./dev arch` 通过。
- 本任务拥有的文件 `rustfmt --check` 通过；全仓 `cargo fmt --all -- --check` 如果仅被并行 WIP 文件阻塞，必须记录具体文件，不得声称全仓通过。
- crash 证据必须写明是按 journal 协议构造状态还是真实进程 abort；两者不能混称。
- 代码 review 通过：无单次使用抽象、无重复状态、无静默吞错。

### 1.8 回滚

- Stage 1 不接运行时，回滚 = 删除 `v2` 模块及其测试/依赖，legacy 完全不受影响。
- 不产生 live 数据；没有用户可见行为。
- 如果依赖变更引起 workspace 问题，回滚 `Cargo.toml` 与 `Cargo.lock` 的本次改动（在授权后）。

---

## Stage 2：Unified Memory Service + Tool + Auto Learning

### 2.1 目标

把 v2 store 变成唯一写入/生命周期入口，并接入现有工具、自动提取、TUI/CLI：

- `MemoryService`：actor、intent、authority 推导、admission、CAS、pending review、read/list/recall。
- 模型工具 `remember`/`forget`/`memory` 全部走 service；TUI/CLI/Web 写命令也走 service。
- 自动用户事实/experience 通过 service 写入；修复 `known_identities` 生产接线。
- 配置 `memory.backend=legacy|markdown`，默认 legacy；Markdown 只用于测试/dogfood，不切换真实用户。
- 保留 `experience_enabled=false` 默认值。

### 2.2 代码范围

新增/修改：

| 位置 | 变更 |
| --- | --- |
| `crates/leveler-memory/Cargo.toml` | Stage 2 增加 `leveler-core`（复用 ID newtype）；Stage 1 不加 |
| `crates/leveler-memory/src/v2/service.rs` | `MemoryService`、`Actor`、`MemoryIntent`、`Admission`、`MemoryError`、review、expire |
| `crates/leveler-memory/src/v2/mod.rs` | 导出 service/lifecycle/store |
| `crates/leveler-memory/src/lanes.rs` | 不改；只读复用现有 lane 判定 |
| `crates/leveler-tools/src/tools/memory.rs` | `MemoryTool`/`RememberTool`/`ForgetTool` 改为 service adapter；保留工具名 |
| `crates/leveler-tools/src/registry.rs` | 注入 service/root；lane 开关不变 |
| `crates/leveler-agent/src/memory_extract.rs` | 支持传入 `known_identities`；生产调用走 context |
| `crates/leveler-app/src/memory_consolidator.rs` | `admit_*` 改为 `MemoryService::admit`；构造 identity context；experience 走 service |
| `crates/leveler-app/src/interactive.rs` | 直接命令、accept/reject、remember/forget 走 service；不直接 open store |
| `crates/leveler-app/src/lib.rs` | 按 `memory.backend` 构造 legacy 或 v2 service；默认 legacy |
| `crates/leveler-agent/src/executor.rs` | `maintain_memory`/direct command 走 service；Stage 2 不改召回（legacy backend 保持现状，markdown backend 的 search 明确不可用） |
| `crates/leveler-cli/src/memory_cmds.rs` | 写命令走 service；读命令兼容两种 backend |
| `crates/leveler-project/src/config.rs` | `MemoryConfig` 增加 `backend`（默认 `Legacy`）；不改 `enabled/user_enabled/experience_enabled` 默认 |
| `crates/leveler-client-protocol` | 若 pending/tool 结果需要新字段，做向后兼容的可选字段；不改写协议语义 |

不做：

- 不实现 FTS5/召回。markdown backend 下 `memory.search`/`vector_search` 在 Stage 2 明确返回 `not_available_until_stage_3`（不静默返回空结果，不假装成功）；list/read 正常。legacy backend 保持现状。Stage 3 用唯一 `MemoryService::recall` 替换，不保留临时 BM25。
- 不改 event `MemoryRecalled` 持久化（Stage 3）。
- 不改 UI 布局，只改数据来源。
- 不迁移真实数据。
- 不改变 `experience_enabled` 默认。

### 2.3 Actor / authority 实现

```rust
pub enum Actor {
    User { invocation: UserInvocationId },   // 可信交互命令；不是模型可传的参数
    Model { session: SessionId, turn: TurnId, tool_call: ToolCallId },
    Runtime { task: TaskId },
    Migration { run: MigrationId },
    External,
}
```

- ID 类型复用 `leveler-core`（`SessionId`/`TurnId`/`ToolCallId`/`TaskId`）；`MigrationId`/`UserInvocationId` 是 service 内部的新类型；不依赖 `leveler-agent`/`leveler-engine`。
- `Actor`/`UserInvocationId` 不实现 `Deserialize`，也不提供 `From<String>`；模型 tool payload、CLI argv、session `TurnId` 都不能构造 `Actor::User`。只有进程内可信命令处理路径（`/remember` 解析、TUI accept、CLI 交互入口）能 mint `UserInvocationId`。
- authority 由 `(Actor, EvidencePack)` 推导，`MemoryIntent.proposed_authority` 不作数。
- 用户直接命令 → `user_explicit`；TUI accept → `user_approved`；自动用户事实 → `user_stated`；模型工具 → `model_inferred`；experience → `runtime_observed`/`tool_observed`。
- 模型工具对受保护用户记录（receipt 为 `user_explicit`/`user_approved`）的 update/deprecate/forget/pin → 生成 pending，返回 `needs_user_review`；Full 模式也不绕过 actor 规则，不提升 authority。
- `review` 只接受 `Actor::User`；模型工具不注册 review action。
- 直接命令只拥有**它自己的那段事实**：有 subject_key 时按同 subject 跳过 extractor 候选；无 subject_key 时按 `(session, turn)` + 精确 evidence span 包含关系判定所有权，span 只属于该命令时 extractor 候选 `Skipped`，同轮其它独立事实仍按各自 OpId 准入，不做 turn 级去重。不使用正文相似度、标题哈希或同义词判定身份。模型工具同 subject 也不能覆盖直接命令产生的用户事实。
- 外部文件/无 receipt 的记录不进入 service 的可信读取路径；模型不能通过修改 Markdown 的 `authority`/`pinned` 获得用户 authority。

### 2.4 身份与 known_identities 接线

- `MemoryService::identity_context(scope, limit=24)`：从 active 记录构造 `{id, subject_key, subject, value, revision, kind, authority}`；排序 pinned_by=user、user_explicit、updated_at。
- `memory_consolidator` 在调用 `extract_batch_recorded_with_context` 时传入；不再用 `ExtractionContext::default()`。
- 测试锁定：真实调用路径收到 24 条以内 identity；模型输出 `replaces_memory_id` 必须是已知 id；未知 id 被拒绝/降级 pending。
- 这是 U3/U5 生产可达性的关键修复；Stage 2 完成标准包含它。

### 2.5 Pending / review

- pending 是完整 Markdown 记录，status=pending，带 `proposal`；只存放在 `<state_root>/pending/`，不进入 current root/Git。
- `review(actor=User, pending_id, accept/reject)`：
  - accept：重新校验 pending digest 与 proposal 的 `expected_revision`/`expected_digest`，按 operation 走完整提交协议；`user_approved`；CAS 失败 → conflict，不覆盖。
  - reject：删除 pending，写完整 intent digest 到 `<state_root>/suppressed.jsonl`；不写正文。
- TUI/CLI/Web 的 accept/reject 由可信用户路径 mint `Actor::User`，全部走 service；不再直接操作 `pending/` 目录。
- 旧 pending 不自动 active；legacy backend 下保持旧行为，v2 backend 下按新契约。
- 外部放入 current 的 `status=pending` 文件不是合法 pending，按 `ExternalUnverified` 处理。

### 2.6 自动提取

- 用户事实：fresh turn only（保持现有 inbox 触发器）；proposal/hypothetical 不写入；精确 evidence span；secret scan 含 value；同 subject 不同 value 按 §6.5 规则。
- experience：failure+resolution 由 runtime receipt 构造；`EnvironmentConstraint` 最多 observed；模型声称 verified 无效。
- 失败/重试：inbox 保留，boot id 恢复；service 错误分类可重试/终态；不把校验失败当 provider 失败。
- 不新增 LLM 调用；沿用现有 extractor batch。
- continuation 用户消息暂不扩展（见设计 §14）；如果未来扩展，必须先有真实 user-authored message 来源，不解析 objective。

### 2.7 测试 / Dogfood

测试：

| 测试文件 | 覆盖 |
| --- | --- |
| `tests/memory_v2_service.rs` | actor 矩阵、authority 推导、CAS、显式 OpId 幂等/重放/复用冲突、pending accept/reject、pin、forget、scope、receipt 更新、外部记录不进入可信路径 |
| `tests/memory_v2_tool.rs` | 三个工具的动作、模型不能自升 authority、不能 review、Full 不绕过、错误语义、Stage 2 markdown backend 的 search 明确 `not_available_until_stage_3` |
| `tests/memory_v2_extraction.rs` | known_identities 接线、evidence span、proposal/hypothetical、value secret、同 subject 直接命令优先但不同 subject 不被 turn 级去重、continuation gap 记录 |
| `tests/memory_v2_parity.rs` | E3 11 场景在 legacy 与 v2 上参数化运行；结果一致或差异有明确原因（同 body 不同 subject 的差异必须记录为刻意变更） |
| `tests/memory_v2_direct.rs` | 可信用户命令 + extractor + 模型工具；模型伪造 User actor 不可能；交互式无 attachments 与有 attachments 的差异记录 |

Dogfood（小规模，真实 CodeLeveler + DeepSeek Flash）：

1. 临时项目 + `memory.backend=markdown`（测试配置，不提交）。
2. 用户 `/remember` 写一条 project fact；模型 `remember` 写一条 model_inferred；检查 Markdown/frontmatter/authority。
3. 模型尝试 update 用户 fact → 返回 pending；TUI accept → 记录 revision/history。
4. 自动提取（fresh turn）写一条 user_stated；检查 known_identities 传给 extractor（测试日志/ledger，不含用户原文）。
5. experience lane 在测试配置开启，构造一次 failure→resolution；检查 standing/runtime evidence；不改默认配置。
6. 重启进程，读回 Markdown；确认 current 唯一、pending 不注入。
7. 检查 `git status` 无 secret、无意外文件。

### 2.8 停止条件

- 任何一条写入还能绕过 service（工具/CLI/TUI/worker 任一）→ 停。
- 模型能通过 tool payload、CLI 参数或 Markdown frontmatter 构造 `Actor::User`/提升 authority → 停。
- known_identities 在生产调用路径仍为空 → 停。
- 模型工具能在 Full 下直接改受保护用户记录/自升 authority → 停。
- 同 body 不同 subject 被服务合并，或同 turn 多条独立事实被 OpId 去重吞掉 → 停。
- legacy 与 v2 的 E3 契约差异无法解释，或 v2 需要语义相似度才能通过 → 停。
- 默认配置被改变（experience 开启、backend 默认 markdown）→ 停。
- 真实用户数据被写入/迁移 → 停。

### 2.9 完成标准

- 所有生产写路径经 `MemoryService`；代码搜索不再有直接 `MemoryStore::activate/commit_candidate/accept` 的生产调用（legacy backend 适配层除外）。
- 模型工具/自动提取/TUI/CLI 的 actor 与 authority 测试全绿；`Actor::User` 无法从模型 payload/CLI argv 构造。
- 受保护用户记录的更新只能 pending 或经可信 review；Full 模式不改变。
- 外部编辑/无 receipt 记录不进入可信路径；receipt 与 CAS 测试通过。
- OpId 幂等、同 OpId 冲突、同 turn 多事实测试通过；不存在 turn 级去重。
- known_identities 在真实 consolidator 路径非空，`replaces_memory_id` 可命中。
- pending 永不注入；accept 是唯一激活路径；模型不能 review；pending 只在 state root。
- legacy backend 默认不变；v2 backend 只在测试/dogfood 使用。
- `cargo test -p leveler-memory -p leveler-tools -p leveler-agent -p leveler-app` 相关目标通过；`./dev arch` 通过。
- 没有改动 `e3_lifecycle.rs` 并行 WIP。

### 2.10 回滚

- `memory.backend` 切回 `legacy`；v2 files 不删除，作为测试/迁移输入。
- 如果 v2 写入过测试项目，删除测试项目即可；真实用户 store 未被触碰。
- 代码回滚：撤掉 service 接线，保留 legacy 路径；不删除 legacy 代码。
- 不承诺 v2 测试数据可反向迁移；Stage 2 不用于真实数据。

### 2.11 Stage 2.1 正确性收口（2026-10-23）

Stage 2.1 只修正确性与证据链，不进入 Stage 3：没有 FTS5、Context Builder、Logged Injection，没有真实数据迁移，默认 backend 与 Experience 默认开关未变。

#### 2.11.1 无 subject 的显式用户记忆

- 显式 `/remember` 不需要 Subject。直接命令以 Memory ID 作为身份；`(scope, id)` 是其唯一寻址方式。自动提取与模型写入仍必须有 `subject_key`，无 subject 的自动 UserFact 在 Store 入口即 `Rejected{MissingSubjectKey}`，不会伪造 Subject、标题哈希或关键词身份。
- 直接命令的 `OpId` 由可信 `UserInvocationId` 派生（`OpId::derived`，journal-safe）；同一调用重试返回 replay，两次独立命令是两个身份。
- 直接命令只拥有它自己的精确 evidence span：extractor 候选的 `evidence_span` 必须包含于该命令正文时才 `Skipped{DirectCommandOwned}`；同轮其它独立事实按各自 OpId 正常准入，不做 turn 级去重，也不做正文相似度或同义词合并。
- 按 ID 更新：接受 pending update 后目标 ID 不变，旧 revision 进 history，新 revision 的 `source_refs` 追加本次 actor provenance；不是 supersede，也不是新身份。
- Subject 补齐：`MemoryService::update` 可在 service 层为既有无 subject 记录附加结构化身份；模型工具不能改写身份（`subject: None`），冲突进 Pending（`subject_key_in_use` / `ProtectedUserFact`）。

#### 2.11.2 多 scope Pending review

- Pending 身份是 `(scope, id)`；同一 ID 在不同 scope 是两条独立记忆。
- `AcceptMemory` / `RejectMemory` / `ForgetMemory` / `RememberMemory` 都携带 `UiMemoryScope`（旧客户端缺省为 project）。Accept/Reject 只作用于指定 scope。
- Store → MemoryService → Pending List → Client Protocol → TUI → User Review Event → MemoryService 全链路按 scope 传递；TUI 行键为 `"{scope}:{id}"`，中文标签 全局 / 项目 / 本地，窄屏同样可见。
- Pending 永不注入。接受前按目标 CAS（revision + digest）校验；目标已变时拒绝旧 UI 的覆盖，不静默改写。
- 权限：真实 TUI 用户评审可以 accept；模型工具没有 accept/reject 动作；CLI/客户端的 `UserInvocationId` 由可信投递路径（`command_id`）在运行时侧铸造，argv 不能伪造；Full Access 不改变域 Authority；headless `run` 保持 Pending；没有第二套审批机制。

#### 2.11.3 Worker 恢复与幂等

- 取消不再被当成“没有可学内容”：`ensure_running` 在提取前后各检查一次，取消时 claim 保持 `failed_retryable`，不写入 Store。
- 崩溃点测试：
  - 模型已返回、落盘前崩溃 → 重新提取，事实只写一次；
  - staged answer 已落盘、应用前崩溃 → 回放 staged answer，不再调用 provider；
  - 部分候选有效 → 有效者应用，无效者计入 `rejected`，不整体丢弃；
  - provider 失败 → `failed_retryable`，Store 不出现 Active/Pending。
- 重复处理同一答案不会产生第二个 Verified；Experience 的 `verified` 只来自 runtime 观测到的 failure → resolution 证据。

#### 2.11.4 真实 E3 / Dogfood 状态

2026-10-23 使用真实 DeepSeek Flash、隔离 `LEVELER_HOME`、隔离项目与 Store 运行（lab 目录 `dogfood/scratch/m2-stage21-*`，未读写真实用户 Memory）：

| 场景 | 结果 | 证据 |
| --- | --- | --- |
| U1 无 subject 的 `/remember` | PASS | 一条 `user_explicit`、无 subject、无 pending 的 Active；`user_turn` span + `user_command` invocation |
| U2 自然重述 / extractor 收敛 | NOT_VERIFIED | 两轮自然重述后 extractor 未产生新候选（既有 current 事实的确认按契约不生成候选），因此没有可观察的重复写入；未出现重复 Active |
| U2b 工具身份收敛 | PASS | 同一 subject 的相同写入被 `Skipped{Identical}`，不同 subject 保持两条 Active |
| U3 按 ID 纠正 | PASS | 模型 update 先 Pending；TUI `/memory accept project:<id>` 后同一 ID revision 2，history 保留旧值，队列清空 |
| U5 跨会话一致性 | PASS | 新会话看到同一 ID、revision 2、旧 revision 与空的待确认队列 |
| U6a 受保护事实 | PASS | extractor 的结构化 supersede 因 `protected_user_fact` 停在 Pending；非正式建议未改变正式值；模型按 ID 更新仍停在 Pending |
| D6 多 scope review | PASS | 全局与本地 pending 在 TUI 中带中文 scope 标签，接受后落入各自 scope；64 列窄屏仍显示 `[全局]` / `[项目]` |
| P11 重启 | PASS | 同一 ID 与 revision 在重启后仍是 current |
| P14 无人值守 global 写入 | PASS | Assisted 下停在 Pending，未成为 Active |
| P15 headless | PASS | 非交互 `run` 的 global 写入停在 Pending |
| P16 user lane 关闭 | PASS | 工具与自动提取都没有写入 |
| P12/P13 Experience | PASS | 真实 `cargo test` 失败（exit 101）→ 修复 → 通过（exit 0），生成 `runtime_observed` 且 `verification:verified` 的 experience；lane 关闭时不写入 |

未验证 / 不声称：

- U2 的“自动 extractor 在跨轮重述时收敛”没有被真实模型触发；当前证据只到“没有重复 Active”和“工具路径精确收敛”。
- U6a 的“模型按 ID 更新”一轮中，该目标此前已有 pending proposal，模型的尝试走的是既有队列，没有新增 proposal；受保护值未变这一属性成立。
- 真实进程崩溃 / 真实 provider 中断的恢复由确定性测试覆盖，没有在生产进程中注入崩溃。
- 跨平台（Windows 文件语义）未验证。

#### 2.11.5 剩余风险

1. 直接命令正文里嵌套的较窄事实仍可能被 extractor 生成第二条结构化记录；当前明确不做正文相似度合并，需要后续显式身份收敛。
2. 真实 UI 还没有“给无 subject 的用户记忆补 subject”的入口；service 层已支持，UI 未实现。
3. Pending 行本身的 UI 内容没有版本校验：accept 时按当前 pending 重新读取；target 的 Update/Supersede 有 CAS，但 UI 展示内容与提交内容之间没有独立 digest 回传。
4. `direct_owner_for` 依赖精确 span 包含关系；措辞 paraphrase 的跨轮重述不会被判定为同一事实，这是不引入语义相似度的明确边界。

---

## Stage 3：FTS5 Index + Context Builder + Logged Injection

### 3.1 目标

- FTS5 作为可重建 derived index，支持中英文/代码标识符、scope 隔离、增量更新、损坏重建。
- `MemoryContextBuilder` 成为唯一注入 Owner：standing + candidates + experience，预算、去重、authority 标签、按需 read。
- recall block 放到 uncached tail；catalog 仍为 SessionPrefix，但由同一 builder 生成并记录版本。
- catalog 失效有明确契约：版本/失效触发/tail delta/run 边界重建/resume/compaction；不在每轮重建整个 system prefix。
- `MemoryRecalled` 持久化，记录实际注入的 id/revision/scope/kind/mode/token/digest/omission；区分历史注入与当前 Memory。

### 3.2 代码范围

| 位置 | 变更 |
| --- | --- |
| `crates/leveler-memory/src/v2/index.rs` | FTS5 schema、tokenize 预切分、增量更新、重建、损坏恢复、查询 |
| `crates/leveler-memory/Cargo.toml` | 增加 `rusqlite = { version = "0.32.1", default-features = false, features = ["bundled"] }`；锁 `libsqlite3-sys 0.30.1` |
| `crates/leveler-agent/src/memory_context.rs` | `MemoryContextBuilder`、block 格式、预算、选择、omission、catalog |
| `crates/leveler-agent/src/executor.rs` | `relevant_memory_injection` 调 builder；recall segment 移到 tail；删除/修正 stale 注释 |
| `crates/leveler-agent/src/prompt.rs` | catalog 从 prompt 直接生成改为 builder；guidance 保留 |
| `crates/leveler-engine/src/event.rs` | `MemoryRecalled` 不再 transient；payload 扩展；文档修正 |
| `crates/leveler-storage/src/event_repo.rs` | 持久化兼容；schema_version 不变（payload 是 JSON） |
| `crates/leveler-client-protocol/src/event.rs` | 可选字段 catalog/injections/omitted；旧客户端兼容 |
| `crates/leveler-tui` / `apps/leveler-desktop` | 显示注入审计/`/memory` 状态；不新增写路径 |
| `crates/leveler-app/src/lib.rs` | 构造 index/service；启动 reindex；状态报告 |

不做：

- 不引入 embedding/vector DB/LLM Judge。
- 不新增第二套 Session Log；只用现有 events 表。
- 不在每轮 recall 扫描整个目录。
- 不把 index 作为身份/生命周期权威。
- 不改 `experience_enabled` 默认。

### 3.3 索引实现要点

- `documents` + `documents_fts` 如设计 §11.4；`content_sha256`、`mtime_ns`、`size_bytes`。
- 写入：token 文本由现有 `tokenize` 生成；CJK bigram + unigram；ASCII/camel 保留；查询同样预切分，双引号转义。
- 更新：每次 service 写入成功后事务更新；失败标 `dirty`，不回滚 Memory。
- 重建：DB 缺失/损坏/版本不符 → 全量到 tmp + fsync + rename；启动/显式 reindex。
- 读取：index 命中后读 Markdown 校验 digest/revision；不信任 index title/body。
- 并发：WAL、busy_timeout、scope 独立 DB；写入重试一次，仍失败标 dirty。
- 规模：10k fixture；p95 < 20ms、重建 < 2s、context build < 10ms（验收目标，未达标先修，不扩大预算掩盖）。

### 3.4 Context Builder 实现要点

- 输入：last user message（tool continuation 用 goal/plan 文本）、lanes、`ResolvedContextPolicy`、scope。
- 选择：FTS top 12/scope user + top 6/scope experience（最终 candidates 最多 8 条 user + 2 条 experience；standing 最多 8 条）；active-only；`ExternalUnverified` 排除；sensitive 排除；同 `subject_key` 跨 scope 去重（结构化）；不同 subject 的相同 body 不去重，只标 `possible_duplicate`；standing pinned/user constraints；candidates 摘要。
- 预算：`usable = pressure_threshold or 8192`；memory 4% clamp 192–1200；standing ≤600；experience ≤300；用 `estimate_tokens`；omission 记录原因。
- 格式：固定 header（untrusted/advisory/no permissions）；`[scope][authority][id rN]`；model_inferred 标签；sensitive 只计 omission。
- catalog：active user_fact、非 sensitive、最多 16 条，SessionPrefix；只 title/id；首次 run 记录 `catalog_ids`/`catalog_digest`/`catalog_version`。
- catalog 失效：任何 active 集合/可见字段变化递增该 scope 的 `catalog_version` 并更新 `catalog_digest`（写入 `<state>/catalog/<scope>.json`，无独立事件）；run 内不重写 prefix，下一轮 tail 由 Builder 对比已注入快照生成受限 `catalog_delta`（≤8 条/1KiB），超过阈值只标 `catalog_stale` + 版本；run start/resume/compaction 重建完整 catalog；forget 的 delta 不含 title/body。
- tail：recall segment 不进入 leading system prefix；`PrefixSegment` 只保留 guidance/catalog。需要确认 `ControlContext`/encoder 支持 tail system block；如果不支持，新增一个 tail segment 类型，但只保留一个注入 Owner。
- 按需 read：`memory.read` 返回完整正文 + revision；非 active 返回 `not_current`；`ExternalUnverified` 可读但标注未验证、不进入 effective；每轮 read 4 次/8k tokens。
- 子代理：继承读取 scope；catalog 为空；写入 model_inferred/pending。
- resume/compact：重新组装；不重放旧 block；catalog 按版本重建并记录。

### 3.5 Logged Injection 实现要点

- `MemoryRecalled` payload 扩展：injections、omitted、budget、block_digest、catalog_ids、catalog_digest、catalog_version、catalog_delta_ids、degraded。
- 不保存 body；sensitive title `[redacted]`；forget 后只保留 digest/id，不保留 title；secret scan 后写事件。
- 0 条也写；事件在请求发起前写；只声称“已组装”，不声称 provider 收到。
- 历史注入从 events 读；当前 Memory 从 Markdown 读；两者不混用。
- LocalOnly 数据类不变；不进入 public projection/远端。

### 3.6 测试 / Dogfood

测试：

| 测试文件 | 实际覆盖 |
| --- | --- |
| `tests/memory_v2_index.rs` | 中文 1/2/3+ 字、英文/代码标识符（含 CamelCase 及其分词）、混合、scope 隔离、active-only、外部编辑不注入旧正文、索引删除/损坏后从 Markdown 重建、status/lane 过滤、增量 update、supersede、pending/credential 排除、10k 规模（ignored）。尚无独立 FTS5 可用性探测、索引并发读写、版本升级或 rebuild 失败用例。 |
| `tests/memory_v2_context.rs` | 跨 scope 同一 subject 取最具体、预算随 resolved policy、损坏索引显式降级并保留 standing、外部编辑正文不注入；Builder 单元另覆盖预算公式与同 subject 去重。sensitive/omission/空结果主要经 index 测试与 Builder 单元路径覆盖，没有独立端到端用例。 |
| `tests/memory_v2_injection.rs` | MemoryRecalled 形状、请求实际内容与 receipt 一致（含 catalog version/digest、block digest 与请求行一致）、supersede 后只召回替代者、词法相关性、experience 模式、关闭 experience lane；0 条由 executor 单元测试覆盖；resume/compact 与历史 vs 当前只有 dogfood 覆盖。 |
| `tests/memory_v2_tail.rs` | recall 在 tail、guidance/catalog 在 prefix；run 内 delta 不改 prefix、removed delta 无 title/body、run start 重建；catalog version/digest 另见 `tests/memory_v2_injection.rs` |
| `tests/memory_v2_subagent.rs` | 子代理继承父 scope/lane 并在 tail 召回、不携带顶层 catalog；尚无子代理写入 model_inferred/pending 的用例。 |
| `tests/memory_v2_legacy.rs` | Legacy backend 的 recall 仍到达请求、位于 tail、两轮 prefix 稳定、receipt 不声称 catalog/budget。 |

catalog 失效由 `<state>/catalog/<scope>.json` 的版本/digest 与 Builder 的结构化对比实现，没有独立的 `CatalogInvalidated` 事件；run start 的完整重建由 `memory_v2_tail.rs` 覆盖，resume/compaction 重建只有 dogfood 覆盖；超阈值 `catalog_stale` 与 TTL 变化尚无独立用例。

Dogfood（真实 CodeLeveler + DeepSeek Flash）：

1. 用真实项目记忆副本（不是真实 store）建 v2 index；跑固定 query corpus，比较旧 BM25 与新 FTS5 的 top-k；记录 `legacy_only/v2_only`，不自动调参。
2. 真实会话：中文 2 字 query、英文标识符、代码符号；确认 candidate 摘要 + `memory.read` 能取到正文。
3. 检查每轮 `MemoryRecalled`：id/revision/token/omission/catalog；0 条也记录；resume 后重新组装。
4. 检查 prompt：recall 在 tail，prefix 稳定；catalog 只在首次 run 出现；修改一条 catalog 内记录后，下一轮只有 tail delta，run start 才重建完整 catalog。
5. 性能：10k fixture p95、context build、index rebuild；记录实测，不把目标当结果。
6. 隐私：sensitive 不出现；事件无 body/secret；远端投影不含注入详情。

### 3.7 停止条件

- FTS5 中文 2 字查询失败，或需要人工停用词/同义词表才能通过 → 停；回到设计评审，不硬上。
- index 命中后无法用 Markdown digest 校验，或 index 成为唯一内容来源 → 停。
- recall 仍在 leading prefix，或每轮扫描目录 → 停。
- `MemoryRecalled` 仍 transient，或记录 body/secret → 停。
- 上下文预算需要靠截断 id/title 才能过 → 停（先调选择策略）。
- 事件持久化破坏旧客户端/迁移 → 停。

### 3.8 完成标准

- index 可删除重建；删除后 recall 只降级 standing，不崩、不返回旧值。
- 中文/英文/代码 query 实测通过；scope 隔离；active-only。
- 每轮注入有持久事件；id/revision/scope/kind/mode/token/digest/omission 齐全；0 条也记录。
- recall 在 tail；prefix 不因每轮 recall 变化；catalog 失效有版本与 tail delta，不在 run 内重写 prefix。
- sensitive/凭据零注入；事件零 body；forget 后事件/ delta 无 title/body。
- 规模目标实测达标，或明确记录未达标原因。
- `cargo test -p leveler-memory -p leveler-agent -p leveler-engine` 相关目标通过；`./dev arch` 通过。

### 3.9 回滚

- 配置关闭 index：`memory.index=enabled|disabled`（新增，默认 enabled；legacy backend 不读）。关闭后只注入 standing，事件 `degraded=index_unavailable`。
- recall 位置/格式回滚：保留旧 `relevant_memory_injection` 的 feature flag（短期），确认新路径稳定后删除；不长期双路径。
- 事件扩展是可选字段，旧客户端忽略；如果 UI 不兼容，回滚 UI 读取，不删事件。
- index 文件可删除；Markdown 不受影响。

---

## Stage 3.1：性能根因修复、TUI 管理页与引用可见性

状态：2026-10-23 实现并完成 Phase A 验证（性能基准、生命周期测试、受影响测试）。真实多轮 Dogfood 的 Catalog Delta / Prefix Cache / Lane 关闭三项见 §3.1.7，属于本轮未完成项。

### 3.1.1 实测缺陷与根因

Stage 3 交付后 10k Memory 的 Context Recall p95 为 1980.829ms，而单次 FTS5 查询只有 10.722ms。本轮用同一 Store 形态复现并定位（debug 构建，10k，60 次 recall）：

| 指标 | 修复前 |
| --- | ---: |
| recall p95 | 1.668s |
| `Store open` 次数 / 60 次 recall | 660 |
| `Orphan cleanup` 次数 / 60 次 recall | 660 |
| 单次 `MarkdownStore::open`（warm） | 144.6ms |

根因是 Store 生命周期而不是检索算法：`MemoryService` 每次读取都调用 `v2::MemoryService::open`，而 `MarkdownStore::open` 每次都做目录创建、canonicalize、Git 边界检查、独占加锁、journal recovery，并在 `recover_locked` 末尾无条件 `cleanup_orphans`——后者遍历 stage/history/pending/attic/tombstones/receipts/intents/journal/catalog/index 以及**每条记录一个历史子目录**。10k 记录时每次 open 约 145ms，一轮 recall 打开 11 次，且写入路径同样付费。

### 3.1.2 修复

| 位置 | 变更 |
| --- | --- |
| `crates/leveler-memory/src/service.rs` | facade 按 Scope 缓存已初始化的 `Arc<v2::MemoryService>`。缓存的是**路径句柄**不是事实：每次 `read` 仍读当前 Markdown 并比对 receipt digest/revision/status/subject_key |
| `crates/leveler-memory/src/v2/store.rs` | `cleanup_orphans` 移出 `recover_locked`，只在 `open` 与显式 `recover()` 执行；新增 `read_many`（一次共享锁 + 一次 journal 检查）；新增进程级生命周期计数器 `lifecycle_metrics()`（opens / journal_recoveries / orphan_cleanups / reads） |
| `crates/leveler-memory/src/v2/index.rs` | `documents(scope, kind, status, …)` 二级索引；reconcile 后 `optimize`；`stored_many` 单锁批量校验；`stored` 保留 |
| `crates/leveler-agent/src/memory_context.rs` | standing 选择改为一次批量读取；检索候选按 chunk 校验，命中即停（正常情况每候选 1 次读，而非 4 次） |

保持不变的一致性契约：Revision、Digest、CAS、外部编辑检测、跨进程写入一致性、Journal Recovery、Forget 不复活、Supersede 只读当前版本、Multi-Scope 隔离、Pending 不注入。Store handle 缓存不跳过跨进程崩溃恢复：另一进程留下的 prepared journal 由本进程下一次加锁读取发现并恢复，见 `tests/memory_v2_crash_injection.rs::a_long_lived_handle_recovers_a_foreign_crash_before_its_next_read`。

### 3.1.3 Orphan Cleanup 生命周期

| 阶段 | 是否扫描 |
| --- | --- |
| `open`（含进程启动首次打开） | 是，一次 |
| 显式 `recover()` | 是，一次 |
| 正常读 / list / search / standing | 否 |
| 正常写事务 | 否（写入完成时 `finish_plan` 清理自己准备的 stage/temp） |
| 异常退出后的下一次 `open` | 是 |
| 跨进程 prepared journal | 由 `lock_shared_recovered` 触发 recovery（不触发 orphan 扫描） |

`tests/memory_v2_store.rs::orphan_cleanup_is_a_lifecycle_event_not_a_read_cost` 用真实文件锁定该契约。

### 3.1.4 性能结果

同一 fixture（current + history + receipts + tombstones + pending + journal + FTS index）、同一机器、`cargo test --release -p leveler-agent --lib memory_scale -- --ignored --nocapture`：

| 规模 | 修复前 p95（debug） | 修复后 p95（debug） | 修复后 p95（release） | Store opens / 60 recalls |
| ---: | ---: | ---: | ---: | ---: |
| 100 | 27.3ms | 1.29ms | 0.56ms | 0 |
| 1,000 | 152.9ms | 1.71ms | 1.16ms | 0 |
| 10,000 | 1,668ms | 25.9ms | 7.63ms（user lane）/ 15.8ms（user+experience） | 0 |

10k release 分解（单 scope）：FTS 查询 6.34ms、候选校验（12 条）≈0.8ms、standing 查询 0.054ms、standing 批量读 8 条 ≈0.6ms、目录/预算组装 <0.3ms。Cold open 仍为 155ms（一次性 recovery + orphan 扫描），catalog 首次 197ms、warm 0.1µs，index rebuild 10k 688ms。

Gate 判定：

| 目标 | 结果 |
| --- | --- |
| FTS5 10k p95 < 20ms | PASS（4.32ms，release，`memory_v2_index::ten_thousand_memories_index_scale`） |
| Index rebuild 10k < 2s | PASS（0.69s，service 路径；纯 reconcile 210ms） |
| Context Builder 10k p95 < 10ms | PASS（7.63ms，user lane = 出厂配置 `experience_enabled=false`） |
| 同上，experience lane 有记录时 | 未达标（15.8ms）。原因：两个 lane 需要各自深度的 FTS 排名，而 FTS5 的 `bm25()` 不能用于子查询/窗口函数（实测报错 `unable to use function bm25 in the requested context`），per-kind top-K 必须两次扫描，每次 6.3ms。未达标不做预算/候选削减；下一步可选方案是按 lane 拆分 FTS 表，使 experience lane 的扫描规模与其记录数成正比 |

### 3.1.5 TUI 管理页收口

`/memory` 在既有 Home / Review / Active / Archived / Detail 结构上补齐：

- 列表：`/` 关键词筛选（标题+正文，大小写不敏感子串）、`s` Scope 循环（全部/Global/Project/Local）、`t` Lane 循环（全部/用户记忆/经验）、`x` 清除。筛选是视图，只决定画哪些行；运行时 listing 仍是唯一权威，筛选生效时页头常显「筛选 + 当前条件 + 命中数」，隐藏行必须可见。
- 行结构：`● 标题` + 第二行 `Scope · Kind · Status · Authority · rN`（`●`=用户固定，`○`=未固定）。
- 详情：完整正文按宽度折行不截断；元数据块显示 `scope:id`、Scope、Kind、Status、Authority、Revision、Updated、Pinned；`y` 复制正文、`Y` 复制 `scope:id`（不含 TUI 边框/缩进），复制回执区分「原生确认」与「OSC 52 未确认」。
- 动作：`p` 固定/取消固定（可逆，一次按键）、`d` 标记过期、`f` 归档。后两者需同一键按两次，任何其它按键或离开详情都会撤销确认；运行时未报告 revision 时拒绝发送 Deprecate（不猜写）。
- 所有动作经 `SetMemoryPin` / `DeprecateMemory` / `ForgetMemory` 到 `MemoryService`；远端设备一律 DENY（`remote-protocol` 策略），TUI 不写 Markdown、不改 authority。

### 3.1.6 引用可见性

`MemoryRecalled` 增加可选字段 `injected`（scope、bounded title、mode），由 builder 的 injection receipt 生成；body 永不进入该事件。`/memory` Home 显示「最近一次召回：N 条」与最多 5 条记录标题，标题**从当前 listing 解析**：已被 forget 的记录只计入「其中 M 条已归档或不再提供」，不再显示标题。transcript 仍只保留 count-only 行。

### 3.1.7 本轮未完成

| 项 | 状态 |
| --- | --- |
| 真实多轮 Catalog Delta（真实 TUI + 真实 Provider） | 未执行（单元/集成层已有 `memory_v2_tail.rs`、`memory_v2_injection.rs` 覆盖） |
| Markdown Backend Prefix Cache 验证 | 未执行（同上，run 内 prefix 稳定性由 `memory_v2_tail.rs` 覆盖，但未取自真实 wire） |
| User Memory Lane 关闭的真实验收 | 未执行（`leveler-cli` 的 lane 关闭契约测试在本轮 HEAD 已失败，见交付说明） |

---

## Stage 4：Migration + Full Dogfood + Cutover

### 4.1 目标

- 实现迁移工具的 inventory/backup/convert/validate/shadow/cutover/rollback。
- 在隔离副本上完整演练；真实数据迁移需单独批准。
- 真实 CodeLeveler + DeepSeek Flash 完整 Dogfood：用户 Memory、Experience、检索、存储、迁移。
- 文档同步；legacy 进入只读/退休流程。

### 4.2 代码范围

| 位置 | 变更 |
| --- | --- |
| `crates/leveler-cli/src/memory_migrate.rs`（或同级模块） | `leveler memory migrate` 子命令：`--inventory`、`--backup`、`--convert`、`--validate`、`--shadow`、`--cutover`、`--rollback`；`--dry-run` 默认 |
| `crates/leveler-memory/src/v2/migrate.rs` | 只读 legacy JSON → v2 records 的转换、映射、冲突组、secret quarantine、manifest |
| `crates/leveler-memory/src/v2/validate.rs` | 条数守恒、链接、schema、索引、secret、shadow compare |
| `crates/leveler-project/src/config.rs` | `memory.backend` 最终默认切换只在用户批准后；迁移配置只读 |
| `crates/leveler-app/src/lib.rs` | backend 选择、cutover 启动检查、legacy read-only |
| `docs/ARCHITECTURE*.md`、`SECURITY.md`、`CHANGELOG*`、`README*` | 语义/路径/安全/迁移说明 |
| `docs/ARCHITECTURE_GUARD.exceptions` | 只有 guard 误判且测试证明时才加 |

不做：

- 不在本轮迁移真实数据。
- 不自动 commit/push。
- 不删除 legacy store；不自动清理备份。
- 不改变 `experience_enabled=false` 默认。
- 不把迁移报告/备份放进 repo。

### 4.3 迁移工具接口

```text
leveler memory migrate inventory --project <root> [--legacy-root <path>] --report <dir>
leveler memory migrate backup    --project <root> [--legacy-root <path>] --backup-dir <dir>
leveler memory migrate convert   --project <root> --run <id> --isolated-dir <dir>
leveler memory migrate validate  --run <id> --isolated-dir <dir>
leveler memory migrate shadow    --run <id> --queries <file> --report <dir>
leveler memory migrate cutover   --run <id> --confirm-cutover
leveler memory migrate rollback  --run <id> --confirm-rollback
```

- 默认 `dry-run`；破坏性/写 live 需要显式 `--confirm-cutover`。
- 不接受模型调用；只作为用户 CLI。
- 每个子命令输出 JSON/markdown 报告到指定目录；不打印 body/value/secret。
- `--legacy-root` 用于大小写/多 store 歧义；不接受自动扫描后合并。

### 4.4 测试 / Dogfood

测试：

| 测试文件 | 覆盖 |
| --- | --- |
| `tests/memory_v2_migrate.rs` | 9 条真实形态 fixture、字段映射、status/kind/scope/authority、keyless、derived/ephemeral、pending、archive、links |
| `tests/memory_v2_migrate_conflict.rs` | 同 subject 不同 value → conflict group + pending；同 body 不同 subject → 保留；精确重复折叠 |
| `tests/memory_v2_migrate_secret.rs` | value/evidence 中的凭据 → local deprecated 脱敏；project 零命中；报告零原文 |
| `tests/memory_v2_migrate_idempotent.rs` | 重跑不重复；输入变化 → 新 run；输出已存在 → 拒绝覆盖 |
| `tests/memory_v2_migrate_crash.rs` | 备份/转换/cutover 阶段崩溃恢复；不出现半 live store |
| `tests/memory_v2_migrate_git.rs` | current 只出 active；history/pending/attic/tombstone/journal/receipt 只进 state；`git status` 不含内部状态；local ignore 生效 |
| `tests/memory_v2_migrate_rollback.rs` | 冻结写回滚、首次写前回滚；写后回滚的明确失败/需反向迁移 |

Dogfood（真实 CodeLeveler + DeepSeek Flash）：

1. 复制当前仓库 legacy store 到隔离目录（不碰真实 store），执行 inventory/backup/convert/validate；确认 9 条 active 的 disposition。
2. 确认两条疑似凭据记录被 quarantine/local/deprecated；project 零 secret。
3. 在临时项目启动 v2，跑用户记忆/experience/检索/存储场景：
   - 用户 `/remember` → Markdown；重启 → recall；
   - 模型 remember → model_inferred；用户 fact 冲突 → pending；
   - failure→resolution experience → verified_resolution；environment constraint → 最多 observed；
   - 中文 2 字 query + 代码标识符；context builder 预算；注入事件；
   - `memory.read` 按需读；pending accept；forget 后 tombstone、不可 recall。
4. Shadow compare：固定 query corpus 的 legacy vs v2 top-k；差异解释。
5. Rollback drill：冻结写阶段切回 legacy；首次 v2 写前回滚；记录写后回滚的限制。
6. 真实迁移（如用户另行批准）：按迁移文档执行 cutover，保留 legacy 只读，观察稳定窗口。

### 4.5 停止条件

- 盘点出现未解释的 parse_error/ambiguous store，且用户未决定 → 停。
- 备份未完成/哈希失败 → 停。
- project 输出命中 secret → 停，清理隔离输出，重新处理；不进入 live。
- 冲突组被自动选赢家，或 pending 被自动 active → 停。
- 条数不守恒、链接断裂未报告、experience 伪造 verified → 停。
- shadow compare 关键 query 旧值优先且无法解释 → 停。
- 真实数据迁移未获单独授权 → 停。
- Windows/网络 FS 的 rename/锁/fsync 未验证且目标平台是 Windows → 标记 blocked，不假装通过。

### 4.6 完成标准

- 迁移报告完整、可审计、无 secret；每个 legacy 文件有 disposition。
- 隔离转换/验证/shadow 全部通过；冲突组/pending 需要用户决定的事项清楚。
- 真实 CodeLeveler + DeepSeek Flash 的 Dogfood 场景通过；未通过项明确 NOT_VERIFIED/blocked。
- v2 成为唯一写路径（在测试项目/批准的项目）；legacy 只读。
- 回滚 gate 与写后回滚限制写入文档并演练。
- 文档同步：ARCHITECTURE 双版、SECURITY、CHANGELOG、README（按实际变更）。
- gate：`./dev fmt`（本任务文件）、相关 `cargo test`、`./dev arch`、项目现有 gate；需要 release 时再按 release 流程。
- 没有删除真实数据；备份可读；没有 AI 署名。

### 4.7 回滚

- Cutover 前：删除隔离输出；live 未变。
- Cutover 冻结写、首次 v2 写前：配置切回 legacy；Markdown 保留只读。
- 首次 v2 写后：不做配置级“无损回滚”；执行反向迁移/导出，保留 v2 归档；需用户批准。
- Legacy retirement 后：从备份恢复是唯一路径；不自动删除备份。

---

## 5. 依赖与顺序

```text
Stage 1 (store/schema/journal/lifecycle)
   │
   ├── Stage 2 (service/tool/auto)  ── 需要 Stage 1 的 store + lifecycle
   │        │
   │        └── Stage 3 (index/context/event) ── 需要 service 的 recall 接口 + 标记
   │                 │
   │                 └── Stage 4 (migration/cutover) ── 需要 index + context + 事件
   │
   └── 迁移工具可在 Stage 2 后并行开发，但 cutover 必须等 Stage 3 完成
```

| 依赖 | 版本/来源 | 阶段 | 说明 |
| --- | --- | --- | --- |
| `serde_yaml`（serde_yaml_ng 0.10） | workspace 已有 | Stage 1 | frontmatter；不新增 YAML 实现 |
| `sha2` 0.10 | workspace 已有 | Stage 1 | content digest |
| `uuid` v4 | workspace 已有 | Stage 1 | `mem_<uuid>`；不新增 ULID |
| `tempfile` 3 | 现有 | Stage 1 | 原子写 |
| `fs2` 0.4 | workspace 已有 | Stage 1 | scope lock |
| `leveler-core` | workspace 已有 | Stage 2 | `SessionId`/`TurnId`/`ToolCallId`/`TaskId`；不反向依赖 agent/engine |
| `rusqlite` 0.32.1 | 新增 | Stage 3 | 必须与 `libsqlite3-sys 0.30.1` 对齐；workspace 的 sqlx 已经是 `bundled`，不要另开 system SQLite |
| `leveler-context::estimate_tokens` | 现有 | Stage 3 | 预算 |
| 现有 Event/Storage | 现有 | Stage 3 | 不新建日志 |

不新增：embedding 库、向量库、LLM Judge 框架、第二套 store、第二套 session log、消息队列、迁移框架。

### 5.1 验证 Gate

| Gate | 何时用 | 内容 |
| --- | --- | --- |
| 阶段 Gate | 每个 Stage 完成时 | 目标测试 + 受影响 crate/target；Memory 语义变化跑 `./dev arch` |
| Bug/回归 Gate | 任何 Bug 修复 | 先有能复现的 Red 测试，再最小修复；保留回归测试 |
| Commit/PR Gate | 用户授权 commit/PR 时 | 本任务 Rust 文件 `./dev fmt`、受影响 workspace check/clippy、确定性测试、`./dev arch`（涉及 Memory 语义时） |
| Dogfood Gate | Stage 2/3/4 | 真实 CodeLeveler + DeepSeek Flash；记录场景、实际观察、未通过项；不把单测绿当产品验收 |
| Release Gate | 用户明确 release/RC 时 | `./dev` 调外部实验室 `release_check.py` → `rc_check.py`；不在本计划内另建门禁 |

不在普通开发中默认跑 workspace 全量测试、all-features check、全量 clippy、release build；需要时说明技术原因。

---

## 6. 决策表

| 决策项 | 方案 | 原因 | 代价 | 风险 |
| --- | --- | --- | --- | --- |
| Markdown 权威存储 | 一条逻辑 Memory 一个 `.md`；frontmatter + body；history 目录；SQLite 只做索引 | 人可读/可编辑/可 diff/可提交；当前字段集合不需要关系事务；唯一权威易审计 | 文件数多、写入是 rename 而非 SQL、跨文件需要 journal | 外部编辑/合并冲突；目录 IO 在超大 store 下变慢；Windows fsync 语义待验证 |
| Frontmatter Schema | 最小字段 + `legacy` 保留未知字段；schema=2；严格校验；proposal/experience 块 | 可读、可演进、迁移可追溯；不靠 serde default 猜 | YAML 解析/序列化成本；重复键/alias 需要额外拒绝 | 用户手写 YAML 出错；恶意项目文件伪造 authority 元数据（不授予权限） |
| 三层 Scope | global `~/.leveler/memory`、project `<repo>/.leveler/memory`、local `<repo>/.leveler/local/memory` | 用户/团队/机器三种生命周期；project 可版本化；local 可 ignore | 多 store、跨 scope 去重、路径/权限检查 | project 误提交敏感事实；local 未 ignore；repo 只读 |
| Memory Service | 唯一写/lifecycle 入口；actor 推导 authority；CAS；pending review | 消除多写入路径；模型不能自升 authority；所有 UI/工具一致 | 需要改所有调用点；actor 上下文要贯通 | legacy 适配层仍可能绕过；调用点遗漏；Full 模式误以为无约束 |
| Memory Tool | 保留 `memory`/`remember`/`forget` 名字，扩展 update/deprecate/pin/unpin；全部走 service | 不破坏模型 surface；工具只是薄适配器；便于逐步迁移 | 工具 schema 变大；模型可能误用动作 | 模型把建议写成事实；模型 pin 影响排序；工具结果泄露 body |
| 自动提取 | fresh-turn 用户事实 + evidence span + known_identities；experience 由 runtime receipt；不新增 Judge | 保持现有成本模型；身份收敛靠结构化上下文；证据可验证 | 提取延迟/成本；known_identities 需要查询；continuation 不覆盖 | 模型误标 statement；paraphrase 重复；provider 输出 malformed；用户修正延迟 |
| 生命周期 | 同身份结构化更新；受保护用户事实只能 User 命令/review 直接更新；不同 subject 的相同 body 只提示不合并；history/tombstone；TTL 转 expired | 一个事实一个当前版本；保留证据；用户拥有最终控制 | pending 增多；用户 review 负担；旧数据冲突需要解决 | 过度保守导致旧值滞留；keyless 旧值；模型误标 correction |
| FTS5 | 独立 SQLite FTS5；现有 tokenize 预切分 + unicode61；可重建；active-only | 满足中英文/代码召回；不引入向量/Judge；索引可删 | 新增 rusqlite 依赖；索引维护/重建；中文 2 字需预切分 | 版本冲突（libsqlite3-sys）；损坏；并发；词法召回不懂同义 |
| Context Builder | standing + candidates + experience；预算；sensitive 排除；catalog 独立 + version/delta；recall 放 tail | 有界、可解释、不改变 prefix cache；按需 read 降低 token | 新格式/预算逻辑；tail 注入需要 encoder 支持；catalog 失效管理 | 选错候选；catalog stale；scope 泄露；预算挤压 user constraints |
| Logged Injection | 扩展现有 `MemoryRecalled` 并持久化；id/revision/scope/kind/mode/token/digest/omission | 不新建 Session Log；可回答“注入了哪个版本”；历史与当前分离 | 事件体积/保留策略；payload 兼容；UI 需要读新字段 | 事件含敏感 title；历史事件被误当当前；旧客户端忽略字段 |
| 历史迁移 | 盘点/备份/隔离转换/校验/shadow/cutover/rollback；精确重复折叠；冲突组 pending；secret 隔离 | 可预演、可回滚、不触碰真实数据；不强制语义合并 | 人工 review 成本；迁移窗口；报告/备份管理 | secret 泄露；条数丢失；冲突组积压；写后回滚困难；大小写 store 歧义 |

---

## 7. 十个问题的结论

### 7.1 Markdown-first 是否足够？

**足够承载当前 Memory 语义，不足以承载“语义等价判断”。**

证据：

- 当前 `MemoryEntry` 的 durable 字段（title/body/tags/key/value/kind/status/scope/provenance/supersedes/expires/experience）都能用 frontmatter + body 表达；没有需要跨行事务的关系模型。
- 现有 JSON store 已经是“一条一个文件”，Markdown 只是把同一模型换成可读格式；写入的原子性、锁、history、tombstone 都能在文件层实现。
- 现有身份是 `subject_key` 精确匹配，不是 SQL 关系；FTS5 只是召回候选，不参与身份。
- 不需要 SQL 事务来保证“一个当前版本”；per-scope lock + journal + CAS 足够。

不足：

- 不能证明“同一事实的两种措辞”等价；没有 embedding/Judge 时，Markdown 和 SQLite 都一样不能。v2 只让同 `subject_key` 与显式 `replaces` 收敛；精确 body 跨 subject 只提示 `possible_duplicate`。
- 大 store 的文件枚举/写入比 SQL 慢；用 FTS5 + 写入路径不扫目录缓解。
- 项目 Markdown 是用户/团队可编辑内容，权威但不可信；authority 元数据不能授予权限。

结论：采用 Markdown-first；不采用“Markdown + SQLite 双权威”。

### 7.2 如何避免双真相？

**一个当前权威 + 一个写入口 + 一个派生索引 + 一个历史日志。**

- 当前事实只在 Markdown；SQLite 只存 token/摘要，可删可重建。
- 所有写/状态转换只经 `MemoryService`；legacy store 在迁移期只读，不双写。
- Event Log 只记录“发生过什么”（注入/变更），不决定“现在是什么”；当前注入从 Markdown 重建。
- 注入事件只存 id/revision/digest，不存可被误当 current 的 body。
- 测试：删除 index 后 recall 只降级；重开 store 只读 Markdown；同一 subject 只能一条 active；pending 不注入。

残余：项目文件可被外部编辑；CAS/digest 防止静默覆盖，但外部编辑者可以制造冲突。冲突可见，不自动选赢家。

### 7.3 如何保证一个事实一个当前版本？

- 身份 = `(scope, kind, subject_key)`；自动/模型写入的 user fact 必须有 subject_key（迁移 keyless 例外，标 `legacy.keyless`）。可信用户显式写入（`/remember`、`记住：…`、User Review）可以无 subject_key，记录身份是稳定 `MemoryId`；不伪造 subject、不按正文相似度合并。Stage 2.1 实现见 `crates/leveler-memory/src/v2/store.rs` 的 keyless 守卫与 `service.rs` 的显式命令路径。
- 同身份 active 唯一是 store invariant；写入在锁内复查，index 只加速。
- 同身份不同值：受保护用户事实（receipt `user_explicit`/`user_approved`）只能 User 命令/review 直接更新，Model/Runtime 一律 pending；目标不受保护时按 authority/新 evidence 决定 supersede 或 pending。
- 不同 subject 的相同 body 是两条独立事实，只提示 `possible_duplicate`；不同 subject 的不同值不判冲突。
- 旧版本进 history + `superseded_by`；forget 进 tombstone；expired/deprecated 不再注入。
- 注入只读 active + 可信 receipt；resume/compact 重新组装，不重放旧 block。
- keyless 旧值通过迁移映射/successor 处理；未迁移前报告，不假装已收敛。

残余：paraphrase + 不同 subject 仍可能是两条 active；这是语义等价问题，不在本轮用词表/相似度/Judge 解决。

### 7.4 E3 U1–U6a 如何修复？

| 缺陷 | v2 修复 | 仍然不能证明 |
| --- | --- | --- |
| U1 同轮双写 | 直接命令的写入以 Memory ID 为身份；extractor 候选仅当 evidence span 精确包含于命令正文时 `Skipped{DirectCommandOwned}`，同轮其它独立事实按各自 OpId 准入；模型工具走同一 service 的显式 OpId 幂等 | 不同措辞、不同 subject 的跨轮重述仍可能两条 |
| U2 subject 分裂 | 身份只由 `subject_key` 决定；同 body 跨 subject 不合并，只提示 `possible_duplicate`；known_identities 让模型复用 id/subject；related_subject 只做一次性 target，不造别名 | paraphrase 不合并；相同 body 不同 subject 仍是两条 |
| U3 不完整纠正 | known_identities 生产接线；`replaces_memory_id` 必须命中；keyless 迁移 successor；target 缺失 → pending，不猜 | 模型不给 id 且无法匹配时，仍需用户确认 |
| U5 跨会话矛盾 | 只读 active + receipt；superseded 退出 current；Markdown 是跨会话唯一权威；resume 重建；keyless 迁移 | 不同 subject 的矛盾仍并存；需要用户/主模型判断 |
| U6a proposal 误激活 | statement 持久化；proposal/hypothetical 不写入；authority 由 actor 推导；Model/Runtime 对受保护用户记录只能 pending；review 只给 User | 模型把 proposal 误标 assertion 时，store 不读语义，仍不能把 Model actor 变成 User；但无法自动识别“这是建议”的表达 |

关键点：现有 E3 测试在 JSON store 上全绿，但 U3/U5 的生产可达性依赖 `known_identities`；v2 必须先修这个接线，否则只是“测试里能过”。

### 7.5 如何防止 proposal → decision 升级？

- 提取阶段：`StatementKind` 只允许 assertion/correction/rejection/note；proposal/hypothetical 直接 `not_stored`，不生成 pending。
- 存储阶段：statement 写入 frontmatter；proposal 不能是 active；`proposal` 字段只是提案内容，不是授权。
- authority：由 Actor 路径推导；模型 JSON 的 authority/verified 不采信；Model/Runtime 对 receipt 为 `user_explicit`/`user_approved` 的目标只能 pending。
- `Actor::User` 只能由可信交互路径构造；模型 shell/CLI 参数/tool payload 不能声明用户身份；Full Access 不改变。
- review：`review` 只接受 User actor；模型不能 accept 自己的 pending；模型 pin 不进入 standing。
- 注入：model_inferred 标签；pending/superseded/ExternalUnverified 不注入；header 声明 advisory data。
- 残余：运行时不能理解“这句话是不是建议”；模型误标 assertion 时无法确定性识别。但 authority 不来自 statement 文本，Model 写入受保护目标仍然只能 pending；这是不引入 Judge 的明确边界。

### 7.6 模型工具与自动学习如何共存？

- 同一个 service、同一个 identity、同一个 store；区别只在 Actor/authority。
- 模型工具写 project active 时是 `model_inferred`；自动提取的用户事实是 `user_stated`；直接命令是 `user_explicit`；用户 accept 是 `user_approved`。
- 同 subject 的用户直接命令优先：extractor 候选 `Skipped`；不同 subject 的独立事实不因同一 turn 被吞掉。
- 幂等：显式 `OpId`；tool 用 `tool_call_id`，extractor 为每个候选生成稳定 id，直接命令用 `UserInvocationId`；同 OpId 重试返回 `Replayed`，不同内容返回 `Conflict`。
- 冲突：同身份按 authority/receipt/lifecycle；不同 subject 的相同 body 只提示；不自动选赢家。
- 自动 experience 由 runtime receipt，不由模型工具生成 verified；模型 remember 不能写 experience verified。

### 7.7 并发与崩溃恢复如何做？

- 每 scope 一个 fs2 锁；写持排他锁，读/列表/effective_active/pending review 持共享锁。
- 读者在共享锁内完成整个读操作；发现 prepared journal 先升级恢复，避免半提交/零 active。
- CAS：`expected_revision` + `expected_digest`；外部编辑者不持锁，靠 digest/receipt 发现。
- 多文件：history 先写 → stage(current FS) → journal prepared/fsync(state) → 新值出现 → 退休旧值 → intent/tombstone → journal committed/fsync；恢复按 journal 状态表重放/停止。
- 单文件：temp+fsync+rename+目录 fsync（Unix，同 FS）；旧文件或新文件完整。
- index 最后更新；失败标 dirty，不回滚 Memory。
- 测试：每个阶段 abort 的子进程、两进程并发、外部编辑冲突、读一致性、index 删除重建。
- 未验证：Windows 目录 fsync、网络 FS 锁语义、真实跨文件系统组合；不能声称已保证。

### 7.8 如何安全迁移？

- 只读盘点 → 全量备份（manifest/哈希/`BACKUP_COMPLETE`）→ 隔离转换 → schema/链接/secret/index/shadow 校验 → 人工 review → 冻结写 cutover → v2 live → legacy 只读。
- 不做语义合并；精确重复折叠；同 subject 不同 value 进 conflict group + pending；pending 不自动 active；旧 suppress 不导入。
- secret 默认 quarantine/local/deprecated；project 零命中；报告零原文；不自动 commit。
- 回滚窗口到首次 v2 写之前；写后回滚不是配置开关，需要反向迁移，必须提前说明。
- 真实数据迁移需单独授权；本轮只设计，不执行。

### 7.9 剩余架构风险

1. paraphrase + 不同 subject 的重复/矛盾无法自动证明；只能精确匹配和用户 review。
2. 模型误标 statement/authority 的语义风险；不引入 Judge，无法确定性识别；但 authority 不从文本读取，Model 对受保护目标仍只能 pending。
3. 项目 Markdown/外部编辑不可自证权威；无 receipt 匹配即 `ExternalUnverified`，不授予权限；receipt 不防能写 state root 的本机同等权限进程。
4. Windows 目录 fsync、网络 FS 锁、跨平台 rename 语义、真实跨文件系统组合未验证；不承诺全局原子提交。
5. FTS5 词法召回的中文同义/跨语言能力有限；不引入向量是明确取舍。
6. forget 不能擦除 Git/事件备份/provider 日志/文件系统残留；凭据需轮换。
7. continuation 用户消息不自动提取；只能直接命令/模型工具。
8. 迁移冲突组/人工 review 负担；写后回滚成本高。
9. 事件日志体积/保留策略；注入 digest 可审计但不能还原 body（需要 history 仍在）。
10. 自动提取的 provider 质量/成本；malformed/超时不是“没有可学内容”。
11. 显式无 subject 的用户记录身份是 Memory ID，不与结构化 subject 自动合并；正文中嵌套的较窄事实仍可能生成第二条结构化记录；当前 UI 没有补 subject 的入口，Pending 展示内容也没有独立 digest 回传。

### 7.10 Stage 1 是否可以开始？

**是。当前任务已明确授权 Stage 1 实现；完成后停止，不进入 Stage 2。**

Stage 1 的启动条件：

- 本设计、迁移、实施三份文档的错误边界已修订（2026-10-23 勘误）。
- 允许 `leveler-memory` 增加 workspace 已有依赖 `serde_yaml`/`sha2`/`uuid`；不引入 SQLite（Stage 3 才引）。
- 只在 `v2` 模块内实现，不改 legacy 行为、不接运行时、不迁移真实数据。
- 按 TDD 先写 v2 E3 契约/崩溃/并发/安全测试；`./dev arch` 通过。
- Windows 目录 fsync 与真实跨文件系统组合在 Stage 1 标记 UNVERIFIED，不写成已保证。
- 不修改并行 WIP 的 `e3_lifecycle.rs`；不 push/开分支。当前任务允许在当前分支做小步 commit，但共享文件有活跃 WIP 时停止该文件并报告 BLOCKED。

Stage 1 的第一批 commit（授权后）：

1. `test(memory): pin v2 markdown store contracts`（测试先红；不含实现）。
2. `feat(memory): add markdown v2 store and journal`。
3. `test(memory): cover crash recovery and concurrent writers`。
4. `docs(memory): record Stage 1 verification`（如需要）。

不加 Codex/AI 署名；不 push；不自动开分支。

---

## 8. Git 交付

| 阶段 | 交付形态 | 授权 |
| --- | --- | --- |
| 本轮 Stage 1 | 修订三份 docs + `v2` 实现/测试；当前 branch 上小步 commit；不 push | 用户已明确授权 Stage 1 与当前 branch 小步 commit |
| Stage 2 | 同上；配置默认不变 | 需授权 commit；push 单独授权 |
| Stage 3 | 同上；含依赖/事件/UI | 需授权 commit；push 单独授权 |
| Stage 4 | 迁移工具/文档/cutover 脚本 | 需授权 commit；真实 cutover 单独授权 |

规则：

- 不在 commit message/author/committer/trailer 加 Codex/AI 标识。
- 不擅自创建/切换分支、worktree、rebase、reset。
- 不删除用户/其他 agent 的未提交改动。
- 不自动 `git add` 整个工作区；只加本阶段文件。
- 不把迁移报告/备份/secret 加进 Git。
- push 是单独授权；tag/release 另行授权。

2026-10-23 实际：未产生 commit。`crates/leveler-memory/src/lib.rs` 的 `pub mod v2;` 与其他 Agent 的 WIP 同文件，按“共享文件存在活跃并发修改时停止该文件操作”的约束，未做 partial-stage；v2 实现、测试和 docs 保留在工作区。

---

## 9. 迁移风险与缓解

| 风险 | 影响 | 缓解 | 停止条件 |
| --- | --- | --- | --- |
| 凭据进入 project/Git | 泄露 | 全字段 secret scan；quarantine/local/deprecated；project 零命中；报告零原文；不自动 commit | 任一 project 命中 |
| 条数丢失/重复 | 用户事实丢失 | 备份 + manifest + 条数守恒 + disposition；重跑幂等 | 守恒失败 |
| 同 subject 冲突被自动选赢家 | 错误 current | conflict group + pending；无自动赢家 | 出现自动选择 |
| pending 自动 active | 未确认建议变成事实 | pending 完整记录 + review；迁移不激活 | pending 出现在 active |
| 写后回滚困难 | v2 增量丢失 | 回滚窗口说明；反向迁移工具；冻结写 | 用户未接受写后回滚成本 |
| 大小写/多 store 歧义 | 迁移错误数据 | inventory 列全部候选；`--legacy-root` 指定；不合并 | ambiguous_store 未决定 |
| keyless 旧值 | 跨会话双 current | 不造 subject；legacy.keyless；报告；后续 successor/用户处理 | 把 keyless 当 keyed 合并 |
| experience 伪造 verified | 错误经验 | evidence 结构校验；EnvironmentConstraint 收紧；旧 verified 降级报告 | verified 无 failure+resolution |
| 迁移窗口中断 | 半 live store | journal/manifest/`BACKUP_COMPLETE`；cutover 可恢复 | 半文件/无法恢复 |
| Windows/网络 FS | 持久性/锁失效 | Stage 4 前验证；不支持则标记 blocked | 未验证却声称通过 |

---

## 10. 后续优化顺序

Stage 4 之后，按以下顺序（不改变本轮范围）：

1. **continuation 用户消息 provenance**：先让存储区分真实 user-authored message 与 runtime objective，再扩展用户事实提取；不解析 objective 文本。
2. **paraphrase 重复报告**：只做只读审计/用户 review，不做自动合并；不引入语义词表/相似度/Judge。
3. **history compaction/retention**：按 revision 数/时间压缩历史，保留 tombstone 与关键 evidence；先定产品保留契约。
4. **FTS5 查询质量/规模调优**：基于真实 query corpus 调整 token 权重/预算，不改身份/生命周期。
5. **Windows/网络 FS 持久性验证**：目录 fsync、rename、锁、只读 repo；未通过就明确支持边界。
6. **跨 scope 移动 UX**：新记录 + supersede，不原地改 scope；先有用户场景。
7. **事件保留/导出**：注入审计的保留策略、导出格式、隐私脱敏；不新建第二套日志。
8. **工具面清理**：评估 `vector_search`（FNV hashed bag-of-tokens）是否下线或映射到 FTS5；不与 Memory 2.0 核心混做。

明确不做：embedding/向量库、LLM Judge、同义词/停用词表、第二套 store、第二套 session log、长期双写。
