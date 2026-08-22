# CodeLeveler Mobile Beta Closure Report

**Status:** 已落地，随后 **FROZEN**（tag `mobile-beta-mvp`，见 [`MOBILE_FREEZE.zh-CN.md`](MOBILE_FREEZE.zh-CN.md)）。未做 Push / Multi-Agent UI / 系统分享插件。  
**对照：** [`MOBILE_RUNTIME_ALIGNMENT.zh-CN.md`](MOBILE_RUNTIME_ALIGNMENT.zh-CN.md)

> 手机创建任务 → Agent 执行 → Timeline → 干预 → **登记过的产物可拉取预览** → Task 工作区。

---

## 1. 完成内容

| 项 | 结果 |
| --- | --- |
| `fetch_attachment` RPC | 有。签名通道，按 sha256 从 Runtime media store 取字节，分片返回。无公开 URL。 |
| Preview 状态 | Loading / Success / Error / Empty |
| Markdown / Diff / Image | 有字节即渲染 |
| Task Detail（M11b） | TaskHeader 点开；复用 SessionState |
| Observe 配对 | 可以 fetch，不能 upload |
| 路径穿越 | 非 64 位小写 hex 在问 Runtime 之前拒绝 |

**仍然不是：** Agent 往仓库 `write_file` 不会自动变成 `attachment_added`。手机不扫仓库。产物必须先由 Runtime 登记进 media store。

## 2. 修改文件

Runtime / 协议：

- `crates/leveler-remote-protocol/src/tunnel.rs` — `RpcMethod::FetchAttachment`
- `crates/leveler-remote-agent/src/{bridge,attachments,lib}.rs`
- `crates/leveler-remote-agent/tests/attachment_fetch.rs`
- `crates/leveler-local-transport/src/lib.rs` — `fetch_attachment` + Unix `FetchAttachment` 帧
- `crates/leveler-media/src/lib.rs` — `put_blob` / `load_bytes`
- `crates/leveler-app/src/interactive.rs` — 走 media store；非图片 `AddAttachmentData` 存 blob

Mobile：

- `apps/leveler-mobile/lib/domain/app_controller.dart` — `fetchAttachment`
- `apps/leveler-mobile/lib/ui/artifact_preview.dart` — 四态
- `apps/leveler-mobile/lib/ui/task_detail_screen.dart` — 任务工作区
- `apps/leveler-mobile/lib/ui/{chat_screen,task_header}.dart`

## 3. 协议 / API

```json
{ "type": "rpc_request payload",
  "method": "fetch_attachment",
  "project_id": "…",
  "body": { "sha256": "<64 hex>", "chunk_index": 0 } }
```

成功（runtime 签名）：

```json
{ "sha256": "…", "mime_type": "text/markdown", "size_bytes": 123,
  "chunk_index": 0, "chunk_total": 1, "data_base64": "…" }
```

失败：无业务签名。`invalid_frame`（坏 id）、`not_found`、`payload_too_large`（>5 MiB）。

Unix 本地：`WireRequest::FetchAttachment` → 整包（本地帧上限远大于 5 MiB）；remote-agent 再切 512 KiB 片。

## 4. 安全

- 只认 media store 的 content hash，不打开 workspace 路径。
- sha256 必须是 64 位 `[0-9a-f]`，`../` 到不了磁盘。
- 与配对同一套验签；Observe 只读。
- 错误不回文件路径。
- 无短期 HTTP 下载地址（多客户端以后仍走签名 RPC 或另开设计，不在本阶段）。

## 5. UI

- Timeline Artifact Card → Preview（fetch）
- TaskHeader → Task Detail：Goal / Status / Current / Plan `done/total` / Artifacts / Approvals / Timeline
- 文本产物：复制。图片：预览。未接系统 Share sheet（避免新 native 插件）。

## 6. 测试

| 命令 | 结果 |
| --- | --- |
| `flutter test` | 全绿 |
| `flutter analyze` | No issues found |
| `cargo test -p leveler-media` | 12 过 |
| `cargo test -p leveler-remote-agent --test attachment_fetch` | 4 过 |
| `cargo test -p leveler-local-transport` | 27 过；`deliver_envelope_can_retry_after_revival_without_duplicate_effect` 复跑通过（socket 竞态，与本改动无关） |

未跑完整 `cargo test --workspace` / clippy。

## 7. 下一阶段（不要现在做）

1. Agent `write_file` 把 md/diff/图登记为 attachment（host，不是手机扫盘）
2. 系统 Share sheet
3. Push
4. Multi-Agent Fleet / Voice
