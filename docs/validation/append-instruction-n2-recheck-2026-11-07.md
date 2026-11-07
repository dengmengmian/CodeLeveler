# 运行中追加指令 N2 探针修正与复核 · 2026-11-07

## 结论

N2 的原始 FAIL 保留，不回写：封存运行 `20261107-112704-phase3-blockers-N2-1-product-health-xhxwr1i9` 的 `one_transcript_row` 和 `row_keeps_journal_identity` 当时确实失败。失败原因是**探针用原始 JSON 文本做子串匹配**，不是 transcript 缺行。

修正只改测试探针，不改生产消息内容、不隐藏原始失败：

- `scripts/append_instruction_phase3.py` 先解析 `session_messages.payload`，只接受 `role == "user"`，按完整 text 段匹配原文。
- 身份判断使用 `session_messages` 的 `id` / `ordinal` / `turn_id` / `input_id` 与 `session_inputs` 行逐项比对。
- 新增 4 个回归测试，覆盖双引号、换行、反斜杠和 Unicode；同时锁定角色、精确文本和畸形 payload 的边界。
- 只读复核封存库后，两项检查均为 true，数据库前后 SHA-256 不变。

原始记录在归档 `N2_TRANSCRIPT_RECHECK.json` 和 `executions.json`；修正后的独立复核在 `runs/N2_TRANSCRIPT_RECHECK_V2.json`。

## 原始失败

追加原文：

```text
另外，虽然与布局不同，请在本次任务一并增加 /health 接口，返回 JSON {"status":"ok"}，并实际请求确认。
```

该句含双引号。写入 `session_messages.payload` 后，引号被 JSON 转义为 `\"`，原始文本不再作为连续子串存在。旧探针 `user_message_rows` 直接在序列化 payload 上查找原文，返回 0 行，于是：

| 检查 | 旧结果 | 实际事实 |
| --- | --- | --- |
| `one_transcript_row` | FAIL | 库里存在 1 条 user 行 |
| `row_keeps_journal_identity` | FAIL | 该行 `input_id` 等于 journal 行 ID |

原始 FAIL 是测量错误，但记录本身是真实的失败结果，因此不改写。

## 探针修正

`scripts/append_instruction_phase3.py`：

| 函数 | 行为 |
| --- | --- |
| `decoded_user_texts(row)` | 解析 JSON；只取 `role == "user"`；`content` 为字符串时整体返回，为数组时只取 `type == "text"` 且 `text` 为字符串的段；畸形 payload 返回空 |
| `user_message_rows_in(rows, text)` | 返回解码后 text 段与原文**完全相等**的行；不做子串、前缀或近似匹配 |
| `Case.user_message_rows(text)` | 从 `dog.messages(self.home)` 读取结构化行后调用上述函数 |

探针仍使用真实数据库内容，不读 `result.json` 的自述，也不修改任何生产消息。

## 回归测试

`scripts/tests/test_append_instruction_blockers.py` 新增 `StructuredTranscriptRowTests`，先红后绿：

| 测试 | 锁定的行为 |
| --- | --- |
| `test_decoded_text_matches_even_when_raw_payload_cannot` | 含 `"`、换行、`\` 和 Unicode 的原文在原始 payload 中不可见，结构化匹配仍命中同一条消息 |
| `test_role_and_exact_text_decide_the_match` | assistant 行和只差一个尾随空格的近似文本都不算命中 |
| `test_string_content_is_still_structured` | `content` 为字符串时仍按结构化内容匹配 |
| `test_broken_or_non_text_payloads_are_not_guessed` | 非法 JSON 和 image-only 段不会被猜测为文本行 |

红：实现前测试因 `user_message_rows_in` 不存在而失败（`AttributeError`），不是环境错误。实现后该文件 8/8 通过；证据脚本测试 6/6 通过。

## 独立复核

`n2_recheck.py` 以 `file:<db>?mode=ro` 只读打开封存的 `sessions.sqlite3`，在前后各取一次 SHA-256，解析 payload 后与 `session_inputs` 比对。结果：

| 项 | 值 |
| --- | --- |
| 数据库 SHA-256（前/后） | `f17d56249fc1850d1fb85f9fcaef115d8917eebc1110c7032a4113761006845d` / 同值 |
| `raw_substring_ordinals` | `[]` |
| `decoded_user_rows` | 1 行：ordinal 3、message id 4、`turn_id 10956c85-...`、`input_id 6910b743-b200-4722-9b67-ce4fedb5585a` |
| journal 行 | `6910b743-...`，state `consumed`，ordinal 3，内容与追加原文完全一致 |
| `one_transcript_row` | true |
| `row_keeps_journal_identity` | true |
| 数据库是否被修改 | 否 |

## 边界

- 本修正不改变追加指令的投递、持久化、transcript 写入或 Resume 行为。
- 封存的原始 FAIL、`executions.json` 和历史报告均未回写。
- 该复核只证明“探针在封存库上能正确识别行和身份”，不替代真实模型轮次或产品验收。
