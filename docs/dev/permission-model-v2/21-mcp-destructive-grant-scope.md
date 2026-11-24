# 21 · MCP 破坏性授权的范围（Gate M1）

日期：2026-11-24
源码基线：`9c569b3d6cd27b27bb776284a4d7f480b73d09dc` + 本轮最小修复（未提交）
问题结论：**原始缺陷存在，已修复并复验。** 批准 Resource A 不再授权 Resource B。

## 1. 原始缺陷

上一轮 F3 的真实 MCP 记录（16 §2，Session `92094f2e-…`）已经显示：

> 同一工具、不同参数（`victim1.txt` / `victim2.txt`）共享一次 turn 批准；第二次没有再次询问。

当时的实现把 MCP 的会话授权键设为**配置工具名**（`mcp-tool:<tool>`），并且 `ApproveAlways` 会持久化一条仅按工具名匹配的 `permissions.yaml` 规则。对普通 MCP 工具这是既有契约；对带可信 `destructiveHint=true` 的工具，它让「批准删除 A」自动等于「批准删除 B」，违反本轮冻结契约：

- 不可逆 MCP 操作至少绑定 Server、Tool、实际操作与目标/参数身份；
- 无法表达资源身份时，至少绑定精确参数；
- 仅工具名相同不得扩大不可逆授权。

## 2. 修复前失败证据

新增的回归测试在修复前的源码上运行（原始输出：`evidence/final/windows/m1-prefix-unit-red.txt`）：

```text
cargo test -p leveler-agent --lib a_destructive_mcp -- --nocapture
...
assertion `left == right` failed: approving Resource A must not authorize Resource B
  left: 1
 right: 2
...
assertion failed: !first.request.always_persists()
test result: FAILED. 0 passed; 2 failed
```

含义：批准 A 后，对 B 的同一工具调用没有再次询问（approver 只被调用 1 次）；destructive 工具的 `always` 仍声称可持久化。

## 3. 最小修复

| 层 | 改动 |
|---|---|
| `leveler-execution::ApprovalRequest` | 新增 `#[serde(default)] exact_call_only: bool`；`always_persists()` 在其为 true 时返回 false（UI/CLI 不再提供 Always） |
| `leveler-agent::Executor::resolve_policy` | 当工具有类型化 `destructiveHint=true` 时设置 `exact_call_only`（不依赖模型文本） |
| `leveler-agent::Executor::ask` | `mcp_session_key` 对 exact-call-only 请求返回 `None`；会话/`always` 决策只插入精确签名 `tool + SHA256(arguments)`；防御性地把 `ApproveAlways` 降级为本次 turn 的精确授权，不再排队持久化规则 |
| `SECURITY.md` | 公开契约同步：destructive MCP 的审批绑定精确参数，不提供/不持久化工具级 Always |
| `leveler-cli` / `leveler-app` | 构造点和测试补齐新字段；投影沿用 `always_persists`，无需新增 UI 字段 |

精确签名原本就包含工具名与完整 JSON 参数；工具名含 server，因此不同 server 天然不同。普通 MCP 工具仍保留工具级 turn 授权（旧 Auto 兼容），V2 Auto 仍 0 审批，Full 行为不变。

## 4. 真实 MCP 端到端复验

### 4.1 隔离 fixture

真实 stdio MCP server（Python，JSON-RPC），由产品自身的 MCP transport 启动，经生产 `ToolHost` 调用。每次请求写入 `protocol.log`；资源为独立的 `resource-a.txt` / `resource-b.txt`。工具：

| 工具 | 类型化 annotations | 行为 |
|---|---|---|
| `write_resource` | `destructiveHint=false` | 写资源 |
| `delete_resource` | `destructiveHint=true` | 删除资源 |
| `unannotated_delete` | 无 annotations | 删除资源（接受风险） |

证据：`evidence/final/mcp-grant-scope/`。

### 4.2 V2 替我审批：批准 A、拒绝 B（生产 runtime + 真实 MCP）

Session 由测试驱动（in-process 生产 `Application` + `InProcessRuntimeClient`，真实 `ClientCommand::ApprovalDecision`）。脚本：启用 external_tools → 普通写入 → 删除 A → 再次删除 A（同参数）→ 删除 B。

| 观测 | 值 |
|---|---|
| 审批数 | **2**（A 一次、B 一次；同参数 A 不重复询问） |
| 两次 reason_code | 均为 `irreversible.mcp_destructive` |
| `always_persists` | 均为 **false** |
| 决策 | `approve_session`（A）→ `deny`（B） |
| 服务端协议 | `delete_resource` 只收到两次 `resource-a.txt`；`resource-b.txt` 从未收到 `tools/call` |
| 副作用 | A 删除；**B 保留**；普通写入 `note.txt = ordinary-mcp` |
| 普通 MCP 审批 | **0** |

原始报告：`mcp-grant-scope-v2-assisted.json`。

### 4.3 真实 CLI + 真实 MCP（同一修复）

真实 `leveler.exe` 进程、mock provider 的确定性工具序列、stdin 审批（`t` = 本回合，`d` = 拒绝）。stderr 出现两次：

```text
⚠ approval needed mcp__fixture__delete_resource wants to run a Network action:
    MCP tool `mcp__fixture__delete_resource` declares destructive behavior
```

结果：`resource_a_exists=false`、`resource_b_exists=true`、`note=ordinary-mcp`、协议日志中只有 A 的两次 `tools/call`。完整 stdout/stderr/protocol：`mcp-cli-{run.jsonl,run.stderr,protocol.log}`。Session `32a0bf55…` 等的解析见 `mcp-cli-meta-after.txt`。

### 4.4 Full、第一档与无注解工具

| 场景 | 审批 | 结果 | 证据 |
|---|---:|---|---|
| Full + destructive 脚本 | 0 | A、B 都删除；`delete_resource` 共 3 次调用 | `mcp-grant-scope-full-and-tier1.json` |
| V2 请求批准（LegacyAuto）+ destructive：A 批准一次、B 拒绝 | 2（理由码 `legacy.auto`） | A 删除，**B 保留**；协议只有 A 两次 | 同上 |
| V2 请求批准 + 普通 MCP 写两次（不同内容） | 1（首次；旧 Auto 工具级 turn 授权） | 第二次不再问，`note=second` | 同上 |
| V2 替我审批 + `unannotated_delete` | 0 | 文件真实删除（已接受的残余风险） | `mcp-grant-scope-unannotated.json` |

第一档继续继承旧 Auto 的普通 MCP 授权语义；已确认的「不可逆授权按工具名扩散」在第一档同样被消除（A/B 复验）。

## 5. 覆盖矩阵

| 要求 | 覆盖方式 | 结果 |
|---|---|---|
| 同资源重复调用 | 同一参数删 A 两次 | 第二次不再询问，仍可执行 |
| 不同资源 / 不同参数 | 删 A 后删 B | B 重新审批；拒绝后无 B 的 `tools/call` |
| 不同 Server | 签名含完整工具名（`mcp__<server>__<tool>`），工具级键已移除 | 不同 server 必然不同签名 |
| Turn 边界 | 授权集合在 turn 的 `Drive` 上，不持久化 | 不跨 turn |
| Session 边界 | 不写 `sessions.db` / 规则文件 | 新 Session 重新询问 |
| 用户拒绝 | A 批准、B 拒绝 | B 文件保留、服务端未收到 B 调用 |
| 用户批准 | A 的 `ApproveSession` 仅覆盖 A 参数 | A 被删除 |
| Server 重启 | 既有 `mcp_crash_resume` / `mcp_lifecycle` 覆盖传输；授权是 turn 内存状态 | 不重放未知结果 |
| 缺少 destructiveHint | 真实 `unannotated_delete` | 0 审批（接受风险） |
| 普通 MCP 不增加审批 | V2 Auto 写入 0 审批 | 是 |
| Full 不变 | 0 审批，全部删除 | 是 |

## 6. 回归

- `leveler-agent --lib`：614 passed（修复后）。
- `leveler-execution --lib`：367 passed。
- `leveler-app --test permission_persistence` 8、`full_permission_lifecycle` 8、`full_permission_acceptance` 5、`execution_ownership` 13、`resource_grant_lifecycle` 1（Windows）。
- `SECURITY.md` 已同步；没有新增 UI 字段，没有新权限模型，没有额外的模型调用。
