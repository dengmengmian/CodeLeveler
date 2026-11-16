# Windows Platform Final Closure evidence index

Candidate code commit: `7966c829697356afaef0881755894fff5bd78d59` (schedule wakeup + doctor).
Candidate build revision: `03def7aaeecb07632a9ab74c47862eb6310799c2` (code commit + the scheduler acceptance evidence; no source delta).
Version string: `1.0.27 (03def7aaeecb)`.

## final-7966c829/

| 文件 | 内容 |
| --- | --- |
| `candidate.sha256` | leveler.exe / leveler-confine.exe / ZIP / README / LICENSE SHA-256 |
| `version.txt` | 从正式 ZIP 解压的 `leveler.exe --version` |
| `release-build.log` | 干净工作树的 release 构建 |
| `package.log` | ZIP 组装、解压、`--version`、completions、`leveler-confine` 冒烟 |
| `task-scheduler-root-cause.log` | 修复前后最小对照（无 LogonTrigger / 无 UserId / 有 UserId / 简单 ONLOGON） |
| `task-scheduler-acceptance.json` | `LEVELER_TASK_SCHEDULER_ACCEPTANCE=1` 真机验收（注册、执行、IgnoreNew、自然触发、删除） |
| `product-e2e-final.json` | 最终 Release 归档二进制的真实 OS 到期业务任务、身份/IL/cwd/输出/退出码 |
| `registration.xml` | 最终候选 Task Scheduler 记录，LogonTrigger 绑定当前用户 |
| `security-canary-manual.log` | 11 项独立安全 canary（Low IL、写根、Junction/.. /\\?\ /ACL Deny、大小写） |
| `security-canaries-ci.log` | CI 同款 `windows_` 测试（76 passed：3 tools + 73 execution） |
| `browser-acceptance.log` | 当前候选 Edge acceptance：12 passed / 0 failed |
| `workspace-tests.log` | 全量 workspace 测试（PATH 含文档化便携 CPython 3.12.10；结果见报告） |
| `workspace-tests-python-missing.log` | 首次全量运行：4 个 `leveler-web --lib` fixture 因主机无 `python3` 而失败（Store 别名），其余 target PASS；属环境缺失 |
| `workspace-web-lib-with-python.log` | 用便携 CPython 复跑 `leveler-web --lib`：39 passed / 0 failed，证明上述 4 个 fixture 通过 |
| `clippy.log` / `check.log` / `fmt.log` | Clippy / check / format |
| `dev-verify.log` / `dev-pre-release.log` | 项目入口检查与 Release Lab 阻断 |
| `debug-candidate/` | 修复工作树的 debug 构建功能 E2E（生命周期、并发、daemon 重启）；只证明行为，不冒充出货字节 |

## 历史证据

- `evidence/task-scheduler-final/`：修复后 debug 构建的 Task Scheduler acceptance（1.0.27 / 7966c829 工作树）。
- `evidence/` 其余文件：上一候选 `4bc08296` 的 TUI、Dogfood、MCP、Edge、安全 canary、全量测试；按影响范围复用并在报告中标注。
- `REPORT-4bc08296.md`：上一候选报告原文。
