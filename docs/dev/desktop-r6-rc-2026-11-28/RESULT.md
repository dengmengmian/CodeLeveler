# R6 最终报告

日期：2026-11-28
候选源码：`8bde10ca66e6`（干净工作树）
上一正式版：`v1.0.29`（`f972f9f6`，CLI-only）

## 1. 总体结论

**NO-GO。**

- macOS：可构建、可安装、真实 Provider/多文件/测试/Steer/Memory/重启核心闭环 PASS。
- Windows：**BLOCKED**（无真机、无 Desktop 安装器、无 Windows Desktop CI）。
- macOS 正式签名/公证：**BLOCKED**（0 identities；只有 ad-hoc 本地签名）。
- Desktop 自动更新/回滚：**BLOCKED**（未实现）。
- 上一正式版升级：**BLOCKED**（上一正式版无 Desktop；RC 未发布）。
- 未发现 P0；P1 见 `remaining-issues.md`。

本轮交付的是一个可核对证据的 macOS **本地未签名 RC**，不是双平台正式版。

## 2. 版本与源码身份

| 项 | 值 |
| --- | --- |
| Cargo 版本 | 1.0.29（未改版本号） |
| Runtime build | `1.0.29 (8bde10ca66e6)`，dirty=false |
| 安装产物 | `CodeLeveler-1.0.29-macOS-arm64.{zip,dmg}` |
| 产物源码 HEAD | `8bde10ca66e6f8c134d6728ce77ec2b3737d192f`，dirty=0 |
| 测试修复 | 另有一个 test-only 提交（`real_session_corpus` 调用新签名），不改变 Runtime 行为 |
| 上一正式 tag | `v1.0.29` = `f972f9f613db` |
| 距上一 tag | 96 个提交 + 本轮 4 个提交 |

## 3. R6-A 可靠性结果

| 专项 | 结论 |
| --- | --- |
| A1 长任务 + Steer | **PASS**（真实 Provider，59s；见 `reliability.md`） |
| A2 Goal 资源治理 | **PARTIAL**（全局 `[limits]`、预算终态文案；无 Desktop 预算 UI/软提醒） |
| A3 图片媒体配额/清扫 | **PASS（macOS）**；Windows NOT VERIFIED |
| A4 Memory | **核心 PASS**（22/22 集成 + R6-0 真机）；A4 全矩阵 PARTIAL |
| A5 凭据安全 | macOS 未发现 P0；Windows ACL **NOT VERIFIED** |
| A6 安全边界 | 未发现新 P0；未做逐项渗透复测；Windows 路径/重解析点未验证 |

## 4. Steer 调查

- 当前构建真实复跑：steer journal `consumed`、ordinal 3、后续 11 次模型调用、
  turn_count=1、最终同答含 5050 与 marker；59s 完成。
- 第一版脚本未设 `busySend=steer`，把文字排成第二个回合却“表面 PASS”；
  已用 `session_inputs` + `turn_count` 抓住并修正。详见 `reliability.md`。
- R5 原始 Session 已删除，240s INCOMPLETE 的根因 **UNKNOWN**；本轮不把它改写成已修复，
  也不判定为产品 BUG。

## 5. Goal 资源治理

- 已有：`model_requests` 结构化用量、`ProgressLedger`、项目级 token/cost/duration cap、
  重启复用、无进展守卫。
- 新增：全局 `[limits]` 回退（项目优先）、预算停止维度/金额文案（live 用结构化值，
  replay 解析运行时机器 detail）。
- 缺口：Desktop 无预算编辑器/软阈值提醒/goal 级 usage 查询；`TaskFinished` 未持久化
  结构化 budget；Desktop 不显示 cost。见 `goal-resource-governance.md`。

## 6. 图片媒体生命周期

真实 daemon 验证（隔离 HOME）：

- 被历史引用的旧图片保留；
- 无引用旧孤儿删除；
- 宽限期内新对象保留；
- 损坏引用使清扫 fail-closed；修复后恢复；
- 目录/文件权限 `0700/0600`；
- 软预算超限只警告，不删除可达对象。

22 个媒体单测 + 5 个引用扫描/清扫测试 PASS。详见 `media-storage.md`。

## 7. Memory 并发与实际 Dream/Flush

- 22/22 `memory_integration` PASS：非空手动 Dream、capture 重试、重启续跑、崩溃恢复、
  fork/parallel、pause、临时 workspace、lease 释放等。
- R6-0 真机 16 项 PASS；本轮安装版 `/remember` + `/memory` + 重启 PASS。
- 未实测：非空 `/flush` 独占队列、Desktop+TUI 双写压力、并发 `/remember`/Dream、
  35 分钟边界、真实遗忘竞争。详见 `memory-reliability.md`。

## 8. 安全与凭据

- `config.toml` 在 macOS 为 `0600`、home 为 `0700`；共享 writer 原子写 + 锁 + 保留 key。
- Desktop 不读回 key；保存/探测时明文经本机 IPC；无 OS keychain。
- 本轮修复：`ProtocolContext`/`GlobalConfig` Debug 脱敏、panic 消息脱敏、媒体文件私有化。
- Windows ACL/reparse point 无真机证据；正式 Windows 发布 NO-GO。详见 `security.md`。

## 9. macOS 正式安装版

- `.app` + `.zip` + `.dmg` 已构建；bundle 内含匹配 Runtime，SHA 校验一致。
- 安装到 `~/Applications/CodeLeveler R6 RC.app`；真实进程命令确认使用 bundle 内 Runtime。
- 未签名/未公证；只有 ad-hoc 本地签名，`spctl` 拒绝；**不是正式分发产物**。

## 10. Windows 正式安装版

**BLOCKED**。没有 Windows 主机、VM、Wine；CI 只构建 CLI；没有 Desktop Windows job 或安装器。
完整矩阵见 `windows-validation.md`，全部 NOT VERIFIED。

## 11. Windows 特有安全与兼容

全部未验证：盘符/UNC/长路径/中文路径/大小写/reparse point/junction/文件占用/原子替换/
SQLite WAL/ACL/PowerShell 子进程/MCP stdio/剪贴板图片/输入法/拖放/代理/防火墙/崩溃恢复。
不能把 macOS 的 symlink 测试当作 Windows PASS。

## 12. 上一正式版升级

- 上一正式版 `v1.0.29` 无 Desktop 产物，所以没有“上一 Desktop → 本 RC”的升级。
- CLI 旧数据兼容有非破坏性证据（真实 HOME 325 tasks、0 source errors；配置/历史可读）。
- 正式自动升级无法执行：RC 未发布到更新源。**BLOCKED**。详见 `upgrade-migration.md`。

## 13. 更新、签名、公证

| 项 | 状态 |
| --- | --- |
| macOS Developer ID | **BLOCKED**（0 identities） |
| 公证/Staple | **BLOCKED** |
| Windows 代码签名 | **BLOCKED**（无证书/无产物） |
| Desktop 自动更新/回滚 | **BLOCKED**（未实现） |
| CLI 更新器 | 存在，但不覆盖 Desktop |

## 14. 两平台真实 Dogfood

- macOS 安装版：运行中 Steer、多文件实现、独立 `node --test` 2 passed、`/remember` +
  `/memory`、重启恢复 4 条消息、config 恢复；全部 PASS。详见 `installed-product-dogfood.md`。
- Windows：**BLOCKED**。
- 安装版未逐项重跑 MCP/Skills/Review/Plan/Goal/图片/Diff/搜索/定时任务；不得宣称完整 D2。

## 15. 自动化与性能

- Desktop：282/282 unit、typecheck、lint、build PASS。
- Rust workspace：**9931 passed / 0 failed / 114 ignored（374 个测试二进制，EXIT 0）**。
  首轮全量在 `background_crash_matrix::b9_...` 出现 1 次失败（仅在全量并行负载下）；该测试单独 10/10、整套 5/5 PASS，完整重跑 EXIT 0。归类为 **FLAKY_UNRESOLVED（负载下）**，不是已证实的投放缺陷，也没有被隐藏。
- Memory integration：22/22 PASS。
- Media unit：22/22；media_gc：5/5；storage/media ref query：PASS。
- A1 真实任务 59s；安装版启动 ~1.48s；CLI `--version` 中位数 38.5ms；
  安装版空载全部进程 RSS ~425MB。
- 无 v1.0.29 Desktop 可比基线，不能宣称性能提升/回退。详见 `performance.md`。

## 16. 上一正式版与当前版对比

当前 RC 的增量是 Desktop 产品面（R1–R6-0）从源码运行变成可安装产物，加上媒体生命周期、
预算终态文案、凭据 Debug/崩溃脱敏。Agent 核心控制语义未重写。详见 `version-comparison.md`。

## 17. HTML 与历史

`audit.html` 为独立静态页：门禁、证据、产物 SHA-256、历史版本索引（R6 RC、v1.0.29、v1.0.28）。
未修改 `docs/releases/index.html` 的正式发布列表，避免把未发布 RC 伪装成正式版本。

## 18. 产物与 SHA-256

| 产物 | SHA-256 |
| --- | --- |
| zip | `30d2bee222fbbaaf20b160ba9ad445ed99e5afa6024085cc72369d1f5b8e45c1` |
| dmg | `f1586dc15d0312d6e9b72e3948bd32a2954363d84db9e13df0f79421aa5647e7` |
| Runtime | `16f97ce34bcff53d232abaa45e3bdaa9d48f236f7fb74709bf6bfa49e730d97d` |
| Bundle CDHash | `ce01160d627c264a307f671acc948dc012c1efa9`（adhoc） |

最终产物由 `8bde10ca` 的干净工作树构建；完整清单见 `artifacts-manifest.json`。

## 19. P0 / P1 / P2

- P0：未发现。
- P1：Windows 真机/安装器；macOS 签名/公证；更新/回滚；上一正式版升级；
  Memory A4 全矩阵；Goal 预算产品面；24h Soak。
- P2：媒体启动扫描基准、cost 显示、Memory 文案/键盘、崩溃窗口演练、Windows 输入法/拖放等。

## 20. 本地提交

| 提交 | 内容 |
| --- | --- |
| `a220f888` | `feat(runtime): bound media storage and surface budget stops` |
| `430941ab` | `feat(desktop): bundle the matching runtime and package a macOS RC` |
| `1537abbb` | `style(app): format the top-level limit merge` |
| `ce74f201` | `fix(desktop): make the macOS RC bundle reproducible` |
| `8bde10ca` | `test(cli): pass the budget slot to the terminal projection` |


未 push、未 tag、未 release。

## 21. Git / Tag / Release 状态

- 分支：`main`（本地提交，未 push）。
- tag：无新增；最新仍是 `v1.0.29`。
- GitHub Release：未创建。
- `target/desktop-rc/` 与 `~/Applications/CodeLeveler R6 RC.app` 是本地测试产物，不是发布。

## 22. 最终发布建议与六问

**建议：NO-GO。** 继续投入的顺序应是：Windows 真机与安装器 → macOS 签名/公证 →
Desktop 更新/回滚 → 上一正式版升级 → Memory/A2 全矩阵 → 24h Soak。

1. **是否已在 macOS 和 Windows 两平台完成真实安装与真实 Agent 执行验收？**
   **没有。** macOS 安装版核心闭环 PASS；Windows 完全 BLOCKED。
2. **长任务、Steer、Goal、Memory、图片和历史持久化是否具备正式产品可靠性？**
   **部分具备。** Steer/媒体/历史有真实 PASS；Memory A4 与 Goal 预算产品面仍有缺口；
   Windows 未验证。因此不能回答“具备正式产品所需的全部可靠性”。
3. **从上一正式版升级，配置/密钥/Session/记忆/业务数据能否安全保留？**
   **没有 Desktop 上一正式版可升级；正式升级流程 BLOCKED。** CLI 旧数据兼容有非破坏性
   证据，但不等于正式升级 PASS。
4. **正式分发产物是否完成安全、签名、公证、更新及回滚验证？**
   **没有。** 仅有 ad-hoc 本地未公证 RC；更新/回滚未实现。
5. **与上一正式版相比提升了什么、剩余风险？**
   新增 Desktop 可安装产品面、媒体生命周期、预算终态文案、凭据 Debug/崩溃脱敏；
   风险是 Windows、签名、更新、A4/A2 证据缺口与安装版覆盖不全。
6. **当前是否满足正式发布门禁？**
   **不满足。** 精确阻断项见 `remaining-issues.md` 的 P1 表与 `release-gates.md`。
