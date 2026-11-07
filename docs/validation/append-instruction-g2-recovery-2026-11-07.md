# 运行中追加指令 G2 流中断与恢复复核 · 2026-11-07

## 结论

G2 的历史 FAIL 保留：在冻结二进制 `623ebbf2...` 上，该轮没有完成“Access 布局 + 管理员管理”的业务目标。复核同时确认，这是一次**上游模型流在完成前结束**造成的可恢复模型调用失败，不是 Runtime 恢复缺陷：

- 第 3 次请求收到非法 SSE chunk，Runtime 记为 `Decode`，按既有上限追加一次 protocol repair，没有越过 `MAX_DECODE_RETRIES = 2`。
- 第 4 次请求在已收到正文、推理和工具参数后，上游在没有任何 `finish_reason` 的情况下结束；Runtime 记为 `StreamInterrupted`。已有工具参数时重放会重复输出，生命周期按契约拒绝自动重试。
- 回合以 `failed` 结束，用户看到明确失败；Goal 保持 `running`；追加原文保留在 `goals.amendments`；Resume 按同一 session 继续并真实完成。

因此：**不需要修改追加指令机制或 Runtime 恢复代码。** 本轮产品代码零变化；唯一的代码修改是 N2 探针，见 [N2 修正与复核](append-instruction-n2-recheck-2026-11-07.md)。

## 证据

| 项 | 值 |
| --- | --- |
| 冻结 HEAD | `788e18a2d53ceb038b6a85d062953c0604a8c542` |
| 被测二进制 SHA-256 | `623ebbf28fcfac27940cfc0a87cb7a94d4de5ab6ecfd203ea4d4c3d567501858` |
| 模型 / 推理档 | `deepseek-v4.1-flash-1017` / `high` |
| G2 封存运行 | `runs/20261107-110508-phase3-blockers-G-2-product-access-f09xones/` |
| Resume 复核运行 | `runs/20261107-122000-G2-resume/` |
| 独立业务复核 | `runs/20261107-122000-G2-resume/G2_RESUME_VERIFICATION.json` |
| 汇总事实 | `runs/20261107-122000-G2-resume/G2_RESUME_FACTS.json` |

归档根：`/Users/mengmian/CodeLeveler-final-archives/20261107T100053-blockers/`。

## 失败因果链

| 请求 | 消息数 | 时长 | 上游响应（代理摘要） | Runtime 结果 |
| --- | ---: | ---: | --- | --- |
| 1 | 3 | 2.59s | HTTP 200，22,229 B，`finish_reason=tool_calls` | 正常完成 |
| 2 | 7 | 6.56s | HTTP 200，38,645 B，`finish_reason=tool_calls` | 正常完成 |
| 3 | 12 | 61.88s | HTTP 200，1,120,374 B，含合法 `tool_calls` finish chunk，也含非法 chunk | `Decode`：`malformed stream chunk: EOF while parsing a string at line 1 column 253`；retryability `Never`；追加 1 次 protocol repair |
| 4 | 13 | 92.80s | HTTP 200，424,243 B，无 `finish_reason`，有工具参数增量 | `StreamInterrupted`：`stream ended before completion`；progress `{text:true, reasoning:true, tool_args:true}`；retryability `Caution`；生命周期不重试 |

第 4 次请求带上了第 3 次失败后追加的 repair 消息，说明协议修复路径本身生效。

## 七个问题的直接回答

| 问题 | 结论 |
| --- | --- |
| 为什么第 3 次请求触发解码 + 协议修复？ | SSE 的 `data:` 负载不是合法 JSON，解析在字符串内部遇到 EOF（第 1 行第 253 列）。Runtime 将该轮判为 `Decode`，按既有 `MAX_DECODE_RETRIES` 上限把精确错误反馈给模型并要求重发同一工具调用；没有猜测参数，也没有越过 repair 预算。 |
| 为什么第 4 次请求没有完整 `finish_reason`？ | 上游在已经给出正文、推理和工具参数增量后结束，既没有 `finish_reason`，也没有 `[DONE]`。协议层因此无法判定完成。 |
| `provider_retries_exhausted=false` 是否符合重试策略？ | 符合。该字段是传输层诊断标志，不是逻辑重试生命周期；逻辑生命周期的 `retry_attempts` 为 `None`，表示从未进入重试。`StreamInterrupted` 已收到 text/tool_args 时 `received_no_answer_or_tool_data()` 为 false，按 `is_retryable_by_lifecycle` 不可自动重放，所以根本没有重试，也就不存在“耗尽”。 |
| 哪一层最先产生 `stream ended before completion`？ | `crates/leveler-protocol/src/openai_chat/mod.rs` 的 `decode_stream`：上游字节流结束且 assembler 未 completed 时，协议适配层产出 `ModelErrorKind::StreamInterrupted`。Runtime 只是如实向上传递，没有把残流当成功。 |
| Goal 保持 `running` 是否符合恢复契约？ | 符合。失败窗口只结束当前 turn（`outcome=failed`），不 settle goal；goal 仍为 `running`、`windows_run=1`。Resume 后同一 goal 变为 `settled`、`windows_run=2`。 |
| Resume 是否可靠？ | 本轮实测可靠：同一 session `9be56382-1c35-45b8-8fce-4e4e5ef0a939`，exit 0，13 轮，`session_completed` / `Completed`；布局契约 4/4、项目全量 7/7 通过；`/admin` 真实 HTTP 200。 |
| 追加指令是否仍然生效？ | 是。Resume 第一次请求 `messages[6]` 是追加原文“还有管理员管理也没有。”，`messages[13]` 是上一轮的 protocol repair；两者都在。`goals.amendments` 保留原文，journal 只消费一次，transcript 只有一个结构化 user 行且 `input_id` 与 journal 一致。 |

## 责任分层

| 层 | 已证明 | 不能证明 |
| --- | --- | --- |
| Runtime / Agent | 没有生成畸形字节；按 `Decode` / `StreamInterrupted` 分类；repair 有界；有工具参数时不自动重放；turn 失败对用户可见；Resume 走同一 goal。 | — |
| 协议适配层 | 畸形 chunk 被判为 `Decode`；无终止事件被判为 `StreamInterrupted`；没有把残流当成功。 | — |
| 上游链路 | 非法 chunk 的解析失败发生在 Runtime 收到字节之后、业务逻辑之前；第 4 次确实没有终止事件。 | 封存轮只保留了代理摘要（HTTP 状态、finish 原因、字节数），没有保存原始响应体，因此**无法把 Provider 原始生成、网关重新分包、网络截断三者分开**。 |
| 本地录制代理 | 两轮都是 HTTP 200，`client_error=null`；代理只转发上游字节并记录摘要。 | 代理不是独立取证点，不能单独证明上游某一环。 |

结论按证据强度表述：**可以排除 Runtime 逻辑和协议分类错误；上游链路确有问题，但 Provider / 网关 / 网络的具体归属仍是 unknown。**

## 恢复契约逐项核对

| 契约 | 观察 | 结果 |
| --- | --- | --- |
| 未完成工具调用不被当作已执行 | 事件表没有第 4 次请求那个工具调用的 `tool_call_started` / `tool_call_finished`；失败轮 6 个工具全部是 `run_command`（unittest）和 `read_file`，均为只读/测试 | PASS |
| 用户看到清晰失败 | 终帧：`✗ 请求失败`、`✕模型响应接收中断`、`失败 · 3m 07s · 4 轮 · 6 次工具` | PASS |
| 追加指令不丢 | `goals.amendments` 为 `["还有管理员管理也没有。"]`；journal 行 `3dc51958-...` 状态 `consumed`，只消费一次 | PASS |
| transcript 身份保留 | 唯一 user 行 ordinal 3、`input_id=3dc51958-...`，与 journal 一致 | PASS |
| Goal 不被误结算 | 失败后 `state=running`，`windows_run=1` | PASS |
| Resume 继续未完成目标 | 同一 session、同一 goal；resume 后 `state=settled`，`windows_run=2` | PASS |
| 追加在 Resume 中仍送达 | 第一次 resume 请求 `messages[6]` 为追加原文 | PASS |
| 已确认的非幂等工具不重复执行 | 失败轮没有任何写操作；resume 轮 12 个工具调用的 call ID 全部是新 ID，且都属于新 turn `27fff7f5-...` | PASS |
| 恢复后真实完成业务 | `app.py` + `static/app.css` 落地；`tests.test_access_layout` 4/4、`discover -s tests` 7/7；`/access`、`/roles`、`/admin`、`/static/app.css` 均 200，`/admin` 含“管理员”标题与 `Alice Chen`，`/access` 含指向 `/admin` 的链接 | PASS |

## Resume 实测

```sh
cd /tmp/lv-append-nbgeho7q/repo
LEVELER_HOME=/tmp/lv-append-nbgeho7q/home LEVELER_LOG=info \
  /Users/mengmian/CodeLeveler-final-archives/20261107T100053-blockers/bin/leveler \
  run --resume 9be56382-1c35-45b8-8fce-4e4e5ef0a939 --auto-approve --output jsonl
```

| 项 | 结果 |
| --- | --- |
| 退出码 | 0 |
| 最终事件 | `session_completed`，`stop_reason=Completed`，`rounds=13` |
| 修改文件 | `app.py`、`static/app.css` |
| 模型请求 | 失败轮 4 次 + resume 13 次；resume 每次都有明确 `finish_reason` |
| 目标 | `settled`，`windows_run=2` |
| 布局测试 | 4/4 OK |
| 项目测试 | 7/7 OK（22.956s） |
| `/admin` HTTP | 200，`text/html`，含管理员标题和 `Alice Chen` |
| `/static/app.css` | 200，`text/css`，3,321 B |

Resume 的请求体、上游流、数据库检查点、工作区 diff 和文件清单都在 `runs/20261107-122000-G2-resume/`：`before/` 是恢复前状态，`after/` 是恢复后状态，`after/sessions-after.sqlite3` 通过 `sqlite3 .backup` 封存，`PRAGMA integrity_check` 为 ok。

## 判定与边界

| 项 | 判定 |
| --- | --- |
| G2 该轮业务 | **FAIL（保留）** |
| G2 作为可恢复模型调用失败 | **处理正确**：上游流异常，Runtime 分类、限额修复、失败可见、状态保留、Resume 可续 |
| 是否需要修改 Steer / 追加指令机制 | **否** |
| 是否需要修改 Runtime 恢复代码 | **否**，未发现确定性缺陷 |
| 消息可靠性 | **PASS** |
| 追加指令产品闭环 | **PASS**：同一 session 恢复后真实完成追加要求 |

限制：

- 没有做受控代理故障注入复现同一畸形字节；G2 的恢复是用**原始失败 session 的真实 Resume**验证的，不是重跑到模型成功。
- 封存轮没有原始响应体，Provider / 网关 / 网络的责任细分仍为 unknown。
- 本轮只实测 macOS；Linux / Windows 为 UNKNOWN。
