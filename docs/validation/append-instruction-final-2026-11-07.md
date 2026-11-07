# 运行中追加指令最终工程收尾报告 · 2026-11-07

## A. Final Verdict

| 维度 | 判定 | 依据 |
| --- | --- | --- |
| 功能验收（包含真实产品闭环） | **FAIL** | 可靠交付场景通过；Access 管理员管理要求送达后没有实现，独立 HTTP 确认 `/admin` 为 404。 |
| 工程集成 | **BLOCKED** | 共享改动无法按文件安全拆分；foreign-session 的 Clippy 和 HOME 规则门禁未通过，未强行形成提交。 |
| 本轮原始证据完整性 | **PASS** | 18 次真实运行可读，698 项逐文件 SHA-256 一致，18 份 SQLite 可打开；包含失败原始证据。历史 Provider/PTY 丢失没有被伪造恢复。 |
| 发布质量门禁 | **BLOCKED** | Workspace Clippy 和完整测试存在真实失败；当前源码 Linux/Windows 实测 UNKNOWN。Release 构建通过不等于发布资格。 |

本轮不能声明“运行中追加指令功能已完成最终工程收尾”。没有进入下一阶段、部署、安装、Push、Tag 或 Release。

## B. 修复及功能保留

从已有聊天“追加任务处理机制”的附件找回 Phase 2 报告文本，来源、SHA 和副本见归档 `recovered/attachment-provenance.json`、`phase2-report.txt`。这些材料只恢复缺陷定义与历史叙述，不作为历史端到端 PASS 的独立证据。

| 历史缺陷 | 当前实现与本轮证据 | 状态 |
| --- | --- | --- |
| D1 取消后复活 | 取消结算 Journal；sc8 后续全部 Provider 请求不含取消文本、transcript 0 行；case5c。 | 本轮通过 |
| D2 ACK 早于持久化、取出到写入空窗 | admission 先落库；transcript 与消费状态原子持久化；sc2 三轮和 case6a 真实崩溃恢复。 | 本轮通过 |
| D3 回合结束视觉重复 | `input_id` 贯通事件、transcript、Resume；sc1 双端帧、core case5e、reducer 回归。 | 本轮通过 |
| D5 多客户端/Resume 不可见 | 广播、snapshot 读 Journal；sc1、sc2、双 TUI 崩溃组合。 | 本轮通过 |
| D6 多条追加换行合并 | 一条 seed 加逐条独立输入；sc7 三轮各三条独立 `role=user`、独立 ID/ordinal；case5e。 | 本轮通过 |
| D5a 历史重放擦掉新行 | 历史替换保留 Journal-only 行；相关 TUI 历史测试及运行中重连证据。 | 本轮通过（最终全量中的两项历史回放测试为 ok） |
| D5b 历史重放换 ID | `session_messages.input_id` 回放沿用身份；App Journal 历史读取、sc2、case6a。 | 本轮通过 |

普通 Enter 在空闲时开新任务、运行中追加；Tab 显式排到当前完成后。未增加关键词分类、强制中断、模型理解/执行成功状态或新调度机制。保留既有投递状态和“※ 回顾”；sc5 最终 PTY 实际含回顾。

本轮真实复现并修复了两个相连的投影/顺序问题：

1. 已有 Delivered/Cancelled 行被迟到 `InputAdmitted` 降回 Pending。确定事件顺序的测试先红，TUI 投影增加终态守卫；Unknown 仍可被 ACK 确认 Pending。相关 reducer **32/32** 通过。
2. Runtime 原先先 enqueue/signal/start、后广播 admission，第二订阅者可能先见 settlement 却没有行。新增第二订阅者集成回归先红（startup 早于 admission），修为持久化后先同步广播，再暴露执行。Journal target **8/8** 通过，独立审阅通过。

全量检查还发现一个遗漏的旧 E2E 契约：`tui_session_e2e` 仍用 Enter 并断言运行中暂存。已真实跑红，按既定产品决策改为显式 Tab，保留所有队列、状态和自动推进断言并新增恰一项队列断言；整个 target **8/8** 通过，独立审阅通过。这只同步测试触发方式，未改变产品。

其余本轮修改为：0049–0051 验收测试、过期 TypeScript 协议机械再生成、已有变更的必要 rustfmt，以及持久证据目录/唯一运行目录/身份/SQLite backup/脱敏日志/封存顺序。未改变迁移历史或扩展产品机制。

## C. 最终真实 Dogfood

使用真实 CodeLeveler、`deepseek-v4.1-flash-1017`、主调用 `reasoning_effort=high`、真实 PTY。Phase 3 是 daemon＋独立 TUI；Phase 2 core4 明确为原设计的 **in-process**，仍使用真实模型和 PTY，不能计为 daemon 验收。Compaction 场景声明较小上下文/输出限制以触发实际压缩；观测到的全部模型参数保存在每轮 `identity.json`。

| 场景 | 次数 | 检查 | 结果 |
| --- | ---: | ---: | --- |
| 普通Enter、双TUI、B重连 | 1 | 22/22 | PASS |
| ACK后SIGKILL与Resume | 3 | 78/78 | PASS |
| Compaction前及期间追加 | 1 | 26/26 | PASS |
| 后台wait_task期间追加 | 1 | 18/18 | PASS |
| /goal期间追加 | 1 | 18/18 | PASS |
| 非取消失败后恢复 | 1 | 17/17 | PASS |
| 三条独立追加后崩溃恢复 | 3 | 114/114 | PASS |
| 取消后不复活 | 1 | 16/16 | PASS |
| Access布局＋管理员管理 | 1 | 24/25 | FAIL |
| 双TUI在线＋daemon崩溃组合 | 1 | 33/33 | PASS |
| Phase2 case1 | 1 | 18/18 | PASS |
| Phase2 case5c | 1 | 9/9 | PASS |
| Phase2 case5e | 1 | 10/10 | PASS |
| Phase2 case6a | 1 | 18/18 | PASS |

合计 **18 次运行、422 项检查，421 PASS / 1 FAIL**。基础 Phase 3 九场景是 8/9 通过（205/206 检查）；重复 sc2/sc7 均三轮通过，另增加双 TUI 崩溃组合 33/33；Phase 2 core4 为 55/55。

### Access 真实失败

追加“还有管理员管理也没有。”确实在原 turn 中收到、持久化、以独立 user 消息交付。Journal consumed、唯一 transcript 行和 `goals.amendments` 均保留原文。模型却将它理解为布局目标之外，明确拒绝实现；最终只完成布局，管理员路由仍 404。

权威证据位于 `runs/20261107-001721-phase3-product-access-do79h__x/`：请求 `0006.json` 的 messages[16] 为追加，messages[1] 的 host pin 为 User 角色原布局目标；后续请求保留追加。原始 SQLite、最终 PTY、工作区 diff 和失败 result 都在。独立随机 loopback 端口 HTTP 复核见 `independent-verification/20261107-001721-phase3-product-access-do79h__x-manual-http.json`：`/access`、`/roles`、CSS 200，`/admin` 404。

系统契约明确用户当前请求高于 repository 工作规则、源码属于 data；没有找到禁止后续用户扩展目标的高优先级指令。host pin 措辞是否诱发误解仍是推测，不能据此猜改 prompt。没有替模型补功能，也没有挑选另一次成功掩盖此失败。其他 fixture 的 case1 实现成功不替代本场景。

### 五类观测边界

- RECEIVED：真实 composer Enter、PTY 和 runtime 事件/快照。
- PERSISTED：Journal、ACK 后 SIGKILL 仍存在、恢复后 SQLite 实读。
- DELIVERED：真实 Provider 请求中的独立 user 消息，逐 ID/内容核对；不据 UI 推断。
- EXECUTED：README 标记、真实 HTTP、fixture 测试、工作区 diff；Access 管理员部分明确 FAIL。
- VISIBLE：双 TUI 的原始/解码帧、状态及同一身份一行；“已进入当前任务”不等同理解或完成。

原 sc2/sc7 的收尾 Web observer 在 daemon 更换后返回 404，已保留采集错误；原始 Provider、恢复 PTY、SQLite 和业务结果仍完整。这些轮次不声称存在有效的收尾 Web snapshot。额外双 TUI 组合保存真实生产 UDS snapshot 补充观测，不伪造旧 HTTP 结果。

## D. 代码集成与迁移

- 最终 HEAD：`788e18a2d53ceb038b6a85d062953c0604a8c542`；本轮 **无 Commit**，无 staging、Push、Tag、Release。
- 初始状态：124 个 tracked modified＋20 个 untracked 状态项（目录项包含多个文件）。原有改动全部保留。最终 Git 状态为 **147 项**，包含原有工作及本轮测试/文档新增；staging 未改变。
- 改动归属：追加消息、foreign-session-import、identity/recovery/schedule、共享协议/Runtime/TUI 分组与文件见 `OWNERSHIP_REVIEW.md`。schedule 的若干 Rust 变动实际是 foreign 字段 fixture；web observability 是 crash-unknown 投影消费者，不能随意排除。
- 不能只提交新增文件：共享协议结构和 fixture 构造跨多 crate；已应用 0049 的 DB 也要求构建携带该迁移。允许的集成提交仍需包含范围明确且通过门禁的完整依赖组；当前 foreign 门禁失败，因此本轮没有强行提交，也没有删除其他功能以拆分。
- 本轮相对初始备份的逐文件清单：`this-run-changes.json`；最终 Git 清单与 patch：`final-git-status.txt`、`final-tracked.patch`。保留的其他未提交工作为上述 foreign/identity/schedule 等已有修改。

### 迁移兼容性

`cargo test -p leveler-storage --lib append_input_migrations --locked`：**2/2 PASS**。真实 SQLx 历史覆盖 fresh/48/49/50/51 → 51，Session、message、foreign origin、Journal pending/consumed/cancelled、input_id 保留，重开迁移 metadata 不变，FK 完整。缺少已应用 49 的 Migrator 实际报 `VersionMissing(49)`，完整 Migrator 随后仍可打开。

0049、0050、0051 保持原文件，没有回退/删除用户数据库或绕过 missing version。App Journal 的历史读取和真实 Resume 另有 8/8、sc2、case6a 证据；Storage 测试没有被当作完整 Runtime 验收。

## E. 质量门禁

所有日志在持久归档 `logs/`；初轮和重跑均保留，没有删除失败、降低断言或新增 ignored。

| 命令/检查 | 实际结果 | 日志 |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 初轮 FAIL；必要 rustfmt 后 PASS | `fmt.log`、`fmt-final.log`、`fmt-after-test-sync.log` |
| `cargo check --workspace --locked` | 冻结源码后 PASS | `check-final.log` |
| `cargo clippy --workspace --all-targets --all-features --locked --keep-going -- -D warnings` | FAIL，foreign 的 4 项 lint | `clippy-final.log` |
| `cargo test --workspace --locked` | 初轮 FAIL：测试/CLI 构建身份不同，见下 | `tests.log` |
| `cargo test --workspace --all-features --locked --no-fail-fast` | **exit 101**，完整执行结束，2 targets FAIL：HOME 规则与旧 Enter 排队 fixture；后者按 Tab 契约同步后目标 8/8 PASS，原全量结果保留 | `tests-final.log` |
| `cargo test -p leveler-storage --lib append_input_migrations --locked` | 2/2 PASS | `migration.log` |
| `cargo test -p leveler-app --test input_journal` | 8/8 PASS（包含新增顺序回归） | `runtime-admission-green.log` |
| `cargo test -p leveler-tui --lib reducer::pending_input_tests` | 32/32 PASS | `late-admission-green.log` |
| `cargo test -p leveler-client-protocol --features schema --test schema_export --locked` | 3/3 PASS；无 schema feature 的 0 tests 不计作通过 | `schema-export-feature.log` |
| `cargo test -p leveler-tui --test tui_session_e2e --all-features --locked` | 旧 fixture 先红；契约同步后 8/8 PASS | `tui-session-e2e-recheck.log`、`tui-session-e2e-green.log` |
| `npm run check:protocol` | 初始 stale FAIL；现有生成器再生后 PASS | `protocol-ts.log`、`protocol-ts-green.log` |
| `npm run typecheck` | PASS | `web-typecheck.log` |
| `npm test -- src/lib/observabilityView.test.ts` | 12/12 PASS | `web-observability.log` |
| `python3 -m unittest discover -s scripts/tests -p test_append_instruction_evidence.py -v` | 6/6 PASS，独立复跑一致；恢复归档时使用直接 Xcode Python 3.9.6 再验 6/6 | `evidence-script-final-review.log`、`evidence-script-resume-check.log` |
| `cargo test -p leveler-cli --test config_hot_reload_e2e --locked` | 同身份窄复跑 1/1 PASS | `config-hot-reload-recheck.log` |
| `cargo build --release --locked -p leveler-cli` | PASS | `release.log` |
| 真实 Dogfood | 17/18 场景通过，业务失败见 C | `dogfood-*.log`、`index.json` |

foreign Clippy 失败：`codex.rs:523` trim 后 split_whitespace；`walk.rs:166/180/403` 三处 collapsible_if。最终 workspace HOME 规则失败定位为 `leveler-foreign-session/src/lib.rs:48` 的 `|home| home.join(...)`，触发未修改的 `leveler-core::home` 架构测试。它们属于本轮开始前已有的 foreign 开发，独立记录，未擅自扩展修复范围。完整测试还暴露上述旧 Enter fixture；只改测试触发后窄 target 已绿，没有把原全量命令改报成功。未为 test-only 触发修正重跑整个 15 分钟 workspace：受影响的完整 target 已验证，未改动的 foreign 门禁仍有确定失败，因此整体仍为 FAIL。

初轮 config hot reload 失败是本次验证操作造成的构建身份污染：测试嵌入 `…61c9246db30d4fa3`，重建后的 CLI 为 `…233ba7dd21f492cd`，不能采用同一 daemon。前后 SHA、mtime 和身份实证在 `config-hot-reload-identity-mismatch.json`、`config-hot-reload-recheck-identities.json`；同身份窄复跑通过。最终全量测试重新在冻结源码上运行；真实 Dogfood 始终使用独立复制的固定二进制。

CI 配置已检查：Ubuntu 22.04、macOS、Windows 有 all-features Clippy/测试入口，Linux有格式检查/分片，另有协议生成、浏览器及 Windows Task Scheduler 条件入口。本轮实际平台为 macOS；当前源码 Linux、Windows CI/宿主结果为 **UNKNOWN**，不引用历史 SHA 的绿色 CI 冒充本轮结果。

## F. 剩余阻断及最小处理

1. **业务闭环 FAIL**：管理员管理已交付模型但未实现。保留失败；只有真实模型完成该追加、独立 HTTP 验收通过，才能关闭这一验收项。当前没有证据允许用新 Harness 机制或猜测 prompt 修复代替定位。
2. **工程 gate FAIL**：foreign Owner 修复上述 lint 与 HOME 路径归属后，复验失败门禁；不能删测试/加豁免。再按依赖组完成可构建、可审阅的本地集成。
3. **跨平台 UNKNOWN**：需要当前源码的 Linux/Windows 实际 CI/宿主结果后才能作平台结论。
4. **测试清理边界**：封存前发现 sc4 隔离 `execution-host`（PID 84966）仍存活；仅对已核实属于本轮的进程发送 SIGTERM，并实际观测其消失，见 `execution-host-cleanup.json`。该轮不证明宿主自动回收，也未定位为产品生命周期缺陷；没有据此猜修产品。
5. **历史证据不可恢复**：历史请求和 PTY 帧仍缺失，本轮新档案替代重新验收，历史重建报告不升格为原始证据。

这些结论分别描述模型执行、工程集成和证据，不把消息交付 PASS 或 Release 构建成功当作总体完成。

## G. 归档与复现

- 本轮持久根：`/Users/mengmian/CodeLeveler-final-archives/20261107T000707-acceptance`。
- 统一索引：[index.md](/Users/mengmian/CodeLeveler-final-archives/20261107T000707-acceptance/index.md)、`index.json`；每个 run 直接链接到 `result.json`，同目录有真实请求、PTY、SQLite、工作区/业务产物和 SHA 清单。
- 备份：`/Users/mengmian/CodeLeveler-final-archives/20261107T000634-backup/`，33,555 条内容/权限/符号链接实读校验；`VERIFIED.json` 记录 archive SHA-256 与 Git 状态未变。
- 被测二进制 SHA-256：`fb1c31ef75c103225da393254674da3cf2c614ffd59e4ef8f103d5d29ea33cac`；`candidate-build.json` 证明构建期间源码无内容漂移；`candidate-source/source.tar` 携带 0049–0051。之后产品源码未变；另有证据脚本/文档及旧 Enter 契约测试 fixture 的同步。实际运行驱动版本 SHA 保存在 `dogfood-harness-sha256.json`，最终脚本副本另存 `harness/`。
- 原历史备份仅复制 DB/工作区到 `recovered/`，13 份 DB integrity 均 ok；没有把历史 `config.toml` 拷入新证据档案或 Git。
- 独立审阅和原始事实：`OWNERSHIP_REVIEW.md`、`independent-verification/`。本轮失败全部保留。
- 恢复收尾归档时系统 `/usr/bin/python3` 启动器出现 Xcode 许可提示；直接 Xcode Python/Git 可以读取和封存，本轮未接受许可或修改系统。此前构建/测试结果仍按各自时间与日志保留；后续重建需可用的本机开发工具环境，见 `resume-environment-note.json`。
- [可重复执行说明](append-instruction-reproduction.md)。无常规构建清理 `target/` 的操作会删除这些档案。

目录、索引和各 run 校验清单已逐文件实读复核；全归档清单及核验结果在 `SHA256SUMS`、`ARCHIVE_VERIFIED.json`。最终 Git 状态与相对初始备份的清单已保存。精确比较了已配置的 3 项凭据值，归档中零命中，见 `credential-scan.json`；该扫描不声称能识别任意私有内容。原始证据没有加入 Git。未提交工作区是当前正常交付状态。
