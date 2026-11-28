# 安全与凭据（R6-A5/A6）

日期：2026-11-28
结论：**macOS 未发现 P0；Windows 安全基线 NOT VERIFIED / BLOCKED**。

## 1. Provider 长期凭据

| 事实 | 证据 |
| --- | --- |
| 存储位置 | `~/.leveler/config.toml` 的 `[providers.*].api_key`，或 `api_key_env` |
| Unix 权限 | 共享 writer 以 `0600` 暂存 + `rename`；本机实测 `config.toml` 为 `-rw-------`，`~/.leveler` 为 `drwx------` |
| 原子性 | `config_edit` 加 `config.toml.lock` 排他锁，读‑改‑校验‑重读‑重试，最多 8 次 |
| 并发保留 | 无 key 的保存不会清空已存 key（provider/search 均有测试） |
| Desktop 读回 | 不读回；表单始终为空，只显示“已保存密钥”或 env 名 |
| Desktop IPC | 保存/探测时 key 以明文经过 renderer → main → bridge stdin；这是本机进程边界，不是加密通道；响应不含 key |
| localStorage | 不保存 provider/search key；草稿保存前经过 host `secret_scan` |
| 日志/错误 | provider Debug 打印 `[redacted]`；TOML 解析错误不回显原文；错误文本经 sanitizer |
| 崩溃报告 | panic message 现在也经过 `sanitize_model_visible` 再写盘 |
| Session export / Review | Desktop 无 export；review 请求不含配置或 key |
| OS keychain | 当前无 macOS Keychain / Windows Credential Manager / libsecret 集成 |

### 本轮加固

- `ProtocolContext` 的派生 `Debug` 改为手写：key 只打印 `[REDACTED]`，额外请求头只打印
  名称。此前一旦在错误路径格式化整个上下文就会泄漏 key/header value。
- `GlobalProvider` / `GlobalConfig` 的 `Debug` 不再输出明文 `api_key`。
- 崩溃记录中的 panic message 先做 secret 扫描/替换，再写盘。
- 媒体目录/文件改为 `0700`/`0600`，并拒绝符号链接对象。

### 未解决 / 阻断

| 项 | 状态 | 影响 |
| --- | --- | --- |
| Windows 文件 ACL | **NOT VERIFIED / BLOCKED** | `private_fs` 在 Windows 是 no-op，只依赖 `%USERPROFILE%` 继承；没有 DACL 断言、没有真机 ACL 证据。正式 Windows 发布前必须验证/实现 |
| 长期密钥仍为明文文件 | **风险接受（当前产品契约）** | 明确不是 SecretVault；不自动迁移，不擅自删除用户凭据。迁移到 OS keychain 需要兼容 CLI/TUI/env 引用的设计与用户批准 |
| IPC 明文 | 设计边界 | 同用户本机进程；不声称加密。任何能读同用户进程内存/管道的本机攻击者不在当前威胁模型内 |
| 自动更新信任链 | **BLOCKED** | Desktop 无更新器；CLI 更新器只覆盖 CLI 产物。安全更新/回滚门禁未建立 |

## 2. 媒体与路径

- 媒体 ID 必须是 64 位小写 hex，路径遍历在触盘前拒绝。
- 读取/写入要求 regular file；符号链接、目录、设备文件一律拒绝或视为 unknown。
- 清扫不跟随链接，不删除 unknown 文件；损坏引用 fail-closed。
- 临时写入使用同目录唯一名 + `fsync` + rename，崩溃不会暴露半个对象。
- Windows reparse point / junction 行为未验证，不能把 macOS 的 `O_NOFOLLOW`/symlink 测试
  当作 Windows PASS。

## 3. Desktop IPC / Bridge

复核范围：`apps/leveler-desktop/src/security.cjs` allow-list 与逐命令 `exactKeys`、
`main.cjs` 的 sender/路径校验、`desktop_bridge.rs` 的会话绑定与参数边界。

- Renderer 只能调用 allow-list 中的方法；未知方法由 bridge 拒绝。
- 文件预览/打开路径经 realpath + registered root 校验，不把 renderer 字符串直接当路径。
- 附件/session 查询绑定 session id，不能跨 Session 读取。
- 浏览器/本地进程仍走 Runtime/宿主唯一路径，Desktop 不另建执行权威。
- 本轮没有发现新的 P0；未对每个 R1–R5 能力做逐条渗透复测，Windows 侧仍未验证。

## 4. 发布阻断（安全）

- Windows ACL / reparse point / 路径规范化没有真机证据：Windows 正式发布 **NO-GO**。
- macOS Developer ID 签名与公证没有凭据：正式分发 **BLOCKED**；本轮只有未签名本地 RC。
- 自动更新未实现安全信任链：正式自动更新 **BLOCKED**。
