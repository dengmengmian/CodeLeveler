# R6 发布门禁

日期：2026-11-28
结论：**NO-GO**。没有任何已确认的 P0；阻断项是正式平台/签名/更新/验收证据缺口。

## 1. 门禁表

| 类别 | 必须核对 | 状态 | 证据/原因 |
| --- | --- | --- | --- |
| 功能 | R1–R6-0 核心能力 | PASS（macOS 开发构建） | R1–R6-0 报告；本轮回归 |
| macOS | 真实安装产物 Dogfood | 见 `installed-product-dogfood.md` | 本轮构建/安装/真实 Provider |
| Windows | 真实安装产物 Dogfood | **BLOCKED** | 无 Windows 真机；无 Desktop 安装器；无 Windows Desktop CI |
| 安全 | 凭据/路径/IPC/Artifact/Memory | PARTIAL | macOS 未发现 P0；Windows ACL/reparse **NOT VERIFIED** |
| 可靠性 | 长任务/Steer/Goal/恢复 | PASS（A1）/PARTIAL（A2） | A1 59s 真实 PASS；A2 预算 UI/软阈值缺失 |
| 持久化 | Session/历史/图片/记忆 | PASS（macOS）/NOT VERIFIED（Windows） | 媒体清扫真机；Memory 22/22；历史读取 325 tasks |
| 兼容性 | 上一正式版升级 | **BLOCKED** | 上一正式版无 Desktop；RC 未发布，正式升级无法执行 |
| 安装 | 安装/卸载/版本身份 | PARTIAL（macOS 未签名）/BLOCKED（Windows） | 本轮 macOS RC；签名缺失 |
| 更新 | 安全更新/失败回滚 | **BLOCKED** | Desktop 无更新器；CLI 更新器不覆盖 Desktop |
| 签名 | macOS/Windows 签名 | **BLOCKED** | 本机 0 个 codesigning identity；无 Windows 证书 |
| 性能 | 启动/内存/长会话/资源 | NOT VERIFIED（无 v1.0.29 Desktop 对照） | 只有原始测量，无正式对比 |
| 自动化 | Rust/Desktop/E2E | PASS（附 1 flake） | Rust 9931/0/114；Desktop 282/0；clippy/fmt/all-features check PASS；1 次全量负载 flake，重跑绿 |
| 交付 | 安装包/哈希/报告/证据 | PARTIAL | macOS 未签名 RC + manifest；Windows 无产物 |

## 2. 阻断分级

### P0（未发现）

本轮审查与实测未发现数据丢失、凭据泄漏、权限绕过、任意路径读写或不可恢复更新损坏。
“未发现”不等于“证明不存在”；Windows 侧没有证据。

### P1（必须关闭才能 GO）

1. **Windows 正式支持**：无真机、无 Desktop 安装器、无 Windows Desktop CI/验收。
2. **macOS 正式分发**：无 Developer ID 签名、无公证/Staple；当前产物未签名。
3. **更新与回滚**：Desktop 无自动更新/签名校验/失败回滚；CLI 更新器不覆盖 Desktop。
4. **上一正式版升级**：没有 Desktop 上一版本；RC 未发布，无法走正式升级机制。
5. **Memory A4 矩阵**：非空 `/flush`、并发双写、35 分钟边界未全部实测。
6. **Goal 资源治理产品面**：无 Desktop 预算编辑器、软阈值提醒、goal 级 usage 查询。

### P2（不阻断本轮 RC 结论，但不得宣传为已完成）

- 媒体 store 启动扫描缺少大库基准；无持久化 media refs 索引。
- Desktop 不显示 cost；`query_usage` 不在 allow-list。
- Memory 管理结果语言为英文；页面键盘操作不完整。
- 崩溃/恢复的破坏性分支未在真实崩溃数据上演练。

## 3. 24 小时 Soak

未执行。按发版原则，Soak 只有在其他门禁全部通过时才可作为“有条件 GO”的唯一剩余项；
本轮 Windows/签名/更新/升级均未通过，因此不能引用该例外。
