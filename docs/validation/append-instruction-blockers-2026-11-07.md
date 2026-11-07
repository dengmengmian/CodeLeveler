# 运行中追加指令阻断项报告 · 2026-11-07

本报告只覆盖上一份收尾报告留下的阻断。历史结论仍以 [2026-11-07 收尾报告](append-instruction-final-2026-11-07.md) 为准，这里不改写那份 FAIL / BLOCKED。

证据目录：`/Users/mengmian/CodeLeveler-final-archives/20261107T100053-blockers/`。11 轮调度、退出码和 SHA 复核在 `executions.json`。取证在 `CONTEXT_FORENSICS.md`，foreign 修复在 `FOREIGN_GATE_REPAIR.md`。

## 判断

| 维度 | 判断 | 依据 |
| --- | --- | --- |
| 消息可靠性 | **PASS** | 11 轮追加都入 journal、同一回合送达并消费。N2 的两项 transcript 检查是测量假失败，封存库里行和身份都在。 |
| 追加指令产品闭环 | **PASS** | 原 Access 生产链路 3/3 完成布局和 `/admin`。同一拒绝没有再现。 |
| 功能最终关闭 | **否** | `/goal` 第 2 轮在写出任何修改前因模型流中断结束，业务检查 5 项失败。 |
| 工程集成 | **PASS** | macOS 上 fmt、check、Clippy、全量测试、迁移、schema、协议、release 构建都是 exit 0。批准文件的 SHA-256 与工作区一致。 |
| 本地提交 | **未做** | 集成审查要求生产链路验收通过后再提交。G2 是真实未完成轮次。 |
| 发布质量门禁 | **BLOCKED** | 本轮只有 macOS 实测。当前源码的 Linux / Windows 结果仍是 UNKNOWN。 |

没有部署、安装、push、tag 或 release。

## A. 原 Access 失败的原因

旧失败目录仍是 `20261107T000707-acceptance/runs/20261107-001721-phase3-product-access-do79h__x/`。追加「还有管理员管理也没有。」已送达；模型把它判在原布局目标之外，最终 `/admin` 为 404。

本轮固定同一二进制、同一配置和同一 fixture 后：

| 轮次 | 入口 | 结果 | 业务 |
| --- | --- | --- | --- |
| B1 B2 B3 | 原生产路径 | 28/28 PASS | 布局与管理员管理都落地 |
| C1 C2 C3 | 仅去掉 Host pin 的诊断对照 | 29/29 PASS | 同样落地；不计入生产验收 |
| G1 G3 | 真实 `/goal` | 28/28 PASS | 同样落地 |
| G2 | 真实 `/goal` | 23/28 FAIL | 交付检查通过，文件没有变化 |
| N1 | 原任务，自然说法「一并完成」 | 28/28 PASS | 同样落地 |
| N2 | 原任务，另加 `/health` | 业务通过 | `GET /health` 为 200，JSON `{"status":"ok"}` |

有 pin 和去掉 pin 的各 3 轮都完成了原追加。这一组对照没有分离出 Host pin 的稳定因果。本轮没有改提示词，也没有加新的 Harness 机制。

G2 的回合状态是 `failed`。代理记录两次 HTTP 200。第 3 次请求约 62 秒，运行时记为 `decode` 并做了一次 protocol repair。第 4 次约 93 秒，`finish_reasons` 为空，错误是 `stream ended before completion`，`provider_retries_exhausted=false`。进度里已有推理、正文和工具参数。最后可见输出准备补 `/admin`，随后是 “Response interrupted before completion”，工具没有执行。目标 amendments 仍保留追加原文，goal 状态停在 `running`。

## B. 重复实验

调度顺序是 B1、C1、B2、C2、B3、C3、G1、G2、G3、N1、N2。每轮都保留失败和完整归档，没有用成功轮覆盖失败轮。11 轮 `baseline_valid=true`，`invalidated_by` 为空，SHA 不一致为 0。

被测二进制 SHA-256：`623ebbf28fcfac27940cfc0a87cb7a94d4de5ab6ecfd203ea4d4c3d567501858`。模型是 `deepseek-v4.1-flash-1017`，`reasoning_effort=high`。配置原文没有进入归档。

N2 封存结果是 24/26，失败项为 `one_transcript_row` 和 `row_keeps_journal_identity`。`user_message_rows` 在原始 JSON 文本里查找追加句子；这句含有引号，落库后引号被转义，子串匹配为 0。只读复查 `sessions.sqlite3`：`session_messages` ordinal 3 是 user 行，`input_id` 等于 journal `6910b743-b200-4722-9b67-ce4fedb5585a`。复查记录在 `N2_TRANSCRIPT_RECHECK.json`。封存的 `executions.json` 仍记为 FAIL，没有回写。

## C. 本轮代码变化

产品闭环没有新的行为补丁。foreign-session 门禁修复发生在实验开始之前，范围是路径归属命名和等价 lint，行为测试与 workspace Clippy 已通过。详见归档 `FOREIGN_GATE_REPAIR.md`。

## D. 工程质量门禁

命令都在仓库根目录执行，协议检查在 `crates/leveler-web/web`。记录在 `quality-final-*.json`。

| 命令 | 退出码 | 耗时 |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 0 | 2.3s |
| `cargo check --workspace --locked` | 0 | 4.4s |
| `cargo clippy --workspace --all-targets --all-features --locked --keep-going -- -D warnings` | 0 | 9.5s |
| `cargo test --workspace --all-features --locked --no-fail-fast` | 0 | 672.7s |
| `cargo test -p leveler-storage --lib append_input_migrations --locked` | 0 | 3.5s |
| `cargo test -p leveler-client-protocol --features schema --test schema_export --locked` | 0 | 3.8s |
| `npm run check:protocol` | 0 | 0.1s |
| `cargo build --release --locked -p leveler-cli` | 0 | 178.9s |

全量测试日志里没有 `FAILED`。Release 构建结束。这些结果不代替上面的真实模型轮次。

## E. 集成与提交

HEAD 仍是 `788e18a2d53ceb038b6a85d062953c0604a8c542`。`integration-selection.json` 的 164 个批准文件与当前内容 SHA-256 一致，没有缺失。独立 schedule soak 的 3 个路径继续留在提交范围外。

没有执行 `git add` 或 commit。G2 的业务未完成，不满足审查里的提交前提。新的阻断报告也不在当时批准的文件清单里。

## F. 剩余事项

1. G2 的模型流在完成前结束。追加已经送达，业务没有落地。这轮保留为 FAIL。
2. N2 的 transcript 检查会漏掉含引号的原文。产品行在封存库里。检查函数还没有改。
3. Linux 和 Windows 上的当前源码门禁没有实测。
4. 原失败样本仍然有效。本轮 3 次生产重复都完成了同一追加，所以它不再表现为稳定的范围拒绝。
5. 历史 Phase 3 丢失的请求和 PTY 没有被补造。
