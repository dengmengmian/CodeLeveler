# 运行中追加指令验收复现

本说明复现追加消息的可靠交付和真实模型执行，不能用单元测试或历史 PASS 代替真实模型验收。上一轮结论见 [2026-11-07 收尾报告](append-instruction-final-2026-11-07.md)。阻断项复查见 [2026-11-07 阻断项报告](append-instruction-blockers-2026-11-07.md)，G2 恢复复核见 [G2 流中断与恢复复核](append-instruction-g2-recovery-2026-11-07.md)，N2 探针复核见 [N2 修正与复核](append-instruction-n2-recheck-2026-11-07.md)。

## 构建与隔离

需要 macOS/Unix PTY、项目固定的 Rust 工具链，以及已有 `~/.leveler/config.toml` 中可用的 DeepSeek 配置。本轮实际使用 `deepseek-v4.1-flash-1017`、`reasoning_effort=high`。不要把配置、API Key 或完整环境变量写进归档或 Git。

先完整备份工作区并核验内容、权限、HEAD、tracked diff 和 untracked 文件。冻结产品源码后构建一次；不要在运行 CLI 集成测试期间替换其二进制或重新生成构建身份。不同身份的测试库与 daemon 会如实拒绝相互采用。

```sh
cargo build --locked -p leveler-cli
```

将二进制复制到本次独立持久目录，记录 SHA-256 和源码内容快照，再设置以下变量。路径应替换为本次唯一目录；复制仅供验收，不替换已安装的 `/opt/homebrew/bin/leveler`。

```sh
export LEVELER_BIN=/absolute/persistent/run/bin/leveler
export LEVELER_APPEND_EVIDENCE_ROOT=/absolute/persistent/run/runs
python3 -u scripts/append_instruction_phase3.py all
python3 -u scripts/append_instruction_phase3.py sc2 --repeat 2
python3 -u scripts/append_instruction_phase3.py sc7 --repeat 2
```

`all` 包含一次 sc2/sc7，上述额外两轮使关键崩溃竞态总计三次。

Phase 3 使用真实 `serve`、独立 TUI 进程、PTY 和 Provider。普通 Enter 从真实 composer 输入；代理保留真实请求，以其中独立的 `role=user` 消息证明 DELIVERED。

Phase 2 核心回归单独运行。它们使用真实模型和 PTY，但按原设计使用 `--in-process`，不能计作 daemon 验收。

```sh
python3 -u scripts/append_instruction_dogfood.py case1
python3 -u scripts/append_instruction_dogfood.py case5c
python3 -u scripts/append_instruction_dogfood.py case5e
python3 -u scripts/append_instruction_dogfood.py case6a
```

脚本默认写入 `~/CodeLeveler-append-instruction-evidence/`，每次使用唯一、权限为 0700 的子目录。`LEVELER_APPEND_EVIDENCE_ROOT` 可以指定其他持久位置。禁止将唯一证据放进 `target/`。

## 核对证据

每轮保存 `identity.json`、`result.json`、`steps.log`、`requests/`、`proxy.jsonl`、PTY 的 `.raw`/`.txt`、数据库投影、`sessions.sqlite3`、工作区及 diff、`SHA256SUMS`。SQLite 是清理前的场景检查点，通过 backup API 包含已提交 WAL，并转为独立可读文件；它不声称是关闭客户端后的终态。Proxy 先等待请求线程结束，再封存清单。

```sh
cd /absolute/persistent/run/runs/unique-case-directory
shasum -a 256 -c SHA256SUMS
sqlite3 sessions.sqlite3 'PRAGMA integrity_check;'
```

进一步核查 Journal ID、transcript 的 `input_id`/ordinal、真实 Provider 请求、两端 PTY 状态与业务产物。ACK 后崩溃须观测 daemon 原先存在、SIGKILL 后退出、恢复进程和恢复投递。取消场景须检查后续全部请求不含取消消息。消息状态“已进入当前任务”只证明交付，不证明模型理解或实现。

不要只读 `result.json` 就批准。业务探针失败应独立用随机 loopback 端口 HTTP 复核；不能替模型补代码，也不能挑选另一次成功覆盖失败。

本轮额外双 TUI + daemon 崩溃组合的薄驱动保存在持久归档 `dual_crash_probe.py`，执行记录在 `dual-crash-execution.json`；其 SHA、真实生产 UDS Snapshot 和双 PTY 帧都在对应运行目录。该驱动复用 Phase 3 的 sc2，未修改产品或冻结的仓库驱动。

## 工程门禁

按顺序在同一份冻结源码上运行，并保存命令、退出码和完整日志；不并发替换被测 CLI。平台结果必须来自当前代码的实际运行。

```sh
cargo fmt --all -- --check
cargo check --workspace --locked
cargo clippy --workspace --all-targets --all-features --locked --keep-going -- -D warnings
cargo test --workspace --all-features --locked --no-fail-fast
cargo test -p leveler-storage --lib append_input_migrations --locked
cargo test -p leveler-app --test input_journal --locked
cargo test -p leveler-tui --lib reducer::pending_input_tests --locked
cargo test -p leveler-client-protocol --features schema --test schema_export --locked
python3 -m unittest discover -s scripts/tests -p test_append_instruction_evidence.py -v
cargo build --release --locked -p leveler-cli
```

在 `crates/leveler-web/web` 内执行 `npm run check:protocol`、`npm run typecheck` 和 `npm test -- src/lib/observabilityView.test.ts`。Schema 发生变化时先用已有 `npm run gen:protocol` 再生成，保留漂移失败和增量记录。

迁移测试覆盖新库和真实 0048/0049/0050/0051 SQLx 历史、数据保留、重复打开及 `VersionMissing(49)`。不得删除用户数据库、篡改已应用历史或跳过缺失版本检查。构建必须携带 0049–0051 全部迁移。
