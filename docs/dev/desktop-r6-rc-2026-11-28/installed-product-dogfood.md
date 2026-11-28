# 安装产物真实 Dogfood（R6-D）

日期：2026-11-28
结论：**macOS 安装版 PASS（真实 Provider 的核心闭环）**；Windows **BLOCKED**。
本文件不宣称完整 D2 22 步矩阵全部重跑。

## 1. 被测产物

- `~/Applications/CodeLeveler R6 RC.app`（由 `target/desktop-rc/CodeLeveler.app` 用 `ditto` 安装）；
- Runtime 进程实际命令：
  `/Users/.../CodeLeveler R6 RC.app/Contents/Resources/runtime/leveler ... serve ...`
  → 证明不是 `target/debug/leveler`，也不是 PATH 里的 Homebrew 旧版；
- Runtime build：`1.0.29 (199334406950)`，`dirty=false`；
- 真实 HOME/config/provider/Runtime/Desktop/文件系统；临时 workspace、Electron profile；
- 测试 Session `db9da08c-...` 已按 id 删除；临时 Memory note `observations/_inbox/remember-nEyZyE.md`
  已用 `memory forget --hash` 清理；配置 SHA 前后一致。

## 2. 结果

| # | 场景 | 结果 | 证据 |
| --- | --- | --- | --- |
| 1 | 安装并启动 | PASS | App 版本 1.0.29；启动到 UI 约 1.47s |
| 2 | Runtime 身份 | PASS | `runtime_info.pid` 的 `ps` 命令位于 app bundle 内 |
| 3 | 真实 Provider/Model | PASS | `deepseek/deepseek-v4.1-flash-1017` |
| 4 | 多文件任务 | PASS | 创建 `hello.txt`、修改 `sum.mjs`、新增 `sum.test.mjs` |
| 5 | 测试 | PASS | 独立 `node --test sum.test.mjs` → 2 passed / 0 failed，断言 5050 |
| 6 | 运行中 Steer | PASS | `session_inputs` state=`consumed`，ordinal 存在，**turn_count=1** |
| 7 | 最终交付 | PASS | 同一回答含 5050 与 `INSTALLED_STEER_MARK` |
| 8 | `/remember` + `/memory` | PASS | 全局 note 写入；页面按文件行选中后预览出 token |
| 9 | 重启 | PASS | 同一 Session 恢复 7 条消息，状态 `completed`，版本仍是 `v1.0.29 · 桌面` |
| 10 | 清理/配置 | PASS | Session 删除、note 遗忘、config SHA 一致 |

过程中模型一次把 `commit_task` 的 content 要求写成非字面行，被 Runtime 拒绝后自行用
`declare_scope`/代码调整修正；最终完成。这不是产品缺陷，结构上说明验证门在工作。

## 3. 未覆盖（必须明说）

- 未在本安装产物上逐项重跑：MCP、Skills、Review、Plan、Goal、图片/工具图片/Diff/搜索、
  定时任务、崩溃恢复、升级/回滚、长会话/重启后的图片与 Artifact。
- 这些能力有 R1–R6-0 的开发构建证据，但“安装产物”证据只覆盖上表 10 项。
- Windows 安装产物/真机：**BLOCKED**。
- 未签名/未公证：Gatekeeper `spctl` 结果为 REJECTED（ad-hoc 本地签名），
  不能作为正式分发验收。

完整证据：`evidence/r6-installed-dogfood-evidence.json`、`evidence/r6-installed-dogfood.png`
（若有截图）。
