# Windows 真机验收（R6-B）

日期：2026-11-28
结论：**BLOCKED — 没有 Windows 真机/桌面会话，未执行任何 Windows GUI 验收。**

## 1. 环境事实

| 项 | 值 |
| --- | --- |
| 当前主机 | macOS 26.6.2 / Darwin 25.6.0 / arm64 |
| Windows 真机 | **无** |
| Windows VM / 远程桌面 | **无** |
| Wine / CrossOver | **未安装**（`which wine` 无结果） |
| 可用的 Windows CI | 只有 `release.yml` / `ci.yml` 的 Rust `x86_64-pc-windows-msvc` CLI 构建与 cargo test |
| Desktop Windows job | **不存在**（`ci.yml` 只有 macOS smoke 与 Linux 静态/单测） |

因此不能把交叉编译、CI CLI 构建或 Rust 单测当作 Windows Desktop PASS。

## 2. 功能矩阵

全部 **NOT VERIFIED**：

| 功能 | 状态 |
| --- | --- |
| 首次启动/安装 | NOT VERIFIED |
| 已有用户配置/历史 | NOT VERIFIED |
| Provider 设置与真实模型调用 | NOT VERIFIED |
| Chat / 流式 / 工具 | NOT VERIFIED |
| Plan / Goal / Steer | NOT VERIFIED |
| MCP（stdio/HTTP） | NOT VERIFIED |
| Skills / Definition Trust | NOT VERIFIED |
| 手动 Review | NOT VERIFIED |
| Memory 四命令与作用域 | NOT VERIFIED |
| 搜索 | NOT VERIFIED |
| 文件/新建/修改/删除/重命名 | NOT VERIFIED |
| Diff（含二进制/路径） | NOT VERIFIED |
| 图片输入/工具图片/历史/预览 | NOT VERIFIED |
| 斜杠命令 | NOT VERIFIED |
| 设置保存/重载/恢复 | NOT VERIFIED |
| 权限（标准用户/授权/拒绝） | NOT VERIFIED |
| 旧 Session 与兼容来源 | NOT VERIFIED |
| 退出与子进程生命周期 | NOT VERIFIED |
| 重启持久化 | NOT VERIFIED |
| Task Scheduler 登录触发/唤醒 | NOT VERIFIED |

## 3. Windows 特有风险（全部未实测）

路径分隔符、盘符、UNC、长路径、中文/空格工作区、大小写、reparse point/junction、
文件占用与共享模式、原子替换、SQLite WAL、文件 ACL、PowerShell/CMD 子进程、环境变量继承、
MCP stdio、Electron 剪贴板图片、Windows 输入法、拖放、系统代理、防火墙、崩溃恢复。

特别地：

- `private_fs` 在 Windows 是 no-op，配置/数据库只依赖用户 Profile ACL；没有 DACL 断言。
- 媒体 store 的符号链接拒绝是 Unix `symlink_metadata` 语义；Windows reparse point 未验证。
- 没有 Windows 版 Desktop 安装器（NSIS/MSIX 均未建立）。
- 没有在非提升普通用户下运行过任何 Desktop 流程。

## 4. 解除阻断的最低条件

1. 一台真实 Windows 11 x64 测试机（普通用户、可交互桌面、非 SYSTEM/管理员常驻）。
2. 构建 Windows Desktop 安装包（与 macOS 同一打包体系），并记录产物 SHA-256。
3. 在标准用户下跑完上表，保存结构化时间线、截图、历史/配置/Session 前后哈希。
4. 单独验证 reparse point、ACL、长路径、中文路径、文件占用与原子替换。
5. 如启用 Task Scheduler，验证 LogonTrigger + 当前用户、无保存密码、无 ACL 绕过。

在上述证据存在前，Windows 正式发布结论只能是 **NO-GO**。
