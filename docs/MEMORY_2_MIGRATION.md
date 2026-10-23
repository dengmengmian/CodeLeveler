# CodeLeveler Memory 2.0 迁移方案

状态：设计稿；Stage 4 未开始，未执行任何真实数据迁移。本轮不修改旧 store、不删除旧数据；legacy 与 Markdown backend 在配置层共存，默认 backend 未改变。

结论先行：

1. 迁移是**离线的、只读的、可重放的转换**：旧 JSON store 永远先完整备份，再复制到隔离目录转换成 Markdown；真实 store 在 cutover 前不被写入或删除。
2. **不做语义合并**。只折叠“同一 scope、同一 kind、同一 subject_key、同一 value”的精确重复；不同 value 的同一 subject 进入 conflict group，两个原值都保留为 deprecated，生成一个 `resolve_conflict` pending，由用户选，不自动选赢家。
3. **pending 永不自动 active**；旧 suppress 指纹不直接导入（旧指纹不含 value，会误抑制不同值），只进报告。
4. 当前仓库的真实旧 store 有 9 条 active，其中 `k-apikey`、`k-llmprovidervolcanoapikey` 的 evidence 疑似真实凭据。迁移默认**隔离并脱敏**，任何含凭据的记录不得写入 project Markdown，也不得进入 Git。
5. cutover 后的回滚窗口只到“第一次 v2 写入”之前。之后回滚不是配置开关，而是反向数据迁移；这一点必须在 cutover 前说清楚，不能承诺“随时无损回滚”。

---

## 1. 迁移对象与边界

### 1.1 输入

| 输入 | 位置 | 说明 |
| --- | --- | --- |
| Legacy store | `<state_dir>/memory/` | `active/`、`archive/`、`pending/`、`suppress/`、`pending_history.jsonl`、`.lifecycle.lock` |
| 项目配置 | `<repo>/.leveler/config.yaml` | 只读；`memory.enabled/user_enabled/experience_enabled` 原样保留 |
| 项目根 | `<repo>` | 目标 project/local 路径 |
| 可选 scope map | 迁移参数，用户显式提供 | 只允许按 legacy id 指定 global/local；不从文本猜 scope |
| 可选 conflict decisions | 用户显式提供 | 只在迁移后 review 阶段使用；迁移本身不选赢家 |

### 1.2 输出

| 输出 | 位置 | 说明 |
| --- | --- | --- |
| 备份 | `~/.leveler/backups/memory-v1-<project-id>-<timestamp>/` | 完整副本 + manifest；0700；不在 repo |
| 隔离转换 current | `~/.leveler/migration/<run-id>/current/project/`、`/current/local/` | 只放 active Markdown |
| 隔离转换 state | `~/.leveler/migration/<run-id>/state/project/`、`/state/local/` | history/pending/attic/tombstone/journal/receipt/intent |
| 迁移报告 | `~/.leveler/migration/<run-id>/report.json`、`report.md` | 含 id、状态、原因、哈希；不含 secret 原文 |
| 验证结果 | 同目录 `validation.json` | 计数、链接、解析、索引、对比 |
| live current | `<repo>/.leveler/memory/`、`<repo>/.leveler/local/memory/` | 只在 cutover 阶段写入；仅 active |
| live state | `~/.leveler/state/projects/<encoded>/memory-v2/project/`、`/local/` | 私有运行时；永不进入 Git |

### 1.3 非目标

- 不迁移其他项目的 store；不扫描整个 `$HOME/.leveler/state/projects/` 后“顺便”迁移。
- 不把旧 `Ephemeral` 变成 durable active。
- 不把旧 `Derived` 变成用户事实。
- 不把旧 `pending` 变成 active。
- 不导入旧 suppress 指纹去抑制新候选。
- 不把 secret 写入项目 Markdown、报告、日志或 Git。
- 不自动 commit/push。
- 不在本轮执行真实迁移。

---

## 2. 迁移前盘点（Inventory）

### 2.1 盘点步骤

1. 解析项目根 canonical path；计算 `encode_repo_path`，得到**候选** state 目录。
2. 列出所有大小写变体（当前本机已出现 `...CodeLeveler-...` 与 `...codeleveler-...` 两个 namespace）。
3. 对每个候选目录记录：存在性、文件数、总字节、`active/archive/pending/suppress` 计数、`pending_history.jsonl` 行数、每个 JSON 的 SHA-256、mtime。
4. 若多个候选都非空：**不合并**；报告 `ambiguous_store`，要求用户用 `--legacy-root` 指定。若只有一个非空，使用它；空目录只记录。
5. 逐条读取 JSON，但不写入；解析失败的文件记录 `parse_error`，继续盘点其他文件，最后一次性报告（不要因为一个坏文件中断）。
6. 对每条记录计算 `legacy_sha256`（原始文件字节）和字段摘要（id/status/kind/scope/key/value 是否存在），不记录 body/title 原文到共享日志。
7. 盘点 secret：对 title/body/tags/key/value/provenance.evidence/experience 做 secret scan；只记录 `secret_suspected` 布尔和命中字段名，不记录命中值。

### 2.2 盘点报告字段

```json
{
  "run_id": "2026-10-23T...",
  "project_root_hash": "sha256:...",
  "legacy_root_id": "state-project-<encoded>-<hash-prefix>",
  "stores": [
    {
      "path_id": "...",
      "active": 9,
      "archive": 0,
      "pending": 0,
      "suppress": 0,
      "pending_history_lines": 0,
      "parse_errors": 0,
      "secret_suspected": 2,
      "files": [
        {"legacy_id": "k-apikey", "sha256": "...", "status": "Active", "kind": "Preference", "has_key": true, "has_value": true}
      ]
    }
  ],
  "ambiguous_store": false,
  "blocked": []
}
```

报告不含 body/value/evidence 原文；需要人工核对时，在本机用受控方式打开，不把报告贴到 issue/PR。

### 2.3 本机已知事实

- 当前仓库旧 store 有 9 条 active，`pending/archive/suppress` 为空。
- `k-apikey`、`k-llmprovidervolcanoapikey` 的 evidence 中出现 `ark-...` 形态的凭据。
- 仓库内 `.leveler/` 只有 `config.yaml`，没有 legacy memory 数据。
- 另一个大小写不同的 state namespace 存在但为空。
- 这些是本次审计快照，不代替迁移时的重新盘点；其他项目可能完全不同。

---

## 3. 备份与隔离

### 3.1 全量备份

```text
~/.leveler/backups/memory-v1-<project-id>-<timestamp>/
  active/...
  archive/...
  pending/...
  suppress/...
  pending_history.jsonl
  manifest.json
  BACKUP_COMPLETE
```

规则：

- 先复制到 `*.partial`，逐文件 SHA-256，校验通过后 rename 为正式目录；写 `BACKUP_COMPLETE` 才算成功。
- 不复制 `.lifecycle.lock`（进程锁不是数据）。
- 目录 0700，文件 0600；备份不进入 Git、不同步到项目目录。
- manifest 记录：源路径 hash、文件相对路径、字节数、sha256、mtime、复制时间、工具版本。
- 备份校验失败即停止迁移；不继续。
- 备份保留策略由用户决定；迁移工具不自动删除任何备份。

### 3.2 隔离转换

- 所有转换写入 `~/.leveler/migration/<run-id>/`，不是 live path。
- 每个 scope 分成 `current/` 与 `state/` 两个输出：active 正文进 `current/`；superseded/deprecated/expired 的正文进 `state/history/`，最终状态进 `state/attic/`；pending 进 `state/pending/`；tombstone/journal/receipt/intent 也只进 `state/`。
- 转换器只读 legacy；如果发现 legacy 被并发修改（mtime/哈希与盘点不一致），停止并要求重新盘点。
- 转换完成后，先在隔离目录做完整校验与 shadow read；通过后才进入 cutover。
- 转换器不调用 `MemoryService` 的写路径（避免 migration 伪造成用户 actor）；它只生成符合 schema 的文件，由 `MemoryService`/store 的只读校验器验证。转换器也不得绕过 schema 校验写 live 路径。
- 隔离目录也必须满足 Git 边界：`current/` 只有 active Markdown + 窄 `.gitignore`，`state/` 不含任何应进仓库的内容。

---

## 4. 字段映射

### 4.1 总表

| Legacy | v2 | 规则 |
| --- | --- | --- |
| `id` | `id` | 合法且 scope 内唯一则保留；否则 `mem_<UUIDv4 simple>`，旧值放 `legacy.original_id` |
| `title` | `title` | 保留；空则 quarantine |
| `body` | 正文 | 原样；secret → 脱敏/quarantine |
| `tags` | `tags` | 保留合法标签；超限/非法放 `legacy.tags_original` 并报告，不静默丢 |
| `created_at` | `created_at` | RFC3339 校验；非法则用文件 mtime + `legacy.timestamp_fallback=true` |
| `updated_at` | `updated_at` | 同上 |
| `archived_at` | `legacy.archived_at` | 仅历史；不映射为当前字段 |
| `key` | `subject_key` | 有则规范化保留；无则 `null` + `legacy.keyless=true`，不猜。Stage 2.1 之后新写入的无 subject 显式用户记录本来就是 `MemoryId` 身份，不是 legacy keyless；迁移只处理旧 store，不能用 `legacy.keyless` 覆盖新记录 |
| `value` | `value` | 原样；缺失为 null；不解析 body |
| `kind` | `kind` + `legacy.kind` | Preference/Decision/Note/LegacyUnknown → `user_fact`；Experience → `experience`；Derived → `user_fact` + `status=deprecated` |
| `status` | `status` | 见 §4.2 |
| `scope` | `scope` | Project → project；Ephemeral → local + deprecated（见 §4.3） |
| `provenance.authority` | `authority` | 见 §4.4；缺失 → model_inferred + `legacy.missing_authority=true` |
| `provenance.source` | `legacy.source` | 字符串保留；不参与 authority 推导 |
| `provenance.evidence` | `legacy.evidence_digest` | secret scan；只存摘要/存在性，不存原文 |
| `supersedes` | `supersedes` | 解析目标为 `<scope>:<id>`；目标缺失 → `legacy.broken_link=true` |
| `superseded_by` | `superseded_by` | 同上；只允许 status=superseded 时使用 |
| `superseded_at` | `legacy.superseded_at` | 历史字段 |
| `expires_at` | `expires_at` | RFC3339 校验；非法 → 报告并 quarantine（不猜 TTL） |
| `experience` | `experience` | 见 §4.5 |
| 无 | `revision` | 统一 1；迁移不伪造版本链 |
| 无 | `statement` | `assertion` + `legacy.statement_unknown=true` |
| 无 | `pinned` | false；不按 tag/标题猜 |
| 无 | `sensitive` | 检测到非凭据敏感 → true；凭据 → quarantine/deprecated |
| 无 | `source_refs` | 一条 `type: migration`，含 legacy id/sha256/source；不含原文 |

### 4.2 状态映射

| Legacy 状态 | 条件 | v2 状态 |
| --- | --- | --- |
| `Active` | 正常、无冲突、无 secret | `active` |
| `Active` | 同 subject_key 多个不同 value | `deprecated` + `legacy.conflict_group`；另建 `resolve_conflict` pending |
| `Active` | 同 subject_key 同 value 精确重复 | 一条 `active`，其余 `superseded` + `legacy.duplicate_of` |
| `Active` | 检测到凭据 | `deprecated` + `sensitive=true` + `legacy.secret_redacted=true`，正文脱敏 |
| `Active` | `kind=Derived` | `deprecated` + `legacy.derived=true` |
| `Active` | `scope=Ephemeral` | `deprecated` + `scope=local` + `legacy.ephemeral=true` |
| `Superseded` | 目标存在 | `superseded` + `superseded_by` |
| `Superseded` | 目标缺失 | `superseded` + `legacy.broken_link=true`，不猜 |
| `Expired` | `expires_at` 存在 | `expired` |
| `Archived` | `archived_at` 存在且无过期 | `deprecated`（遗忘） |
| `Archived` | `expires_at` 已过 | `expired` |
| 缺失/未知 | — | quarantine；不默认 active |

### 4.3 Scope 映射

- Legacy 只有 `Project` / `Ephemeral`。默认 **全部 Project → project**。
- 不根据标题/正文/标签判断“这是全局偏好”；global 只能由显式 `--scope-map` 指定。
- `Ephemeral` → `local`，且 `status=deprecated`；`legacy.ephemeral=true`。理由：旧代码没有强制 Ephemeral 带 expiry，直接迁成 active 会把临时事实变成 durable；不伪造 TTL。
- 如果用户显式提供 scope map，必须按 legacy id 精确匹配；未匹配的记录仍按默认。
- 项目路径不可写/只读 → 整个迁移 blocked；不静默改写 local/global。

### 4.4 Authority 映射

| Legacy authority | v2 | 说明 |
| --- | --- | --- |
| `ExplicitUser` | `user_explicit` | 保留 |
| `ProjectSource` | `project_source` | 保留 |
| `RuntimeObservation` | `runtime_observed` | 保留 |
| `ToolObservation` | `tool_observed` | 保留 |
| `ModelInference` | `model_inferred` | 保留 |
| 缺失 | `model_inferred` | `legacy.missing_authority=true`；不因为 source 字符串升级 |
| 未知枚举 | — | quarantine；不猜 |

- 旧 `provenance.source` 是自由字符串（`user_direct`、`agent_remember_tool` 等），不作为 authority 证明；只保留在 `legacy.source`。
- 旧直接写入的 `ExplicitUser` 保留；但它只代表“当时由调用方声称用户直接写入”，不是可验证身份。迁移报告标注 `legacy.authority_unverified=true`。

### 4.5 Experience 映射

| Legacy | v2 | 规则 |
| --- | --- | --- |
| `category` | `experience.category` | 字符串保留；空/超长 → quarantine |
| `verification` | 不直接写 | v2 由 evidence 派生；旧值放 `legacy.verification` |
| `applies_when` | `experience.applies_when` | 1..400 字符；空 → quarantine |
| `invalid_if` | `experience.invalid_if` | 可空；超长 → quarantine |
| `evidence[].kind` | `experience.evidence[].kind` | 只允许 failure/resolution/mutation/observation |
| `evidence[].reference` | `experience.evidence[].reference` | 1..400；secret → 脱敏/quarantine |
| 无 | `experience.environment` | 空对象 + `legacy.environment_unknown=true`；不猜 |
| 无 | `source_refs` | migration ref |

一致性检查：

- 旧 `Verified` 且 category ≠ EnvironmentConstraint：evidence 必须同时有 failure 和 resolution；否则降级为 `observed`，`legacy.verification_downgraded=true`，报告。
- 旧 `Verified` 且 category = EnvironmentConstraint：v2 最多 `observed`，除非有结构化环境变化 + resolution；没有就降级。
- 旧 `Unverified`：保持无 verified；如果 evidence 只有 observation → observed；否则不进入 active（pending 或 quarantine）。
- 旧 evidence 为空 → quarantine；不是合法 experience。

### 4.6 Pending 映射

每个旧 `pending/<candidate>.json` → 一条 `status=pending` Markdown：

```yaml
proposal:
  operation: create | update | supersede
  target_ids: ["project:mem_..."]   # 能精确解析才填；否则 []
  expected_revision: 1
  expected_digest: "sha256:..."
  proposed_authority: model_inferred | user_stated | tool_observed
  reason: "migrated pending; requires user review"
```

- `key`/`related_key`/`replaces_id` 能精确命中已迁移 active → `update`/`supersede`；命中不到 → `create`，`legacy.target_unresolved=true`。
- 旧 pending 的 `authority` 是候选声称值；v2 `authority` 使用 `user_stated`（若 source 是 UserExplicit）或 `model_inferred`（AgentProposed/System*），`proposal.proposed_authority` 记录原值。Pending 永不自动 active。
- 旧 pending 的 `value`、`expires_at` 原样保留。
- 旧 pending 若是 experience：先按 §4.5 校验；不合法 → quarantine，不生成 pending。
- 旧 pending 的 `fingerprint` 不用于 v2 suppress；放 `legacy.fingerprint`。
- `pending_history.jsonl` 不导入 v2；复制到报告目录，作为审计材料。

### 4.7 Suppress 映射

- 旧 `suppress/<fingerprint>.json` 不导入为 v2 抑制规则。
- 原因：旧 fingerprint 只覆盖 `kind|key|title|body`，不含 `value`、`expires_at`、`replaces_id`、`evidence`（`candidates.rs:265-284`）；直接复用会抑制“同 body 不同 value”的新候选。
- 迁移报告列出旧 fingerprint 与对应 candidate 摘要；用户可以在 v2 重新 reject，生成新的完整 digest。
- 旧 suppress 文件原样留在备份，不删除。

### 4.8 无 key / 无 provenance / 旧 derived

| 情况 | 处理 |
| --- | --- |
| `key=None` | `subject_key: null` + `legacy.keyless=true`；不根据标题/正文造 subject |
| `provenance=None` | `authority: model_inferred` + `legacy.missing_authority=true`；不进入 standing |
| `kind=Derived` | `status=deprecated` + `legacy.derived=true`；不进入 active/注入 |
| `kind=Experience` 但 `experience=None` | quarantine；不伪造 record |
| `value` 存在但 `key=None` | 保留 value；仍 keyless；报告 |
| 旧 id 含 `/`、`..`、空 | 重新生成 `mem_<UUIDv4 simple>`；`legacy.original_id` 保留；不拼路径 |

---

## 5. 身份、重复与冲突

### 5.1 精确重复折叠

只有以下条件全部成立才折叠：

- 同一 scope；
- 同一 kind；
- 同一 `subject_key`（都非空）；
- 同一 `value`（都为 null 或字节相等）；
- 同一规范化 body（trim 后字节相等）或 body 完全相等；
- 没有一方的 `updated_at` 明显更新到构成“修正”的程度（时间只用于排序，不用于选赢家；若不同，仍折叠，保留最新者为 active，另一个 superseded）。

折叠是**结构化精确相等**，不是语义合并。输出：

- 一条 active（保留 `updated_at` 最新；若相同，保留 id 字典序最小，确定性）；
- 其余 `status=superseded`，`superseded_by` 指向保留者，`legacy.duplicate_of` 记录；
- 报告 `exact_duplicate_collapsed`，附所有 id。

### 5.2 同 subject 不同 value

- 不选赢家。
- 每条 `status=deprecated`，`legacy.conflict_group=<group-id>`。
- 生成一条 `status=pending` 的 resolution 记录：
  - `proposal.operation: resolve_conflict`
  - `target_ids`: 冲突组所有 `<scope>:<id>`
  - `expected_revision`/`expected_digest`: 空（目标已是 deprecated，不是普通 CAS）
  - `reason: "migrated conflict; choose the current value"`
- 在用户解决前，冲突组不进入 active，也不进入注入；报告置顶。
- 用户 accept 一个目标时，服务把该目标恢复为 active（新 revision），其他目标保持 deprecated，并在 history 记录 resolution。该操作不是“模型自动赢家”，是用户决定。

### 5.3 不同 subject、相同 body

- 不折叠（subject 是身份）。
- 保留为多条 active；`legacy.possible_duplicate=true`，报告 `same_body_different_subject`。
- 注入时不按 body 去重，不删除记录；只在报告/诊断中标注 `possible_duplicate`。
- 这是 U2 的已知残余：v2 不在迁移阶段做语义合并。

### 5.4 不同 subject、不同 body、同一事实

- 不检测、不合并；报告只列“无法判断”的计数为 0，因为运行时没有语义判断能力。
- 不引入文本相似度、同义词表或 LLM Judge。

### 5.5 跨 scope 重复

- 默认迁移全部 project，不存在跨 scope 重复。
- 若显式 scope map 把某条放到 global/local，同 subject 的 project 记录仍在 project；不自动覆盖。
- Context Builder 按 local > project > global 去重显示；迁移报告标 `cross_scope_same_subject`。

---

## 6. 安全与隐私

### 6.1 Secret 处理

检测范围：title、body、tags、key、value、provenance.evidence、experience.applies_when/invalid_if/evidence.reference、pending 的所有字符串字段。

处理：

| 情况 | 处理 |
| --- | --- |
| 凭据形态（API key/token/密码/私钥） | 不写原文到 Markdown；记录 `status=deprecated`、`sensitive=true`、`legacy.secret_redacted=true`；正文 `[redacted: credential detected during migration]`；scope 强制 local（即使原 scope 是 project）；报告只写 id/字段名 |
| 非凭据敏感（PII/内部路径） | `sensitive=true`；保留正文到 local 或 project？默认 local；不自动注入；报告只写 id |
| 疑似但不确定 | 按凭据处理（fail closed）；报告 `secret_suspected` |

- 备份目录保留原始内容（用户自己的数据），但 0700；不复制到 repo、不进入报告。
- 隔离转换目录同样 0700；验证完成后，未采用的中间文件按用户确认清理，不自动删备份。
- 迁移报告不含 secret 原文、不含 body/value；只含 id、字段名、哈希。
- project Markdown 生成后运行一次 secret scan；命中即视为迁移失败，不进入 live。
- 不自动 `git add`/commit；用户 review `git status` 后自行决定。

### 6.2 路径与权限

- 所有输出路径由固定 root + 白名单 id 生成；id 必须匹配 `^[A-Za-z0-9_-]{1,96}$`。
- 拒绝 symlink 目标；canonicalize root 后确认输出在 root 内。
- project/local/global current 与 state 目录 0700；文件 0600。
- state root 必须在 repo 外，且不得与 current root 嵌套；违反即停止迁移。
- local 目录写 `.gitignore` sentinel（`*`、`!.gitignore`）；project current 写窄 `.gitignore`（`.stage/`、`.tmp-*`）。
- 迁移工具不跟随 legacy 目录中的 symlink；遇到 symlink 记录 `blocked_symlink`，不读取目标。

### 6.3 Git 边界

- project current 是可提交内容，但迁移不自动提交；project state 永远不可提交。
- project current 只允许 `status=active` 的 `<id>.md`、`.stage/` 与窄 `.gitignore`；history/pending/attic/tombstone/journal/receipt/intent/index 只能进 state root。
- local current 必须被 `.leveler/local/.gitignore` 完全忽略（sentinel `*` + `!.gitignore`）；local state 在 repo 外。
- 迁移前检查 `<repo>/.leveler/memory` 是否已被 `.gitignore` 忽略；如果被忽略，报告“团队共享失效”，但不阻止本地迁移。
- 迁移后检查 `git status --porcelain`：只应出现预期的 project active Markdown、`.leveler/local/.gitignore`、project 窄 `.gitignore`、配置变更；出现 history/pending/journal/tombstone/receipt 或 local 文件即报告失败。
- 不修改仓库根 `.gitignore`；只写 `.leveler/local/.gitignore` 与 project current 的窄 `.gitignore`。
- 不把迁移报告、备份、manifest 放进 repo。

---

## 7. 转换与校验

### 7.1 转换流程

```text
inventory（只读）
  → backup（全量 + manifest）
  → convert to isolated/<run>/current|state/{project,local}
  → schema/parse/secret/link validation（current 仅 active；其余只在 state）
  → build index in isolated（只索引 current active）
  → shadow recall compare
  → human review report
  → cutover（仅在批准后；current 与 state 分别落位）
```

### 7.2 Schema 校验

逐文件：

- frontmatter 可解析；schema=2；必需字段齐全；类型正确。
- id 合法且 scope 内唯一。
- `kind=user_fact` → `subject_key` 非空或 `legacy.keyless=true`。
- `kind=experience` → `experience` 块存在且通过结构校验。
- `status=active` → 无 `superseded_by`；`status=superseded` → 有 `superseded_by`。
- `status=pending` → 有 `proposal`，且只出现在 `state/pending/`，不得出现在 current root。
- `status` 非 active 的记录只出现在 state (`attic` + `history`)，current root 中不存在。
- `source_refs` 非空；每条 type 合法。
- 时间字段 RFC3339 UTC。
- 所有字符串字段 secret scan。
- body/title 大小限制。
- 未知字段在 `legacy` 中保留。
- 每个 active 记录写 `receipt`（revision/digest/status/authority/pin/subject_key），作为后续外部编辑检测基线；receipt 不是正文副本。

### 7.3 全局一致性

| 检查 | 条件 |
| --- | --- |
| 条数守恒 | 每个 legacy 文件恰好映射到一条 v2 记录（或一条 quarantine 记录），无遗漏/重复 |
| Active 唯一性 | 同一 `(scope,kind,subject_key)` 最多一条 active；keyless 除外并单独报告 |
| 链接完整 | 每条 `supersedes`/`superseded_by` 目标存在；无自引用/环 |
| Pending 隔离 | pending 不出现在 current root/active 集合、不进入 index 的 active 查询 |
| Current/State 分离 | current root 只有 active Markdown；history/pending/attic/tombstone/journal/receipt/intent 只在 state root |
| Git 隔离 | `git status` 不含 local、state、history、pending、journal、tombstone、receipt |
| Receipt 基线 | 每条 current active 有对应 receipt，digest 与文件一致 |
| Experience | evidence kind 合法；standing 派生与旧 verification 一致或明确降级 |
| Secret | project 目录零命中；local 允许 `sensitive=true`，但不得有凭据原文 |
| 计数 | active/superseded/expired/deprecated/pending/quarantine 与盘点对账 |
| 哈希 | 每个输出文件的 sha256 写入 manifest；重跑可验证 |

### 7.4 索引校验

- 在隔离目录构建 FTS5 index；重建两次，行数/内容哈希一致（确定性）。
- 对每条 active 记录按 id 查询，必须命中；对 pending/superseded/deprecated/expired 查询，不得出现在 active 结果。
- 中文 2 字/3 字、中英混合、代码标识符查询用固定 corpus 校验。
- 删除 index 后重建，结果一致；Markdown 不丢。

### 7.5 Shadow read/compare

- 固定 query corpus：
  - 真实旧会话的用户消息（按隐私要求在本机运行，不导出）；
  - 人工构造的中文/英文/代码标识符 query；
  - 每个旧 active 记录的 subject/key/title 作为 query。
- 对每个 query 比较 legacy BM25 top-k 与 v2 FTS5 top-k：
  - `same_ids`、`legacy_only`、`v2_only`、`rank_changed`；
  - 记录是否因为 scope 过滤、status 过滤、dedupe、tokenizer 差异。
- 不要求 top-k 完全一致；要求：
  - 不能漏掉“用户明确 pin/standing”的记录；
  - 不能把 superseded/pending/expired 召回为 current；
  - 关键 query（billing_model 等）新值优先于旧值。
- 差异报告由人审核；不自动调参掩盖。

### 7.6 迁移报告

`report.md` 结构：

1. 盘点摘要（counts、ambiguous store、parse errors）。
2. 映射摘要（id 保留/重生成、scope、status）。
3. 冲突组（同 subject 不同 value、同 body 不同 subject、跨 scope）。
4. 隔离/脱敏记录（只列 id/字段名）。
5. pending 映射（target resolved/unresolved）。
6. experience 降级。
7. 链接断裂。
8. Shadow compare 差异。
9. Blocked/需要用户决定的事项。
10. cutover checklist 与 rollback 窗口。

报告不写 body/value/evidence 原文。

---

## 8. Cutover

### 8.1 阶段

| 阶段 | v1 写 | v2 写 | v2 读 | 目的 |
| --- | --- | --- | --- | --- |
| Inventory | 是 | 否 | 否 | 只读盘点 |
| Backup | 是（只读复制） | 否 | 否 | 可恢复 |
| Isolated conversion | 是 | 否 | 否 | 转换/校验 |
| Shadow read | 是 | 否 | 是（隔离） | 对比 |
| Cutover（冻结写） | 否 | 否 | 是（live 路径） | 切换/验证 |
| v2 live | 否（只读归档） | 是 | 是 | 新权威 |
| Legacy retirement | 否 | 是 | 是 | 归档旧 store |

### 8.2 Cutover 步骤

1. 公告/进入维护窗口；停止新 turn，等待运行中的 turn 结束；不再接受 memory 写。
2. 重新盘点 + 最终备份；确认 legacy 未变化（mtime/哈希）。
3. 把隔离转换结果落位：active Markdown 复制/rename 到 live project/local current；history/pending/attic/tombstone/journal/receipt/intent 复制到私有 `memory-v2` state root。写 `cutover.json`（run-id、manifest、时间、源 hash）。
4. 在 live 路径做 §7 全部校验；构建 index（只索引 current active）；验证 `git status` 只含 active current 与 ignore 文件，state 目录零文件进入 Git；对 project current 做 secret scan。
5. 启动 v2，禁用写（read-only），跑 smoke：list/read/recall/status；确认 current truth 正确，且 pending/history 不出现在 current 列表。
6. 切换配置 `memory.backend=markdown`（Stage 2 新增的字段，默认仍 legacy），重启/重新加载。
7. 打开 v2 写，跑受控写入 smoke（新建 project fact、pin、forget、pending accept），验证 history/journal/tombstone/receipt 落在 state root、current 只多出 active。
8. 解除冻结；监控 issue/错误。
9. 旧 store 置为只读（文件系统权限/应用标志），保留至少一个稳定窗口。

### 8.3 Cutover 前必须确认

- 备份 `BACKUP_COMPLETE` 存在且哈希校验通过。
- 隔离转换与 shadow compare 已由人 review。
- 冲突组/pending 未自动 active。
- project current 零 secret；state 目录在 repo 外且 0700。
- current root 只含 active Markdown；history/pending/journal/tombstone/receipt 只在 state root。
- local 已 ignore，project 未被 ignore；`git status` 不含内部状态文件。
- 回滚窗口和写后回滚限制已告知用户。
- 没有未解决的 parse_error/blocked 记录；或用户明确接受。

---

## 9. 回滚门

### 9.1 可无损回滚的阶段

| 时点 | 回滚方式 | 数据影响 |
| --- | --- | --- |
| Backup/隔离转换后 | 删除/保留隔离目录；live 未变 | 无 |
| Cutover 冻结写，v2 只读 | 配置切回 legacy；删除 live Markdown（或保留为只读） | 无 v2 写入，无丢失 |
| 第一次 v2 写之前 | 同上 | 无 |
| 第一次 v2 写之后 | **不是配置开关**；需要反向迁移或接受 v2 增量 | 可能丢失 v2 写入 |

### 9.2 写后回滚策略

- 不做长期双写；不接受“v2 写一份、legacy 再写一份”作为回滚方案。
- 如果必须写后回滚：
  1. 冻结 v2 写；
  2. 导出 v2 在 cutover 后的变更集（history/journal/MemoryChanged 事件）；
  3. 由独立工具把 v2 变更反向转换成 legacy 记录（或转成 legacy pending）；
  4. 人工审核后切回；
  5. 保留 v2 Markdown 作为归档，不删除。
- 这个反向工具不是本轮交付，也不在 Stage 4 默认路径里；cutover 前必须说明“写后回滚成本高”。

### 9.3 回滚验收

- 切回后 legacy store 的哈希与 cutover 前一致（或只多出反向迁移的显式变更）。
- v2 Markdown 不被删除，作为证据保留。
- 没有 secret 因回滚进入 repo。
- 事件日志保留 cutover/rollback 记录，便于审计。

---

## 10. Legacy Retirement

条件（全部满足）：

- v2 live 稳定窗口（建议 ≥ 2 周或团队约定的等价周期）；
- 无未解决的 P1 issue、无数据丢失、无 secret 泄露；
- 用户明确同意；
- 备份仍可读且校验通过。

步骤：

1. 把 legacy store 移到 `~/.leveler/archive/memory-v1-<project-id>-<timestamp>/`（不是删除）。
2. 写 `RETIRED` 标记与 manifest。
3. 移除代码中的 legacy read fallback（Stage 4）。
4. 保留至少一个发布周期的归档；不自动清理。
5. 更新 `CHANGELOG`/文档，说明旧路径不再读取。

禁止：直接 `rm -rf` 旧 store；在 cutover 当天删除；把旧 store 提交到 Git。

---

## 11. 幂等与重跑

- 每次运行有 `run_id`（时间 + project hash + input manifest hash）。
- 转换器对每个 legacy 文件计算 `legacy_sha256`；输出 manifest 记录 `legacy_sha256 → v2_id + content_sha256`。
- 重跑：
  - 输入未变、输出已存在且校验通过 → 只验证，不重写；
  - 输入变了 → 新 run-id；不覆盖 live；报告 diff；
  - 输出目录已有内容但不是本 run → 拒绝覆盖，要求新目录。
- 不允许“部分成功后再跑一次补齐”而不知道哪些已应用；journal/manifest 必须记录每个文件的完成状态。
- 迁移过程中崩溃：隔离目录可丢弃重来；live cutover 阶段崩溃按 journal 恢复；不猜测哪些文件已写。
- 不使用旧 `.lifecycle.lock` 作为迁移锁；迁移工具使用自己的 `migration.lock`，并在报告里记录持锁时间。

---

## 12. 验收清单

| 项 | 通过条件 |
| --- | --- |
| 盘点 | 每个 legacy 文件有 disposition；无遗漏 |
| 备份 | 全量、哈希、`BACKUP_COMPLETE` |
| 条数守恒 | legacy 文件数 = v2 记录数 + quarantine 数（一一对应） |
| 状态 | pending 未 active；superseded/expired/deprecated 不进入 active 查询 |
| Current/State | current 只有 active；history/pending/attic/tombstone/journal/receipt 只在 state root |
| Receipt | 每条 active 有 receipt，digest/revision 与文件一致 |
| 冲突 | 同 subject 不同 value 全部进 conflict group；无自动赢家 |
| 重复 | 只折叠精确重复；报告可追溯 |
| 身份 | keyless 不造 subject；ID 合法；无路径穿越 |
| 安全 | project 零凭据；报告零原文；local ignore 生效 |
| 解析 | 所有输出可读；schema 校验通过 |
| 索引 | 重建确定；active-only；中文/英文/代码 query 通过 |
| 链接 | 无断链/环；或全部报告 |
| Experience | 旧 verified 一致或明确降级；无伪造 verified |
| Shadow | 关键 query 无旧值优先；差异全部解释 |
| 文档 | 报告、cutover manifest、rollback 说明齐全 |
| Git | 无自动 commit；`git status` 仅 project active Markdown、ignore 文件与配置；不含 state/local/history/pending/journal |

---

## 13. 本机已知数据的具体预演（不执行）

以当前仓库旧 store 的 9 条 active 为例，预期映射：

| legacy id | 预期 v2 |
| --- | --- |
| `k-apibaseurl` | project active；authority 按 provenance；key `k-apibaseurl` |
| `k-apiKey`（大小写） | 规范化 subject_key；id 保留；报告大小写变体 |
| `k-apimodels` | project active |
| `k-configfilepath` | project active |
| `k-llmmodeldeepseekenabled` | project active |
| `k-llmmodelglm53enabled` | project active |
| `k-llmprovidervolcanoapikey` | **quarantine/deprecated**：evidence 含疑似凭据；local；脱敏 |
| `k-llmprovidervolcanoendpoint` | project active |
| `k-llmprovidervolcanoplan` | project active |

注意：

- 这只是按当前审计快照的预演，不是实际转换结果；真实值、provenance 和 secret 命中以迁移时重新盘点为准。
- `k-apiKey` 与 `k-apikey` 是否同一 subject 取决于 `semantic_key_of` 规范化结果；迁移不手动合并，交给规则和报告。
- 如果 `k-apikey` 的 evidence 含凭据，同样 quarantine；不因为 key 看起来像普通配置就放行。
- 当前 9 条全部是 project 或未知；没有 pending/archive，所以冲突/pending 路径在本机预演中为空，但迁移工具仍必须实现并测试这些路径。

---

## 14. 与实施阶段的关系

| 阶段 | 迁移相关工作 |
| --- | --- |
| Stage 1 | Markdown store、schema、journal、history、tombstone；不读 legacy |
| Stage 2 | service 接线、pending、authority、known_identities；只跑 legacy/v2 对照测试，不迁移真实数据 |
| Stage 3 | FTS5、context builder、logged injection；在合成/隔离 fixture 上验证 |
| Stage 4 | 本文件的 inventory/backup/convert/validate/shadow/cutover/rollback/retirement |

详细代码范围、测试与 gate 见 `docs/MEMORY_2_IMPLEMENTATION.md`。
