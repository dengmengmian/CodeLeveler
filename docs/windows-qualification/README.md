# Windows 平台验收

本目录记录 CodeLeveler 在真实 Windows 主机上的兼容性、可靠性与安全验收，并区分历史候选、当前候选与外部历史基线。

| 专项 | 平台 | 候选 | 入口 |
| --- | --- | --- | --- |
| Windows Platform Final Closure | Windows 11 26300.9711 | 1.0.27（`7966c829` 代码 HEAD） | [Markdown](REPORT.md) · [HTML](index.html) · [环境](ENVIRONMENT.md) |
| Windows 11 Full Qualification（历史候选） | Windows 11 26300.9711 | 1.0.27（`4bc08296` 代码 HEAD） | [Markdown](REPORT-4bc08296.md) · [历史基线](HISTORICAL_BASELINE.md) |
| Windows 10 历史专项 | Windows 10 Education 19045 | 1.0.26（`5a4fc17480a6`，外部基线） | [HISTORICAL_BASELINE.md](HISTORICAL_BASELINE.md) |

当前候选最终判定：**PASS（已执行范围）**。Task Scheduler P1（非提升 XML 注册）已修复并以真实到期业务任务关闭；`schedule doctor` 的 Windows environment 误报已修复；全量 Windows 安全 canary、Release 构建、ZIP 冒烟与本轮产品级 E2E 通过。独立标准用户、ConHost、Release Lab/RC、远端 CI、PS7/VS Code/IME、macOS/Linux 与 Windows 10 当前候选仍为 BLOCKED / NOT VERIFIED。

## Windows 11 历史候选与当前候选

上一候选 `1.0.27 (4bc08296)` 的报告原文保留在 [REPORT-4bc08296.md](REPORT-4bc08296.md)；其证据目录（TUI 截图、Dogfood A/B/E、MCP、Edge、安全 canary、全量测试日志）继续保留在 `evidence/`。当前候选只修改 schedule wakeup/doctor，未改动 agent/TUI 代码，因此按影响范围复用上一候选证据，并在当前报告中逐项标注归属，不把旧证据自动视为新候选证据。

## Windows 10 历史专项

任务说明中引用的 `docs/windows-tui-acceptance/` 与 `windows-audit/` 在本仓库和本机均不存在（`git log --all` 无记录，历史 SHA 不是本仓对象）。历史版本（1.0.26 / 5a4fc174）的验收结论作为**外部基线**保留；原始截图/日志未在当前工作区恢复；当前候选（1.0.27 / 7966c829）未在 Windows 10 验证。检索与影响见 [HISTORICAL_BASELINE.md](HISTORICAL_BASELINE.md)。
