# Windows 平台验收

本目录记录 CodeLeveler 在真实 Windows 主机上的兼容性、可靠性与安全验收。

| 专项 | 平台 | 候选 | 入口 |
| --- | --- | --- | --- |
| Windows 11 Full Qualification | Windows 11 26300.9711 | 1.0.27（`4bc08296` 代码 HEAD） | [Markdown](REPORT.md) · [HTML](index.html) · [环境](ENVIRONMENT.md) |

最终判定：**PARTIAL PASS（已执行范围）**。全量 7930 项 workspace 测试、Clippy、Edge acceptance、真实 Dogfood 与安全 canary 通过；Task Scheduler 非提升 XML 注册失败（L6，未修复）；独立标准用户、ConHost、Release Lab/RC、远端 CI、PS7/VS Code/IME、macOS/Linux 与 Windows 10 当前候选为 BLOCKED / NOT VERIFIED。未执行 push/tag/release 或远端写操作。

## Windows 10 历史专项

任务说明中引用的 `docs/windows-tui-acceptance/` 与 `windows-audit/` 在本仓库和本机均不存在（`git log --all` 无记录）。本次没有创建、改写或伪造 Windows 10 历史页面；历史结论仅作为外部基线引用。检索与影响见 [HISTORICAL_BASELINE.md](HISTORICAL_BASELINE.md)。

如果后续恢复 Windows 10 历史报告，可直接在 `windows-qualification/` 下增加对应子目录，并从本页链接。
