# 媒体存储生命周期（R6-A3）

日期：2026-11-28
结论：**macOS PASS（真实 daemon 实测）**；Windows **NOT VERIFIED**。

## 问题

图片 store 是内容寻址目录（`<state_dir>/media`）。用户附件与工具图片的权威字节
分别持久化在 `session_messages` / `turns` / `events` 的 base64 内容里，工具图片另有一个
`tool_call_images` 事件保存 `StoredImageRef.sha256`。此前：

- 没有配额或清扫，删除 Session 不会回收共享对象；
- 写入是 `create_dir_all` + `fs::write`，崩溃可能留下截断对象，之后同名导入不会修复；
- 读取用 `Path::is_file()`，会跟随符号链接；
- 文件权限继承 umask（实测旧目录 `0755`、文件 `0644`）。

## 本轮实现

| 项 | 行为 |
| --- | --- |
| 原子写入 | 同目录唯一临时文件 → `fsync` → `rename`；读者只看到完整对象 |
| 对象修复 | 已存在对象必须仍哈希到文件名，否则原子重写；截断对象被修复 |
| 私有权限 | 目录 `0700`、文件 `0600`（Unix）；Windows 保持用户 Profile ACL，未额外验证 |
| 符号链接 | 读取与写入都拒绝跟随；清扫把链接视为 unknown，不删除、不解引用 |
| 崩溃残留 | 只清理本 store 自己命名的 `.<hash>.<pid>.<nonce>.tmp`，且超过宽限期 |
| 清扫引用源 | 扫描 `session_messages`、`turns`（`/compact` 后仍保留）、`tool_call_images` 事件、含内联消息的事件（如 `context_snapshot`） |
| 引用解析失败 | 任一 payload 无法解析或 base64 无法解码 → **跳过整次清扫**，绝不把不可读引用当成孤儿 |
| 宽限 | 默认 24h；刚暂存、尚无持久引用的附件不会被误删 |
| 软配额 | 默认 2 GiB；只报告 `over_budget`，**绝不为满足配额删除仍被引用的对象** |
| 配置 | `[storage] media_max_bytes`、`media_grace_hours` |
| 运行时机 | daemon 在取得 workspace 所有权、开始服务之前执行一次；不阻塞客户端 |

未引用的旧对象会被删除；仍被历史引用的对象即使超预算也保留，超预算只记录警告，由用户删除
Session 或接受增长。这是“历史真实性优先”的明确取舍，不是隐藏的硬停止。

## 验证

### 单元测试（22/22 PASS）

`cargo test -p leveler-media --lib`：

- 引用对象保留 / 旧孤儿删除；
- 宽限期内不删除；
- 超预算不删除可达对象并报告 `over_budget`；
- 孤儿 `.mime` 伙伴清理；
- unknown 文件与符号链接不删除，链接目标不被读取；
- 自有临时文件清理；
- 幂等；
- 截断对象修复；
- 目录/文件权限；
- 8 线程并发导入只留下一个完整对象。

### 真实 daemon（`/tmp/r6-media-gc`，隔离 HOME）

1. 创建 3 个对象：被历史消息引用（3h 前）、无引用孤儿（3h 前）、无引用但刚写入。
2. `media_grace_hours=1`，启动真实 `leveler serve`。
3. 日志：

```
media store is over its soft budget; reachable objects were kept
live_bytes=73 remaining_bytes=146 removed_objects=1 removed_bytes=73 unknown_files=0 failures=0
```

4. 文件系统核对：引用对象与刚写入对象存在，旧孤儿消失；目录 `drwx------`、文件 `-rw-------`。
5. 注入一个损坏的 base64 图片引用后重启：日志
   `media sweep skipped: a durable payload could not be read unreadable=1`，孤儿仍在（fail-closed）。
6. 删除损坏行后重启：孤儿被删除，计数恢复。

## 未验证 / 剩余风险

- Windows：ACL、reparse point、文件共享模式、原子 rename 语义未在真机验证。
- 启动扫描会把所有含图片的 payload 读入内存并解码；大量图片的首次启动有一次性成本，未做大库基准。
- 没有持久化 `media_refs` 索引；每次启动重新派生。若未来删除 `turns`/事件历史，需要重新评估引用来源。
- `put_blob` 的非图片对象若没有持久引用，也会在宽限期后清理；当前调用方只有附件暂存路径。
- 软预算不能限制“被引用数据”的总量；这是产品约束，不是遗漏。
