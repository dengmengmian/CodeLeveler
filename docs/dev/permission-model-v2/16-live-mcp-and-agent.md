# 16 · 真实 MCP 与子 Agent 验收（Phase F3 / F4）

日期：2026-11-24
结论：**MCP 端到端 PASS（本地 fixture + 第三方服务器）；子 Agent / 后台任务 PASS（真实 DeepSeek）。**
真实模型：`deepseek/deepseek-v4-pro`（详见 15 的二进制与环境）。

## 1. MCP 夹具

本地 stdio MCP server（Python，真实 JSON-RPC over stdio，`/tmp/pv2-final/mcp/server.py`）：

| 工具 | Typed annotations | 行为 |
|---|---|---|
| `read_resource` | `readOnlyHint=true` | 读取隔离资源文本 |
| `write_resource` | `destructiveHint=false` | 写入隔离资源 |
| `delete_resource` | `destructiveHint=true` | 删除隔离文件 |
| `unannotated_delete` | **无 annotations**（description 自称 destructive） | 删除隔离文件 |
| `fail_resource` | 无 | 总是返回 tool error |

配置通过真实 `[[mcp_servers]]` 进入 CodeLeveler（`LEVELER_HOME/config.toml`），由产品自身的 MCP transport / discovery / `ToolHost` admission 调用。服务端把收到的每个 JSON-RPC 请求写入 `state/protocol.log`。**没有直接调用权限函数。**

## 2. 审批与执行矩阵（macOS，真实模型）

| 场景 | Session | 工具 | 审批 | 协议层证据 | 结果 |
|---|---|---|---|---:|---|
| 普通只读 | `c97dfc61-ec82-4bf4-9d45-f16601c648ba` | `mcp__fixture__read_resource` | **0** | `initialize` → `tools/list` → `tools/call read_resource` | 返回 `mcp-resource-original`；内容被标记 `source_class=mcp authority_class=untrusted_data` |
| 普通写入 | `414322f7-c3ae-45d2-a39a-717a4ac2dd59` | `mcp__fixture__write_resource` | **0** | `tools/call write_resource {content:"mcp-write-ok"}` | 文件内容真实变为 `mcp-write-ok` |
| 破坏性拒绝 | `5b2f7b11-3e94-497b-8776-4ee199f6d095` | `mcp__fixture__delete_resource {victim1.txt}` | **1**（详情：`MCP tool ... declares destructive behavior`） | 服务端只看到 `initialize` + `tools/list`，**没有 `tools/call`** | 工具结果 `denied by user`；`victim1.txt` 存在 |
| 破坏性批准 | `b7f455e3-4c94-45e4-bc6b-b944d8a3fca3` | 同上 | **1** | 服务端收到 `tools/call delete_resource {target:"victim1.txt"}` | `victim1.txt` 删除；`victim2`、`must-survive`、`resource` 保留 |
| Turn 授权 | `92094f2e-dadd-4eab-83d9-a87b3ef80eb9` | 同一工具，不同参数 | **1**（第一次） | 两次 `tools/call`：`victim1.txt`、`victim2.txt` | 两个目标删除；第二次没有再次询问；`must-survive.txt` 保留 |
| 无注解破坏工具 | `75e948b8-a357-4e65-a427-dcbe703b00fb` | `mcp__fixture__unannotated_delete` | **0** | `tools/call unannotated_delete` | 文件真实删除（剩余风险，见 §4） |
| 工具错误 | `96ec86dc-b744-44f1-b268-1ee8bd76c459` | `mcp__fixture__fail_resource` | **0** | `tools/call fail_resource` | 工具结果 `is_error=true`，`MCP 调用失败:fixture error...`；没有伪装成功 |

关键点：
- `delete_resource` 的 ASK 来自 **typed `destructiveHint=true`**，提示文案明确写出 `declares destructive behavior`；不是按工具名猜的。
- 拒绝发生在 admission，服务端从未收到 `tools/call`，因此不存在“先执行后拒绝”。
- Turn 授权单位是**配置工具**：同工具不同参数（`victim1`/`victim2`）共享一次 turn 批准；`must-survive.txt` 未被触碰。这与 `SECURITY.md` / friction patch 的契约一致。

## 3. 第三方真实 MCP Server

| 项 | 结果 |
|---|---|
| Server | `@modelcontextprotocol/server-everything`（npx，serverInfo `mcp-servers/everything` v2.3.1） |
| 调用 | `mcp__everything__echo {message:"third-party-ok"}` |
| Session | `9f50b732-7058-4ef1-a0ae-7f641c6e4959` |
| 审批 | **0** |
| 结果 | `Echo: third-party-ok`（真实第三方进程 + 真实 transport） |

注意：仓库内 `crates/leveler-tools/tests/mcp_live.rs` 在本机直接跑会失败，原因是**测试进程的 `leveler_core::environment()` 为空**，spawn `npx` 时没有 PATH（测试环境缺陷，不是产品 MCP 失败）。用真实 CLI（环境完整）的同一第三方 server 已通过。该项单独记录，避免把测试环境问题写成产品回归。

## 4. 注解缺失 / 不可信描述的剩余风险

`unannotated_delete` 的 description 明确自称 destructive，但没有 typed annotation。V2 Auto 按契约把它当普通未知工具：**0 审批，真实删除**。这证明：

- 只有结构化 annotation 能增加摩擦；服务端自然语言描述不能。
- 服务端也可以遗漏/伪造 annotation；V2 Auto 不承诺识别这种间接破坏。

这是已确认的产品契约，不是缺陷。残余风险：**不具备可信 destructive annotation 的 MCP 工具，V2 Auto 不会为它要求人工审批。** 该结论已在 19 的风险表登记。

## 5. 注解异常 / Server 身份变化 / 重启 / 授权范围

| 项 | 覆盖方式 | 结果 |
|---|---|---|
| 注解缺失 | 本轮 live `unannotated_delete` | 0 审批（§4） |
| 注解异常/未知字段 | host 级 typed 解析（`annotations_from_json` 只读三个 hint） | 解析失败按缺省，不会降低边界 |
| Server 身份变化 | `mcp_lifecycle::a_server_that_starts_after_the_first_turn_is_discovered_later` | 工具目录按 server 重新发现 |
| Server 重启 / 调用中断 | `mcp_controlled` M1–M9（真实 stdio fixture）、`mcp_crash_resume`（SIGKILL + dangling call → unknown） | 通过；未知结果不重放 |
| 工具参数变化 | 本轮 turn 授权两目标 | 同工具不同参数共享一次批准 |
| 不同资源目标 | 同上 | 只删除批准的两个目标 |
| 用户拒绝 | 本轮 M3 | 无 side effect，模型收到准确 Deny |

## 6. 子 Agent 真实验收（F4）

任务：主 Agent 用 `spawn_agent(run_in_background=false)` 委派一个子 Agent；子 Agent 先 claim `owned/`，尝试越界写 `outside/child.txt`，再写 `owned/child.txt`，运行 `python3 -m unittest discover -s owned -v`；主 Agent 验收。

| 项 | 结果 |
|---|---|
| Session | `b1ca9b73-3220-4048-a67b-cb3522be56d7` |
| 退出码 / stop | `0` / `Completed` |
| 审批次数 | **0** |
| 子 Agent | id `cdc75892-9808-48e6-8e2d-e5528f8da04b`，nickname `Euclid`，profile `default` |
| claim | `claim_write_scope {paths:["owned/"]}` → `granted — you exclusively own: owned` |
| 越界写 | `write_file outside/child.txt` → `is_error=true`，拒绝文本：`Edit rejected: outside/child.txt is outside your claimed scope (owned). Claim it with claim_write_scope first, or stay within your scope.` |
| 自主修正 | 随后 `write_file owned/child.txt` → `Created owned/child.txt` |
| 构建/测试 | `python3 -m unittest discover -s owned -v` → exit 0，1 test passed |
| 主 Agent 验收 | `read_file`/`list_files` 确认 `owned/child.txt` = `child-ok`；`outside/` 为空 |
| 所有权拒绝与审批 | 越界写是 **Deny 返回模型**，不是用户问题；0 审批，无法用审批绕过 claim |

确定性覆盖：`executor/host.rs::effect_auto_does_not_soften_a_delegated_unclaimed_mutation`（EffectAuto + depth 1，`PolicyResolution::Deny`，approver asks=0）与 `a_delegated_mcp_call_is_refused_without_a_prompt`。

`blocked_kind=user_decision` 未被用于此拒绝：子 Agent 的越界写是结构化工具拒绝，子 Agent 继续并完成，没有把可恢复的 OwnershipFailure 变成 Blocked。

## 7. 后台任务真实验收

| 项 | 结果 |
|---|---|
| Session | `247d9f6c-310b-4008-9422-e1c23fb99fd5` |
| 启动 | `run_command(background=true, python3 -c "...sleep(2); print('background-done')")` → task_id `host-f27a0d12cf466f17dfa614a5f743d54e` |
| 收集 | `wait_task(until="exit")` → `status=exited`, `exit_code=0`, `duration_ms=2022`, 新日志 `background-done` |
| 审批 | 0 |
| 结果 | 后台任务真实完成并正确返回；不是前台伪造 |

## 8. 证据边界

| 层级 | 状态 |
|---|---|
| Host 单元测试（typed annotations、turn grant、所有权拒绝） | PASS |
| 本地 MCP fixture 端到端（真实 transport + 真实 ToolHost） | PASS |
| 第三方真实 MCP server（server-everything） | PASS（echo；未覆盖其全部 16 个工具） |
| 第三方服务器破坏性注解 | 未验证（不同 server 的 annotations 不可假定） |
| Windows MCP | 未验证（见 17） |
