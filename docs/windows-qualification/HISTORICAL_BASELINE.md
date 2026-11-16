# Windows 10 历史基线与当前候选关系

本文件区分四件不同的事实，避免把“历史版本曾通过”写成“当前候选已通过”，也避免把“原始证据不在本机”写成“没有历史验收”。

## 1. 外部提供的 Windows 10 历史验收结论

以下结论来自本轮任务说明所引用的 Windows 10 真机专项，作为**外部历史基线**保留：

| 项目 | 值 |
| --- | --- |
| 操作系统 | Windows 10 Education 19045 |
| 历史候选 | `1.0.26 (5a4fc17480a6)` |
| 代码提交 | `5a4fc17480a63e14b29fa785778fcde92ca3b940` |
| 文档 HEAD | `4e984c3993dd1d3736ee0f1608a0b03caf8e9254` |
| 历史报告目录 | `docs/windows-tui-acceptance/` |
| 已验证范围 | TUI、Windows Terminal、Console Host、PowerShell/CMD、权限契约、编码修复、最终候选真实 Goal Dogfood |

这些是历史版本的验收结论，不是当前 `1.0.27 (7966c829)` 候选的证据。

## 2. 原始历史证据的恢复结果

本轮检索了以下位置：

| 检索项 | 结果 |
| --- | --- |
| `docs/windows-tui-acceptance/` | 不存在；`git log --all -- <path>` 无记录 |
| `windows-audit/` | 不存在；`git log --all --name-only` 无匹配 |
| 历史提交 `5a4fc174...` / `4e984c39...` | 本仓不是有效 Git 对象（`git cat-file` 失败） |
| `git fsck --unreachable` | 无可用 dangling commit |
| 本机 `C:\qlab`、用户目录、Temp、归档目录 | 无 Windows 10 报告/截图/日志 |
| 用户 `.claude` 会话记录 | 只有任务说明中的历史引用，没有原始报告或截图 |
| 外部 Release Lab / dogfood 根 | 未配置；`DOGFOOD_ROOT_NOT_FOUND` |

结论：**原始历史证据未在当前工作区恢复**。本轮没有编造、重建或改写不存在的 Windows 10 页面；历史结论只作为外部基线引用。

## 3. 当前候选在 Windows 10 上的状态

- Windows 10 当前候选：**NOT VERIFIED**。
- 原因：本机无 Windows 10 宿主，未运行跨机器验收，也未授权远端 CI。
- 不把 Windows 11 的结果外推到 Windows 10。

## 4. 历史 SHA 与当前代码的行为核对

`b1f002d` 与 `5a4fc174...` 在当前仓库不可解析，但对应行为已存在于当前代码，可能经过 squash/cherry-pick/重构：

| 历史修复 | 当前代码位置 / 证据 |
| --- | --- |
| Display Path | `crates/leveler-core/src/path.rs::display_path`，含 verbatim/UNC/非 verbatim 测试 |
| OutputDecoder | `crates/leveler-execution/src/encoding.rs`，UTF-8 跨读、GBK lead/trail、finish 截断测试 |
| 中文宽度/边框 | TUI 使用 `unicode-width`；上一候选真机 TUI 证据 |
| Welcome 图片与无字符画降级 | TUI splash/welcome image；上一候选真机截图 |
| Alt+V | TUI Footer/粘贴路径；上一候选真机截图 |
| 原始/解码 sidecar | `leveler-execution` 输出 sidecar；workspace 测试 |
| 权限拒绝实时 stderr | TUI 权限/执行拒绝路径；上一候选 canary 与 nolauncher 证据 |

当前 `1.0.27 (7966c829)` 只修改 schedule wakeup/doctor；上述行为未改动，按影响范围复用上一候选证据，并在本轮报告中明确标注。

## 5. 与当前候选的关系

| 项目 | Windows 10 历史候选 | Windows 11 当前候选 |
| --- | --- | --- |
| 版本 | 1.0.26 | 1.0.27 |
| 提交 | `5a4fc17480a6`（外部） | `7966c829` |
| 当前工作区证据 | 原始证据未恢复 | `evidence/`、`evidence/final-7966c829/` |
| 可用于当前发布判定 | 否 | 是（仅限已执行范围） |
