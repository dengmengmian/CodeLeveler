# Windows 10 历史基线可用性

本文件记录本次 Windows 11 专项开始时对历史 Windows 10 证据的实际检索结果，避免把“未找到”写成“已验证”。

## 检索结果

| 检索项 | 结果 |
| --- | --- |
| `docs/windows-tui-acceptance/` | 不存在；`git log --all -- <path>` 无记录 |
| `docs/windows-tui-acceptance/evidence/` | 不存在 |
| `windows-audit/` | 不存在；`git log --all -- <path>` 无记录 |
| 全仓 `git log --all --name-only` 中匹配 `windows-tui` / `windows-audit` | 0 条 |
| 本机 `C:\` 根目录 | 无上述目录 |
| 历史 Windows 10 真机 | 本机不可用 |

因此，任务说明中列出的历史结论（Windows 10 TUI、Terminal、Console Host、PowerShell/CMD、中文缺字、Welcome 降级、路径展示、Assisted/FullAccess、OutputDecoder、sidecar 精确性、v1.0.26 候选等）只能作为**外部提供的基线描述**引用，不能在本次仓库内逐条复核原始证据。

## 对本次验收的影响

1. **历史 51 条失败无法逐条对账**：原始失败清单（Test Target / Test Name / 证据）不在仓库或本机。本次改为在当前 Windows 11 上运行完整 Workspace 套件，对**当前实际失败**建立逐条索引；历史 51 行对账标记 BLOCKED。
2. **Windows 10 当前候选回归未执行**：没有 Windows 10 环境，也没有远端 CI 运行授权。报告中 Windows 10 结论只保留“历史外部基线”，不外推为当前候选已通过。
3. **不覆盖历史报告**：本次新增 `docs/windows-qualification/win11/`，不创建或改写不存在的 Windows 10 页面；如果后续恢复历史目录，新页面可直接作为其子页面链接。

## 当前候选与历史候选的关系

| 项目 | 历史候选 | 当前候选 |
| --- | --- | --- |
| 版本 | 1.0.26 | 1.0.27 |
| 提交 | `5a4fc17480a6`（外部描述） | `644d765c6208` 基线 + 本次 4 个本地修复提交 |
| 证据 | 不在本机 | 本次 `evidence/` |
| 关系 | 旧候选，不能替代当前候选 | 唯一用于本次结论的候选 |
