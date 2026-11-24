# 10 · Phase 2D 报告：四臂 Dogfood + Full

日期：2026-11-24
阶段结论：**PASS（macOS 本地）**
运行时：真实 `Application` + `InProcessRuntimeClient`，真实工具执行、真实文件系统、真实进程、真实网络目的地检查。
模型侧：确定性 Mock Provider（不是真实 LLM）。本阶段测量的是权限决策与执行后果，不是模型质量。

## 1. 运行方式

```text
PERMISSION_DOGFOOD_REPORT=/tmp/permission-dogfood-macos.json \
  cargo test -p leveler-app --test permission_model_v2_dogfood -- --ignored --nocapture
```

证据文件：`docs/dev/permission-model-v2/evidence/permission-dogfood-macos.json`（运行时复制自上面的 report）。

## 2. 臂

| 臂 | contract | profile | PolicyKind |
|---|---|---|---|
| `v1_request_approval` | V1 | request_approval | LegacyRestricted |
| `v1_assisted` | V1 | assisted | LegacyAuto |
| `v2_request_approval` | V2 | request_approval | LegacyAuto |
| `v2_assisted` | V2 | assisted | EffectAuto |
| `full` | V2 | full_access | Full |

## 3. 审批矩阵（次数）

| case | v1_req | v1_auto | v2_req | v2_auto | full |
|---|---:|---:|---:|---:|---:|
| write_inside | 0 | 0 | 0 | 0 | 0 |
| write_outside | 1 | 1 | 1 | **0** | 0 |
| python_script | 0 | 0 | 0 | 0 | 0 |
| shell_build | 0 | 0 | 0 | 0 | 0 |
| git_push_local | 0 | 0 | 0 | 0 | 0 |
| delete_real_file | 1 | 1 | 1 | **1** | 0 |
| git_reset_hard | 1 | 0 | 0 | **1** | 0 |
| private_network | 1 | 1 | 1 | **0** | 0 |

## 4. 执行后果

| 断言 | 结果 |
|---|---|
| v2_assisted 工作区外写入落盘 | `file=true` |
| v2_assisted 私有 LAN 请求到达服务端 | `served=true` |
| v2_assisted 本地 Git push 到达 bare remote | `pushed=true` |
| v2_assisted 拒绝真实删除后文件仍存在 | `victim_exists=true` |
| v2_assisted 拒绝 `git reset --hard` 后未提交修改仍在 | `dirty_survived=true` |
| Full 真实删除执行 | `victim_exists=false` |
| Full `git reset --hard` 执行 | `dirty_survived=false` |
| Full 全程 0 审批 | 是（8/8 case） |

## 5. 兼容性结论

- `v1_assisted` 与 `v2_request_approval` 在全部 8 个 case 上审批次数一致 → 第一档确实继承旧 Auto。
- V1 两臂与 V2 第一档的差异只在 `git_reset_hard`：旧 Auto 不 ASK，V1 严格档 ASK；这是既有策略差异，不是 V2 引入。
- `v2_assisted` 与旧 Auto 的关键差异正好是产品要求的三处：工作区外普通写入、私有网络、直接删除/历史丢失。

## 6. 未测量 / 限制

| 项 | 状态 | 说明 |
|---|---|---|
| 真实 LLM | NOT_MEASURED | 使用确定性 Mock Provider；权限决策不依赖模型文本 |
| 真实 MCP server | UNVERIFIED | 无 MCP fixture 接入；证据为 host typed-annotation 单测 |
| 子 Agent / 后台任务 | PARTIAL | 现有 `parallel_ownership`、`background_task_evidence` 等回归全绿；本 dogfood 未单独跑 V2 子 Agent live case |
| 私有网络 | PASS（本机 LAN `10.9.9.108`） | 仅 macOS；Linux 容器无 `ipconfig`，该 case 会标记 NOT_VERIFIED |
| Windows | UNVERIFIED | 无实机，见 11 |
