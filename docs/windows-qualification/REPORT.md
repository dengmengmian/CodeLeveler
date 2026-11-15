# CodeLeveler Windows 11 Full Qualification

> 状态：已执行完毕并定稿。当前候选在已执行范围为 **PARTIAL PASS**；Task Scheduler 为非提升 XML 注册失败（L6，未修复），独立标准用户、ConHost、Release Lab/RC、远端 CI、PS7/VS Code/IME、macOS/Linux 与 Windows 10 当前候选为 BLOCKED / NOT VERIFIED。未执行 push、tag、release 或远端写操作。

## 1. 候选与版本身份

| 项目 | 值 |
| --- | --- |
| 基线 `main` | `644d765c6208eba7b59bc0db88559a0445036135`（v1.0.27 之后 4 个提交） |
| 本次修复提交 | `0ebfa59d` doctor PATHEXT；`4b159c83` TUI DA1；`6edf54ec` ACL 测试能力；`0f9a5cbd` debug-only fault probe；`9936f513` rmdir 开关分类；`8d76717d` TUI pager 路径/引号；`9693cc68` Windows 测试夹具；`1016b495` 写根恢复并发；`3390713d` memory 原子替换；`940ae12c` runtime 端点 EOF；`4bc08296` Windows 原生测试夹具 |
| 最终候选代码 HEAD | `4bc08296`（报告 HEAD 在提交报告后记录） |
| 版本 | 1.0.27 |
| 最终候选产物 | 见第 8 节与 `evidence/candidate.sha256` |
| 安装方式 | 从 ZIP 解压到 `C:\qlab\install\archive` 验证；未覆盖用户已安装的 `C:\Users\code\.local\bin\leveler.exe` |
| 用户真实配置/会话 | 只读复制到 `C:\qlab\home-upgrade` 用于兼容性检查；未修改原件 |

## 2. 真机环境

完整环境身份见 [ENVIRONMENT.md](ENVIRONMENT.md)。要点：

- Windows 11 家庭中文版，Build 26300.9711，x64。
- Windows Terminal 1.25.3131.0；`WT_SESSION` 真实存在。
- PowerShell 5.1 / CMD / Git Bash；无 PowerShell 7、无 VS Code、无 WSL 发行版。
- zh-CN，ANSI/OEM 代码页 936；物理 1920×1080 @150% DPI。
- 用户 `code` 属于 Administrators 组但 token 未提升，进程 IL 为 Medium。
- 长路径未启用；测试工作区位于短路径 `C:\qlab`，另含中文+空格目录。

Windows 10 历史证据不在本仓/本机，见 [HISTORICAL_BASELINE.md](HISTORICAL_BASELINE.md)。

## 3. 方法与证据原则

- 最终结论只关联最终候选的代码身份；修复前后的候选分开记录。
- Dogfood 使用真实 Provider（`taotoken/deepseek-v4.1-flash-1017`）、真实模型、真实工作区、真实工具执行；scripted/mock 只用于确定性回归与故障注入。
- 安全结果以真实文件副作用、子进程完整性级别和退出码为准，不以错误文案或初始化成功代替。
- 未执行的矩阵项标记 NOT VERIFIED；环境不允许的项标记 BLOCKED，不以其他宿主结果外推。

## 4. 本次修复的缺陷
| ID | 级别 | 缺陷 | 根因 | 修复与验证 |
| --- | --- | --- | --- | --- |
| WIN11-D1 | P1 | `leveler doctor` 在所有正常 Windows 主机上误报 git/cargo/node 缺失，必需 git 检查失败并使 doctor 退出非零 | `find_in_path` 只做 `dir.join(name)`，Windows 执行依赖 `PATHEXT`（`git` → `git.exe`） | 按 PATHEXT 生成候选名并保留精确名回退；Windows 单测先 Red（`None`）后 Green；真机 doctor 中 git/cargo 变为 ✓，退出 0 |
| WIN11-D2 | P1 | Windows Terminal 的 DA1/Sixel 能力应答在探测超时后落入输入框，显示为 `7;14;…;52c` 等终端簿记文本 | 探测在 150 ms 固定窗口内未读完回复时，未消费的应答尾部留在与 TUI 共用的输入队列 | 回复一旦开始就获得独立完成窗口；未完成时消费已排队尾部；只有 Esc/`[`/`?` 可延长探测。单测覆盖 DA1 解析与 opener 判定；CPU 高负载下真机 TUI 粘贴不再出现前缀，吉祥物仍正常显示 |
| WIN11-D3 | P2（测试可移植性） | `windows_acl` 的 6 个 restore 单测在非提升 Medium 用户上失败 | `icacls /restore` 需要 `SeRestorePrivilege`；该 coordinator 未被出货的 Low-IL 后端使用 | 按宿主能力断言：有特权时验证成功恢复，无特权时验证明确的 `Restore` 错误；CI 同款 canary 命令 0 failed |
| WIN11-D4 | P3（构建卫生） | Release 构建出现 `unused variable: exited` 警告 | debug-only fault probe 的绑定在 release 中未使用 | 绑定与使用同置于 `#[cfg(debug_assertions)]`；release 构建无该警告 |
| WIN11-D5 | P1 | TUI `/output` 完整输出 Pager 在 Windows 上失败（`'more' 以非零状态退出`） | `Capture::open` 的 `fs::canonicalize` 返回 `\\?\C:\...`，`more.com` 无法打开；同时 Rust 的 argv 引号把 `cmd /C` 尾部包住，路径带字面引号到达 cmd | pager 路径经 `display_path` 转为普通盘符/UNC 形式；Windows 用 `raw_arg` 把尾部原样交给 `cmd /C`。Windows 单测先 Red（收到带引号的 verbatim 路径）后 Green |
| WIN11-D6 | P2 | `rmdir /S /Q 相对目录` 在工作区内也被要求审批，违反“字面路径工作区删除自动运行”的策略 | `deletion_targets_stay_in_workdir` 把 `/S`、`/Q` 当作删除目标操作数 | 仅对 Windows `rmdir`/`del` 识别 `/` 开关；Unix 的前导 `/` 仍视为绝对路径，绝对 Windows 路径仍要求审批。Windows 单测覆盖 Safe/Dangerous 两侧，loop_test 真机通过 |
| WIN11-D7 | P1（并发正确性） | 同一 `LEVELER_HOME` 下多个 `Application`/窗口并存时，一个窗口的启动恢复会把另一个窗口正在使用的写根标签从 Low 恢复为 Medium；该窗口的 Low 命令随后以“拒绝访问”失败 | `recover_stale_write_roots` 把目录里所有 `.label` 记录都当作崩溃遗留，没有“记录仍被活动租约持有”的事实源 | 每个记录增加 sidecar guard 独占锁，活动租约持有到标签恢复完成；恢复只处理能拿到 guard 的记录，并显式跳过本进程 `held` 中的根；租约时对遗留记录做一次加锁恢复。新增 `recovery_leaves_a_live_lease_alone` 先 Red（恢复后标签不再是 Low）后 Green；`execution_ownership` 压测由 3/10 失败变为 12/12 通过 |
| WIN11-D8 | P2（数据原子性） | 并发读线程打开 memory topic 时，原子替换偶发 `拒绝访问 (os error 5)` | overwrite 路径使用 `tempfile::persist`，其 Windows 实现 `MoveFileExW` 不能替换仍被读句柄打开的目标 | overwrite 改用 `std::fs::rename`（NTFS 上原子且可替换读打开的目标）；`persist_noclobber` 保留新建语义。并发测试由 2/40 失败变为 100/100 通过 |
| WIN11-D9 | P2（启动可靠性） | Windows named pipe 服务端在握手后立即退出时，`probe_default_runtime` 返回 `UnexpectedEof`，启动把“端点不存在”误报为传输错误 | `endpoint_unavailable` 未把 Windows EOF 归类为端点不可用 | 把 `UnexpectedEof` 与 ECONNRESET/EPIPE 同等归类；共享分类单测在 Windows 也运行；`windows_runtime_host` 真机验收 10/10 通过 |
| WIN11-D10 | P2（测试可移植性） | 全量 Windows 套件中若干仅按 Unix 语义编写的测试失败：`sleep`/`sh`/裸 `.cmd` 工作区脚本、未转义的 HOME JSON、空环境快照下 `cmd` 无法执行批处理 | 测试夹具假设 POSIX 工具和完整宿主环境；不是产品在这些路径上的真实失败 | 改为 Windows 原生命令/显式 `.\` 脚本；JSON 夹具使用固定 POSIX 路径；后台批处理测试改用产品安装的宿主环境快照。目标测试全部 Green；全量套件复跑结果见第 7 节 |

原生 Ctrl+V 多行/非 BMP 粘贴的换行提交与 emoji 丢失是 **Windows Terminal 宿主行为**，不是本次新缺陷：产品已在 footer 提供 Alt+V，源码注释明确记录该宿主路径限制。详见第 9 节 WIN11-10。

## 4.1 安全与权限证据

**身份矩阵**

| 环境 | 结果 |
| --- | --- |
| 管理员组成员的非提升 Medium 会话（本次宿主） | 已验证；IL Medium，Administrators 在过滤 token 中为 deny-only |
| 独立标准用户（不在 Administrators） | BLOCKED；创建需要管理员授权，本次未执行；未把当前 token 等同于标准用户 |
| Low Integrity 子进程 | 已验证；`S-1-16-4096`，由 `leveler-confine.exe` 启动 |
| 提升管理员环境 | 未使用；未据此掩盖标准用户问题 |

**独立 ACL / Low-IL canary（真实文件副作用）**

`evidence/security-canary-manual.log` 记录 11 项检查，全部 PASS：

| 检查 | 结果 |
| --- | --- |
| 子进程 IL 为 Low | PASS |
| 读取已验证写根内的文件 | PASS |
| 在已标记写根内创建文件 | PASS |
| 修改写根内既有文件 | PASS |
| 写入未标记的外部目录 | 拒绝（exit 1，文件不存在） |
| 通过 Junction 写入外部目录 | 拒绝（exit 1，目标不存在） |
| `..` 相对路径穿越 | 拒绝（exit 1，目标不存在） |
| `\\?\` 扩展路径穿越 | 拒绝（exit 1，目标不存在） |
| 显式 ACL Deny(W) 目录 | 拒绝（exit 1，文件不存在） |
| 写根内大小写不同路径 | 允许 |
| 宿主未提升 | `IsInRole(Administrator)=False` |

**CI 同款 Windows canary**

`cargo build -p leveler-win-confine --all-features` + `cargo test -p leveler-tools windows_ ...` + `cargo test -p leveler-execution windows_ ...`：76 个匹配测试通过（73 execution + 3 tools），0 失败，退出 0。覆盖真实 NTFS ACL、Low Integrity 写根、Job Object 取消/超时杀死孙进程、真实 `cargo build`、崩溃后标签恢复、并发恢复保护、Junction/父目录替换等。证据：`evidence/security-canaries-final.log`。

**Launcher 缺失 fail-closed**

把 `leveler.exe` 单独复制到不含 `leveler-confine.exe` 的目录后，doctor 报告 `write=Unsupported backend=None`；真实模型执行 `shell_command` 时在 spawn 前被拒绝：

```text
Windows FS sandbox is not available (capability write=unsupported); refuse non-FullAccess spawn with write_root="\\?\C:\qlab\repos\ascii-tui"
```

命令从未执行，`MISSING-LAUNCHER-CANARY` 未出现在任何 stdout，`modified_files: []`，会话以 `Blocked` 结束；模型尝试申请 FullAccess 被非交互审批拒绝。证据：`evidence/nolauncher-run.jsonl`。

**ACL coordinator 限制（非出货路径）**

`windows_acl` 的 `icacls /restore` 需要 `SeRestorePrivilege`，非提升 Medium token 不具备（`whoami /priv` 实测）。该 coordinator 当前没有生产调用者，出货的 Low-IL 后端不经过它；测试按宿主能力断言明确的 `Restore` 错误，而不是伪造成功。若未来把它用于标准用户路径，需要改用不依赖 restore 特权的 ACL 恢复机制。

## 5. 验收结果

| 验收项 | 结果 | 证据 |
| --- | --- | --- |
| 全量 workspace 测试 | PASS：328 个 test binary，7930 passed / 0 failed，exit 0 | `evidence/workspace-tests-final.log` |
| 全量 workspace Clippy | PASS：`-D warnings`，exit 0 | `evidence/clippy-full.log` |
| Format | PASS：`cargo fmt --all -- --check` | 本报告第 7 节 |
| 真实业务 Dogfood A/B/E | PASS：真实 Provider/模型，A/B/E 均 Completed，独立 `cargo test` exit 0 | `evidence/dogfood-a-final-4bc08296/` 等 |
| 真实 TUI 关键路径 | PASS/见 5.1：WT 1.25、中文路径、Alt+V、DA1 修复、`/output` Pager | `evidence/tui-final2-*.png` |
| 安全 canary | PASS：11 项独立 canary + CI 同款 76 项（73 execution + 3 tools，0 failed） | `security-canary-manual.log`、`security-canaries-final.log` |
| Browser acceptance | 见第 7 节 | `evidence/browser-acceptance.log` |
| Task Scheduler acceptance | 见第 7 节 | `evidence/task-scheduler-acceptance.json` |
| Release Lab / RC | BLOCKED：本机/本仓无外部实验室脚本 | 第 7 节 |
| 独立标准用户 / ConHost | BLOCKED：需要管理员授权/修改用户终端设置 | `LIMITATIONS.md` |

### 5.1 TUI 真机观察（已执行）

| 项目 | 结果 | 证据 |
| --- | --- | --- |
| Windows Terminal + CMD（`cmd /k` 启动 TUI） | PASS；真实 WT 窗口、Sixel 吉祥物、中文界面 | `evidence/tui-layout-120x30-clean.png` |
| 最终候选 TUI 启动 | PASS；真实 WT 新窗口，版本 1.0.27、Sixel 吉祥物、中文+空格工作区、模型/权限 Footer | `evidence/tui-final2-welcome.png` |
| Windows Terminal + PowerShell 5.1（PowerShell runner 启动 TUI） | PASS；中文+空格工作区路径与分支正常 | `evidence/tui-chinese-path.png` |
| Windows Terminal + PowerShell 7 | NOT VERIFIED；本机未安装 `pwsh` | — |
| Console Host 独立窗口 | BLOCKED；Windows 11 默认委派给 WT，未获授权修改 `HKCU\Console\%%Startup` | `ENVIRONMENT.md` |
| VS Code Integrated Terminal | NOT VERIFIED；本机未安装 VS Code | — |
| 120×30 布局 | PASS；Header、欢迎卡、输入框、Footer、模型/权限/协作状态完整 | `evidence/tui-layout-120x30-clean.png` |
| 100×30 布局 | PASS；吉祥物显示，全部 hint 完整 | `evidence/tui-layout-100x30.png` |
| 80×24 布局 | PASS（有意的截断）；吉祥物隐藏且不保留空白，最后一条 hint 省略，模型名与时钟保留 | `evidence/tui-layout-80x24.png` |
| 最大化窗口 | PASS；大窗口布局稳定；系统“激活 Windows”水印覆盖右下角（环境因素） | `evidence/tui-layout-maximized3.png` |
| 图片协议支持 | PASS；WT 1.25 + DA1 属性 4 → Sixel 高清吉祥物 | `evidence/tui-layout-120x30-clean.png` |
| 图片能力未知 / Sixel 不支持 | PASS；清除 `WT_SESSION` 后直接隐藏吉祥物，无字符画/像素图/空白占位 | `evidence/tui-image-hidden.png` |
| 中文路径与分支符号 | PASS；Header 显示 `C:\qlab\repos\中文 工程 · ◆ master`，无 `\\?\` 前缀 | `evidence/tui-chinese-path.png` |
| 权限模式切换 | PASS；Shift+Tab 在 auto / full / ask 间切换，Footer 同步；切到 FullAccess 时出现“完全访问（免审批）”提示 | `evidence/tui-permission-full.png`、`evidence/tui-permission-request.png` |
| 帮助与面板快捷键 | PASS；Ctrl+? 打开帮助，Alt+1 会话列表，Alt+3 模型选择器，中文完整 | `evidence/tui-shortcuts-overlay.png`、`evidence/tui-alt1-history.png`、`evidence/tui-alt3-model.png` |
| 中文/多行/emoji 粘贴（Alt+V） | PASS；保留换行与 🙂，不提交 | `evidence/tui-altv-multiline.png` |
| 原生 Ctrl+V / Ctrl+Shift+V 单行 BMP | PASS（单行中文可粘贴） | `evidence/tui-ctrlc-paste.png` |
| 原生 Ctrl+V / Ctrl+Shift+V 多行或非 BMP | PARTIAL（宿主限制）；WT 把换行变成 Enter，消息被提交为 steer，emoji 丢失；Footer 提供 Alt+V 作为可靠替代 | `evidence/tui-multiline-paste.png`、`evidence/tui-ctrlshiftv-paste.png` |
| DA1 应答泄漏（修复前） | FAIL 复现；输入框出现 `7;14;…;52c` 前缀 | `evidence/tui-paste-altv.png` |
| DA1 应答泄漏（修复后） | PASS；相同高负载场景下输入框只含粘贴文本 | `evidence/tui-da1-fixed-paste.png` |
| `/output` 完整输出 Pager（修复后） | PASS；真实 TUI 本地命令产生 4627 行输出，`more` 正常打开并显示 `-- More (0%) --`，不再出现“以非零状态退出” | `evidence/tui-final2-pager.png`、`evidence/tui-final2-shell-output-long.png` |
| Shell Mode 输入 | PARTIAL；通过真实剪贴板粘贴 `!cmd ...` 进入并执行本地命令；合成 Shift+1 在本次自动化环境中被当作普通字符，物理键盘路径未独立验证 | `evidence/tui-final2-shellmode.png`、`evidence/tui-final2-shell-output-long.png` |
| 双 Ctrl+C 退出确认 | PASS；Footer 提示“再按一次 Ctrl+C 退出”，未立即退出 | `evidence/tui-cancel-ctrlc.png` |
| IME 候选与组合输入 | NOT VERIFIED；无法用合成按键可靠驱动系统 IME；需人工按第 9 节步骤验证 | — |


## 6. 真实业务 Dogfood

最终候选 `1.0.27 (4bc082963fee)` 在题述 Windows 11 真机上，以真实 Provider `taotoken` / 模型 `deepseek-v4.1-flash-1017`（high）运行。每个场景都有真实工区、真实工具调用和独立的 `cargo test`。

| 场景 | 结果 | 关键证据 |
| --- | --- | --- |
| A：订单 CSV 汇总实现 | Completed，7 轮，12 次工具调用，修改 `src/lib.rs`；独立 `cargo test` 4 passed / 0 failed / exit 0；`data/orders.csv` 未修改 | `evidence/dogfood-a-final-4bc08296/` |
| B：库存对账 + CLI | Completed，12 轮，18 次工具调用；两次后台 `cargo test` + `wait_task`，再 `cargo run -- data/stock.csv data/movements.csv`，输出 SKU-1=9 / SKU-2=9 / SKU-3=6；独立测试 4/4 | `evidence/dogfood-b-final-4bc08296/` |
| E：README 收尾 + 重新验证 | Completed，8 轮，11 次工具调用；只修改 `README.md`；README 修改后重新运行 `cargo test`，4/4 通过 | `evidence/dogfood-e-final-4bc08296/` |
| MCP 真实工具调用 | Completed，5 轮；批准 `mcp__qual__echo_business` 后真实执行，fixture 记录 `WIN11-MCP-4BC08296` | `evidence/mcp-final.jsonl`、`evidence/mcp-side-effect-final.log` |

A/B/E 的 `run.jsonl` 均以 `session_completed` + `stop_reason=Completed` 结束，不是模型自述完成；独立测试在新进程执行。

## 7. 工程门禁

| 门禁 | 命令 | 结果 |
| --- | --- | --- |
| Format | `cargo fmt --all -- --check` | PASS（全仓检查通过） |
| Workspace check | `cargo check --workspace --all-features --locked` | 由 Clippy 全量覆盖；PASS |
| Workspace Clippy | `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | PASS，exit 0 |
| Workspace tests | `RUST_TEST_THREADS=2 cargo test --workspace --all-features --locked --no-fail-fast` | PASS：328 个 test binary，7930 passed / 0 failed，exit 0 |
| `./dev verify` | `./dev verify` | PASS（`NO_OWNED_CHANGES` + version 1.0.27；提交后无未拥有改动） |
| Windows security canaries | `cargo build -p leveler-win-confine --all-features` + `cargo test ... windows_` | PASS：73 项 execution + 3 项 tools，0 failed；手动 11 项 canary PASS |
| Browser acceptance | `LEVELER_BROWSER_ACCEPTANCE=edge cargo test -p leveler-browser --test acceptance ...` | PASS：12/12，真实 Edge；包含网页交互、崩溃、截图、进程清理、公网访问 |
| Task Scheduler | `LEVELER_TASK_SCHEDULER_ACCEPTANCE=1 ... cargo test -p leveler-schedule --test task_scheduler_acceptance` | **FAIL**：`schtasks /Create /XML` 在非提升 Medium 会话返回“拒绝访问”；简单 `/Create` 非提升可成功，见第 9 节 L6 |
| Release payload | `sh packaging/scripts/check_release_payload.sh` | PASS |
| Release Lab / RC | `./dev pre-release` / `./dev release` | BLOCKED：`FAIL DOGFOOD_ROOT_NOT_FOUND`，无外部 dogfood lab；未执行 `./dev release` |
| 远端 CI | GitHub Actions | NOT RUN（未授权 push / 远端运行） |
| macOS / Linux | 本地跨平台检查 | NOT VERIFIED（本机无对应宿主；未执行 cross-target build） |


## 8. 最终产物

版本：`1.0.27 (4bc082963fee)`（provenance 无 dirty 标记）。

| 文件 | SHA-256 |
| --- | --- |
| `leveler.exe` | `d6613530b909bca6e0de5bbc9f2ab5cbac1f3d92574a1e2a087d30d9844e7742` |
| `leveler-confine.exe` | `51398c17c0dfb7ae896392ba9df284927efee47e3896eb0e6a794f21ba7b9ab5` |
| `leveler-v1.0.27-x86_64-pc-windows-msvc.zip` | `8e4ef237e3102bf58f5e5204bf0b625364475b87ba183277ee19fc492289bf58` |
| `README.md` | `d56333375683882ed0ac8928b607ed71a65bd19562a4181c7e66272c04e0fd2d` |
| `LICENSE-APACHE` | `c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4` |

ZIP：`C:\qlab\artifacts\leveler-v1.0.27-x86_64-pc-windows-msvc.zip`；解压至 `C:\qlab\install\archive\leveler-v1.0.27-x86_64-pc-windows-msvc\`。归档冒烟：`--version`、`completions bash`、`leveler-confine -- cmd /d /c exit 23` 全部通过。未覆盖用户已安装的 `C:\Users\code\.local\bin\leveler.exe`。

## 9. 剩余风险与未验证项

完整复现见 [LIMITATIONS.md](LIMITATIONS.md)。

| ID | 级别 | 项 | 状态 / 影响 |
| --- | --- | --- | --- |
| L1 | P2 | Git Bash/MSYS `rm`/`printf`/`sh` 在 Low Integrity 沙箱内无法创建 MSYS 命名对象（0xC0000022） | 确认；原生命令（cmd/PowerShell/cargo/git）正常；替代为原生命令或 FullAccess |
| L2 | 已撤销 | 早期认为“后台 `.cmd`/`.bat` 脚本失败” | 复核为测试夹具使用空环境快照（缺 `SystemRoot`），`cmd` 无法执行批处理；产品入口安装完整宿主快照后，后台 `.cmd` 真实通过（`a_background_batch_script_runs_with_the_host_environment`） |
| L3 | P2 | 本机 `NoDefaultCurrentDirectoryInExePath=1`，裸脚本名不被 cmd 搜索；显式 `.\` 可用 | 环境限制；测试夹具已显式化，产品未静默移除用户设置 |
| L4 | P2 | Windows Terminal 原生 Ctrl+V/Ctrl+Shift+V 多行/非 BMP 丢失并拆分提交 | 宿主限制；Alt+V 已提供可靠 Unicode 粘贴 |
| L6 | P1 | 非提升 Medium 会话中 `schtasks /Create /XML` 返回“拒绝访问”，产品 Windows wakeup 后端无法启用计划任务；同账户简单 `/Create` 可成功 | 未修复；Task Scheduler 真机验收 FAIL；见 `LIMITATIONS.md` 与 `evidence/task-scheduler-acceptance.json` |
| U1 | — | 独立标准用户（非 Administrators） | BLOCKED：创建需要管理员授权 |
| U2 | — | Console Host 独立窗口 | BLOCKED：Windows 11 默认委派 WT，未改用户终端设置 |
| U3 | — | PowerShell 7、VS Code Integrated Terminal | NOT VERIFIED：未安装 |
| U4 | — | 中文 IME 候选与组合输入 | NOT VERIFIED：无法用合成按键可靠驱动，需人工步骤 |
| U5 | — | 图片渲染失败注入、160×45 精确网格 | NOT VERIFIED / PARTIAL |
| U6 | — | Release Lab / RC | BLOCKED：仓库/本机无 `eval/scripts/release_check.py`、`rc_check.py` |
| U7 | — | 远端 CI、macOS/Linux 真实终端、Windows 10 当前候选 | NOT RUN / NOT VERIFIED：未授权远端；本机无对应宿主 |

安全结论不因这些限制而放宽：P0 未发现；L1/L4 都是拒绝或宿主行为，不是权限被绕过。早期 L2 的 P1 结论已按真实根因撤销（空环境测试夹具，不是产品缺陷）。

## 10. 合并与发布资格

| 判定 | 结论 | 依据 |
| --- | --- | --- |
| Windows 11 当前候选 | **PARTIAL PASS（已执行范围）** | 全量测试、Clippy、安全 canary、真实 Dogfood、TUI 关键路径与 Edge acceptance 通过；Task Scheduler 非提升 XML 注册失败（L6），独立标准用户、ConHost、PS7/VS Code/IME、图片失败注入、160×45 仍 BLOCKED/NOT VERIFIED |
| Windows 10 历史证据 | **BLOCKED** | 本机/本仓没有历史 Windows 10 证据与 51 项失败记录，无法对照；不把 Windows 11 结果外推 |
| Windows 10 当前候选 | **BLOCKED** | 本机无 Windows 10 宿主；未运行跨机器验收 |
| 合并资格 | **条件性可合并（仅本地）** | 修复已本地提交并通过 Windows 全量门禁；未 push、未运行远端 CI；macOS/Linux 当前候选未验证，合并前应由远端 CI 覆盖 |
| 发布资格 | **NOT ELIGIBLE** | Release Lab/RC 脚本不可用；跨宿主（Windows 10/macOS/Linux）、独立标准用户、远端 CI 与完整发布流水线未验证。不得以本地候选替代发布门禁 |

本次未执行任何 push、tag、release、远端分支或远端 CI 写操作。
