# CodeLeveler Windows 兼容性与 TUI 质量验收报告

日期：2026-11-14  
分支：`windows/tui-quality`  
代码提交：`b1f002d`（专项基线 `99fef72`，`1.0.26`）  
收口提交：`5a4fc17`（最终候选，见 §9）  
最终候选：`windows-audit/candidate/closure/`（历史候选 `candidate/final/` 保留）  
结论：**Windows 10 已测试环境通过；Windows 11 未验证；标准用户、macOS/Linux 真实终端未验证。收口轮见 §9。**

## 1. 结论摘要

| 项目 | 结果 | 证据 |
| --- | --- | --- |
| Windows 10 Education 19045 + Windows Terminal 1.25 + PowerShell/CMD | PASS | W08、W10–W13 截图；真实模型 dogfood |
| Windows 10 + Console Host + PowerShell/CMD | PASS（有宿主字体限制） | W01/W08 截图；ConHost 字体缺 `⑂` 属宿主限制 |
| W01 中文缺字 | 已修复测试到的 Console Host 空闲提示缺 `文`；根因为 `·` 在 ConHost 中被按宽字形绘制 | `evidence/final/w01-conhost-fixed.png` |
| W02 Composer/状态/footer 布局 | 通过 80×24、100×30、120×30、130×30、最大化实测 | `evidence/final/w06-*.png` |
| W03 欢迎形象回退 | 无原生图片时不再绘制块状字符画；WT Sixel 显示 3D 吉祥物；ConHost/80×24 正确隐藏 | `evidence/final/welcome-sixel.png`、`w06-80x24.png` |
| W04 友好路径 | 仅在展示层去掉 `\\?\`，操作路径不变；doctor/TUI 实测 | `evidence/logs` 与 doctor 输出 |
| W05 字体/颜色/对比度 | truecolor 渲染正常；CJK 字体回退由宿主控制 | W07/W08 截图 |
| W06 resize/滚动/残影 | 实测 120×30→80×24→100×30→130×30→最大化→120×30；无可见残影 | `evidence/logs/w06-resize-events.log` |
| W07 输入/IME/快捷键/粘贴 | Unicode、ZWJ、IME 候选、Alt+V 多行粘贴均通过；原生 Ctrl+V 限制已记录 | `evidence/final/w07-*.png` |
| W09 中文进程输出 | GBK/CP936 输出与 cmd 中文错误正确解码 | `evidence/final/w09-*.png` |
| Goal/Plan/Steer/取消/恢复 | 真实模型 dogfood A–D 通过 | `evidence/dogfood/` |
| Windows 11 | **NOT VERIFIED** | 本次环境无 Windows 11 |
| 标准用户账户 | **NOT VERIFIED**（已测试 Medium integrity 非提升会话） | `evidence/env/environment.txt` |
| macOS/Linux 真实终端 | **NOT VERIFIED** | 本机无对应宿主 |

## 2. 环境与范围

| 项 | 值 |
| --- | --- |
| OS | Windows 10 Education 10.0.19045，build 19045 |
| 主机 | LENOVO 20UFA008CD |
| Windows Terminal | 1.25.3131.0 |
| Console Host | conhost 10.0.19041.4522 |
| PowerShell | 5.1.19041.5486 |
| CMD | 10.0.19045.5487 |
| Rust | rustc 1.90.0 (1159e78c4 2025-09-14) |
| Cargo | 1.90.0 (840b83a10 2025-07-30) |
| Windows 工具链 | MinGW-Builds GCC 16.2.0 (x86_64-posix-seh-rev1) |
| 显示 | 1920×1080，150%（144 dpi 物理） |
| 代码页 | console CP 65001；OEM/ACP 936（中文 Windows） |
| 完整性级别 | Medium，`admin=False`，Administrators 组为 deny-only（非提升） |
| 模型 | `deepseek/deepseek-v4.1-flash-1017`，OpenAI-compatible relay；密钥未记录 |

约束：

- 未修改系统区域、代码页、字体、注册表、安全策略、终端设置或用户账户。
- 未修改 Runtime/Agent/Provider/Goal/Session 的核心语义；改动限于展示、编码解码、宿主适配和对应测试/文档。
- 未 push、未打 tag、未发布、未修改远端。

## 3. 基线、根因与修复

### 3.1 W01：Console Host 空闲提示缺 `文`

**基线现象**：Windows Terminal 中空闲提示完整；Console Host 中 `@ 文件` 显示为 `@ 件`，且仅长样式行出现。系统字体与原生 `Write-Host "@ 文件"` 均正常，排除字体缺字。

**根因**：独立 ratatui/crossterm ConHost 探针复现：

| 探针 | 结果 |
| --- | --- |
| 无样式长提示 | 完整 |
| 有样式长提示，含 U+00B7 `·` | 缺 `文` |
| 将分隔符改为 `\|` | 完整 |
| 仅替换第一个 `·` | 完整 |
| 原生 `Write-Host` | 完整 |

ConHost 将 U+00B7 按宽字形绘制，但布局只计 1 列；长样式行累计偏移后使后续 CJK 字形被空格覆盖。不是 CodeLeveler 的 Unicode 宽度计算错误，也不是字体缺失。

**修复**：Windows 空闲提示使用 ` | ` 作为分隔符；非 Windows 保持 ` · `。新增 Windows 提示断言。

```text
/ 命令 | ! Shell | @ 文件 | Shift+Tab 权限 | Alt+V 粘贴 | Ctrl+? 快捷键
```

**验证**：`evidence/final/w01-conhost-fixed.png` 中 `@ 文件` 完整；WT 仍使用 `·`。修复前对照见 `evidence/final/w01-conhost-before.png`；独立探针矩阵/分隔符/原生文本见 `evidence/final/w01-probe-style-matrix.png`、`w01-probe-separators.png`、`w01-native-text.png`。

**剩余限制**：ConHost 默认字体不含 U+2442 `⑂`，分支符号显示为空白/豆腐块，但分支文本可见；未修改宿主字体。

### 3.2 W03：欢迎页吉祥物与回退

**基线**：无原生图片能力时仍绘制 `█/▀` 块状字符画，在窄终端和 ConHost 中观感差，且占用内容高度。

**修复**：

- 删除 `brand.rs` 与块状字符画回退；无原生图片时欢迎页只显示文本并回收列宽。
- Windows Terminal 通过 `WT_SESSION` + DA1 `ESC[?61;4;...c` 双重确认 Sixel；仅两者同时成立才使用 Sixel。
- DA1 探针有界（150 ms），启动时仅在有键盘事件时退避；图片源最大缩放到 512 px，降低启动编码成本。
- Sixel 清理不越界发送到不支持的主机。

**验证**：WT 显示 Sixel 3D 吉祥物（`evidence/final/welcome-sixel.png`）；ConHost 隐藏图片；80×24 因内容高度不足隐藏图片且不留块状占位（`evidence/final/w06-80x24.png`）；100×30/130×30/最大化显示图片。

### 3.3 W04：友好路径展示

**根因**：Windows canonicalize 产生 `\\?\C:\...`、`\\?\UNC\...`、`\\?\Volume{...}`。直接展示对用户不友好，但操作路径不能改。

**修复**：新增 `leveler-core::path::display_path`：

- `\\?\C:\...` → `C:\...`
- `\\?\UNC\server\share` → `\\server\share`
- `\\?\Volume{...}`、`\\?\GLOBALROOT...`、不完整路径保持原样
- 非 Windows 原样返回

接入 doctor 的 repo/config/crash 路径、TUI 状态行、外部会话 cwd、工具摘要、完整输出错误、skills/problem 位置、导出 toast。

**验证**：`leveler doctor` 实际输出 `repo: C:\...`、`config: C:\...`；操作路径未变；单元测试覆盖 drive、UNC、device、incomplete、普通路径。

**观察（未作为 W08 依据）**：CLI `doctor` 在该测试 shell 的默认 sandbox 工具探测中报告 `git`/`cargo` 不在 PATH，而 `where.exe` 能看到两者；TUI full-access 工具执行与 dogfood 中的 cargo/git 调用均成功。该差异疑似 low-integrity sandbox 探测，未在本次 TUI 修复范围内继续定位；原始输出见 `evidence/logs/final-doctor-chinese.txt`。

### 3.4 W05：字体、颜色与对比度

- Windows Terminal truecolor 渐变/强调色正常；高对比度和 CJK 字形回退由宿主控制。
- Console Host 使用宿主默认字体；`⑂` 字形缺失属于字体覆盖限制，不是颜色问题。
- 未修改主题、字体或终端配置。

### 3.5 W06：resize、滚动与残影

实测序列与实际 TUI 尺寸日志：

```text
120x30 → 80x24 → 100x30 → 130x30 → 133x33(maximized) → 120x30
```

- TUI 日志确认收到的真实 cells 尺寸，非窗口像素近似；日志同时包含窗口过渡产生的短暂 `46x2`/`78x23`/`134x36` 事件，TUI 未崩溃、未留下残影，目标尺寸均到达。
- 80×24 下布局压缩、mascot 隐藏、footer 不溢出。
- 最大化后回到 120×30 无可见残影；未观察到 scroll ghosting。
- `PageUp/PageDown` 按半屏滚动，窗口可回到 live bottom；隐藏行计数显示正确。

证据：`evidence/final/w06-*.png`、`evidence/logs/w06-resize-events.log`。

### 3.6 W07：输入、IME、快捷键与粘贴

| 场景 | 结果 |
| --- | --- |
| 中文/emoji/ZWJ/组合字符/VS16 | 输入与删除正确；ZWJ 作为一个 grapheme 删除 |
| Sogou IME 候选 | `ni'hao` 候选提交为 `你好` |
| Shift+Tab | request-approval → assisted → full 循环 |
| Alt+1/2/3 | 历史会话 / 帮助 / 模型选择 |
| Ctrl+C | 第一次提示，第二次退出 |
| Windows Terminal 原生 Ctrl+V | 宿主限制：换行变 Enter、非 BMP emoji 变 `?`；剪贴板本身正确 |
| Alt+V | 应用层读取剪贴板，多行与非 BMP emoji 完整保留；12 行粘贴折叠为 `[Pasted: 12 lines]` |

**修复**：Windows-only `Alt+V` 分支，仅在 Conversation/Input、无 overlay/search/update 时生效；插入后刷新文件候选；新增 i18n 与 USAGE 文档；composer 增加多行 emoji 粘贴测试。

**说明**：原生 Ctrl+V 的限制属于 Windows 终端把按键转换为控制台输入，不是 CodeLeveler 的剪贴板读取；产品提供 Alt+V 作为可验证替代。

### 3.7 W09：Windows 控制台代码页解码

**根因**：`cmd.exe`/旧控制台程序在中文 Windows 输出 CP936/GBK；按 UTF-8 lossy 解码产生 `�`/`◆`。

**修复**：新增 `leveler-execution::decode_output`：

- 合法 UTF-8 原样返回；
- Windows 非法 UTF-8 使用 `GetConsoleOutputCP()`；当其为 0/65001 时回退 `GetOEMCP()`；
- `MultiByteToWideChar` 两阶段解码；失败才 lossy；
- 非 Windows 保持 lossy UTF-8。

接入：

- 模型工具实时输出与最终 head/tail（`command.rs`）
- 后台任务日志（`background.rs`）
- 用户 shell 完成捕获（`user_shell.rs`）
- TUI 完整输出：优先 `combined.utf8.log`，回退原始 `combined.log`；原始文件仍是权威。

**验证**：真实 TUI 中 `echo 中文测试` 显示 `中文测试`；cmd 中文错误 `'!powershell' 不是内部或外部命令...` 正确；后台任务日志无乱码。

## 4. 真实模型 Dogfood

### 4.1 A：ledger 多文件修复 + CLI + 测试 + diff

- 会话：`d7f63991-026b-42ae-9c4d-0480e403c5b8`
- 结果：59 s，10 轮，22 次工具，TUI 计 209,369 tokens；session 计 210,796。
- 工具：5 次 read、3 次 edit、2 次 run_command、8 次 shell_command（含预期失败路径）、2 次 update_goal。
- 模型先提交 Goal，Plan 后完成；首次目标收尾因“条件尚未全部满足”被运行时拦截，模型修正 R3 后重新提交成功。这是运行时不接受模型自述作为独立验收的证据。
- 独立验证：`cargo test` 6 passed；`cargo run --bin report -- alice bob 30` → `from=70 to=50` exit 0；同账户/非正金额/余额不足均 stderr + exit 1；`tests/transfer_contract.rs` 未修改。

### 4.2 B：inventory Goal、5 步 Plan、Steer、澄清

- 会话：`31dbe065-cee7-4e26-98b9-9762f8c0ba23`
- 结果：2 m 47 s，19 轮，33 次工具，499,943 tokens。
- `/goal` 启动后出现专用 Plan 行：`计划 2/5` … `计划 5/5 已完成`。
- 运行中通过普通 Enter 发送 Steer；UI 显示“已保存 · 等待投递”，模型回复“收到补充要求，我记入范围”，随后额外运行 `cargo test -- --test-threads=1` 并列出测试名。
- 遇到 `cargo fmt --check` 只剩既有测试文件格式差异时，模型通过澄清面板询问；选择“保持测试文件原样”，运行时记录“用户已回答 Agent 的问题”。
- 最终状态为 **已完成，但有警告**：运行时观察到 `cargo test` exit 0，但未获得结构化测试结果，故不把“命令成功”当作“测试覆盖”；`cargo fmt --check` 与中文总结同样标记为模型自述、未机械验证。
- 独立验证：`cargo test` 6 passed；`cargo test -- --test-threads=1` 6 passed；`git diff --stat` 仅 `src/lib.rs` 75+/5-；`tests/feature_contract.rs` 未修改；`cargo fmt --check` 仅该测试文件既有差异。

### 4.3 C：Goal 取消、退出、恢复、继续

- 会话：`efe53fa9-ca34-4d33-b35e-28608adb7f15`
- 取消：`/goal` 运行 11 s 后 Esc，界面显示 `⊘ /goal 已暂停`、`已取消，可继续输入`，记录 1 轮/4 工具。
- 退出：Ctrl+C 两次确认后进程退出。
- 恢复：`leveler --session efe53fa9...` 重新打开；显示取消 recap 与下一步建议 ghost；发送“继续完成 CLI 任务，先说明当前进度”后 Goal 恢复执行。
- 继续完成：46 s，6 轮，8 工具，136,183 tokens；新增 `src/bin/inventory_cli.rs` 与 14 个 CLI 契约测试。
- 独立验证：`cargo test` 20 passed（lib 2 + cli 14 + feature 4）。
- 最终运行时警告：恢复后的 Goal 没有记录 owed requirement，无法机械验证声明交付物；如实标为未验证。

### 4.4 D：中文/空格路径

- 仓库：`...\windows-audit\dogfood\中文 项目`
- 会话：`78664410-d97b-432d-b1f5-a2feeb75e760`（状态：已回答）
- 结果：33 s，8 轮，11 工具，158,706 tokens。
- TUI 标题完整显示 `中文 项目`；读取 `数据 示例.txt` 通过 `Get-Content -LiteralPath '数据 示例.txt'` 正常，内容 `bolt,10 / nut,2` 无乱码。
- 模型修复 `formal_greeting` 为 `您好，{name}`，更新 `说明 文档.md` 勾选 TODO 并补充示例；未改测试。
- 独立验证：默认 target dir 下 `cargo test` **失败**（MinGW ld 对中文/空格路径中的 object 文件报 `No such file or directory`，exit 101）；改用无空格 `--target-dir` 后 2 tests passed。属于工具链/路径限制，不是 CodeLeveler 文件 I/O 失败；报告记录该限制与 workaround。

## 5. 代码改动与测试

### 5.1 改动文件

| 区域 | 文件 | 目的 |
| --- | --- | --- |
| 路径展示 | `crates/leveler-core/src/path.rs`，`leveler-core/src/lib.rs` | `display_path`，操作路径不变 |
| 编码 | `crates/leveler-execution/src/encoding.rs`，`Cargo.toml`，`command.rs`，`background.rs`，`lib.rs` | Windows 控制台代码页解码 |
| 用户 shell | `crates/leveler-app/src/user_shell.rs` | 完成捕获解码 + UTF-8 sidecar |
| TUI 欢迎 | `splash.rs`，`splash/native.rs`，`run.rs`，删除 `brand.rs` | 无块状回退；WT Sixel；有界探针/降采样 |
| TUI 输入/提示 | `reducer/mod.rs`，`composer.rs`，`i18n.rs`，`render/footer.rs` | Alt+V；Windows 提示分隔符；多行 emoji 测试 |
| TUI 展示 | `status_line.rs`，`render/screens.rs`，`tool_cell.rs`，`shell_render.rs`，`skills_view.rs`，`reducer/submit.rs` | 友好路径；解码输出 |
| 文档 | `docs/USAGE*.md` | 原生粘贴限制与 Alt+V |

### 5.2 测试结果

| 范围 | 结果 |
| --- | --- |
| `cargo test -p leveler-tui --lib` | **1630 passed, 0 failed, 23 ignored** |
| `cargo test -p leveler-app --lib user_shell` | **11 passed** |
| `cargo test -p leveler-app --test execution_ownership user_shell` | **2 failed**（Windows 平台：Assisted + FS sandbox unsupported；`exit_code=None`） |
| `cargo test -p leveler-execution --lib encoding::` | **2 passed** |
| `cargo check -p leveler-core -p leveler-execution -p leveler-app -p leveler-cli -p leveler-tui` | pass |
| `cargo fmt -p ... -- --check` | pass |
| 最终 release build | `cargo build --release --locked -p leveler-cli -p leveler-win-confine`，exit 0，14m51s（首次）；提交后重建见候选包 |
| 已知警告 | `leveler-execution/src/execution_host.rs:2184` 既有 unused `exited`；本次未触达 |

**关于两个 app 集成测试失败**：`execution_ownership` 的用户 shell 用例使用 `PermissionProfile::Assisted`。Windows 上 FS sandbox 能力为 `unsupported`，运行时按契约拒绝非 FullAccess spawn：

```text
SandboxPolicy("Windows FS sandbox is not available (capability write=unsupported);
refuse non-FullAccess spawn with write_root=... Use FullAccess explicitly...")
```

运行时的拒绝是正确行为（不静默降权）；TUI 的 FullAccess 用户 shell 与 dogfood 已通过。该失败路径在未改动的 sandbox capability/测试配置中触发，不是本次 TUI/编码 diff 的直接路径；未在基线复跑，未修改产品权限模型或测试断言。报告把它列为 Windows 测试套件平台缺口，而不是 TUI 回归通过项。

> **收口轮更新（见 §9.1）**：该失败已在该测试环境真实原因上复现并分类——测试环境未构建 `leveler-confine.exe`，且用例使用了 Windows 不存在的 `sleep 30`。CI 的 Windows job 会先构建 launcher；在同样前置条件下，`execution_ownership` 12/12（随后新增切换用例后 13/13）与 `user_shell` 15/15 通过。测试契约已按真实主机能力补齐，没有 skip、删除断言或统一切换 FullAccess。

## 6. 最终候选与性能

候选包目录：`windows-audit/candidate/final/`（审计工作区，不提交仓库）。

> 收口轮已重建新候选 `1.0.26 (5a4fc17480a6)`，见 §9.5/§9.8；本节保留 `b1f002d` 历史候选与性能对照，不改写当时事实。

| 项 | 值 |
| --- | --- |
| `leveler.exe` size | 65,718,784 bytes (62.7 MiB) |
| `leveler.exe` SHA-256 | `EBC466872EB711CD4938F43A4D9F7AFFD9A80BFA03B54781ED79C32329D4F8F3` |
| `leveler-confine.exe` SHA-256 | `F7FBFFC81FCF2FD435332EFEDA864F9603E1B1682C1BEE2E78E33CDE7E61845F` |
| `--version` | `1.0.26 (b1f002d75863)` |
| ZIP | `leveler-1.0.26-windows-x64.zip`，SHA-256 `50B4973261BE7A8FEAB30528FF33865E620207FA97AB480C6FDA4A7F3FE581D2` |

**最终 release 真实模型冒烟**：用候选 `leveler.exe` 在 ledger 启动，真实 provider 执行 `请只回复 OK。` → `OK`，1 s，12,388 tokens；截图 `evidence/final/final-release-model.png`。这验证了最终候选的 TUI + provider 路径，而不仅是启动/版本。

启动时间（同一机器、同一尺寸 120×30、同一 audit home，`launch-case` 到首屏）：

| 二进制 | 样本 | 启动 ms |
| --- | --- | --- |
| baseline release (`99fef72`，保留候选) | 2 runs: 1244 / 761 ms | median 1003 ms |
| final release (`b1f002d`) | 3 runs: 907 / 922 / 850 ms | median 907 ms |
| final release + 恢复 205k-token 长会话 | 1 run: 825 ms | 825 ms |

空闲/长会话内存与 CPU：baseline WS 22.9 / 23.1 MB，private 8.6 / 8.9 MB；final WS 26.8 MB，private 11.1–11.4 MB；final + 长会话 WS 62.2 MB，private 42.6 MB；空闲 2 s CPU 0%（单核）。

**方法**：同一台机器、同一 120×30 窗口、同一 audit home；`launch-case` 以 TUI 首次设置标题为 first paint，启动时间从 TUI 进程创建到该时刻；final 含 Sixel 检测/图片路径。baseline 2 次、final 3 次、长会话 1 次；样本量小，读数用于发现数量级回归，不当作统计显著结论。空闲 CPU 为进程 CPU 时间在 2 s 内的增量（单核百分比）；内存为 TUI 进程 WorkingSet/Private。原始值见 `evidence/perf/release-startup.csv`、`final-long.csv`。

## 7. 未验证与剩余风险

1. **Windows 11：NOT VERIFIED**。没有 Windows 11 实机；不得把 Windows 10 结论外推。
2. **标准用户账户：NOT VERIFIED**。当前为 Medium integrity、`admin=False`、Administrators deny-only 的非提升会话；标准用户/受限令牌未单独验证。
3. **macOS/Linux 真实终端：NOT VERIFIED**。相关代码有 `cfg` 分支与非 Windows lossy 测试，但本机无宿主可运行；跨平台真实终端回归未完成。
4. **其他宿主**：VS Code 集成终端、Tabby、ConEmu、SSH/PTY 未验证。
5. **App 集成测试平台缺口**：`execution_ownership` 的用户 shell 用例在 Windows + Assisted 下因 FS sandbox `unsupported` 被运行时拒绝（正确拒绝，不静默降权）；该测试套件未按 Windows 平台条件跳过/改写，TUI FullAccess 用户 shell 已验证。未在基线复跑，未改产品权限模型。
5. **ConHost 宿主限制**：默认字体缺 `⑂`；U+00B7 在长样式行被按宽字形绘制。已修复空闲提示的可复现案例，未声称修复所有 ConHost 字体/宽度问题。
6. **原生粘贴限制**：Windows Terminal 原生 Ctrl+V 会把换行转换为 Enter、非 BMP emoji 替换为 `?`；Alt+V 是应用层替代，不改变宿主行为。
7. **中文/空格仓库的 cargo 链接**：默认 target dir 下 MinGW ld 对路径中的中文/空格报找不到 object 文件；使用无空格 `--target-dir` 通过。属于工具链限制，CodeLeveler 的启动、文件读取/写入和 TUI 路径显示正常。
8. **模型输出波动**：真实 provider 单次 dogfood；不同模型/温度可能产生不同工具序列；运行时的“声明完成但有警告”是预期证据纪律，不是成功兜底。
9. **Windows 激活水印**：截图右下角水印为环境 artifact，不是 TUI 内容。
10. **版本身份**：最终 `--version` 以提交后重建为准；候选 ZIP/哈希与提交对应关系见上表。

**未运行的完整门禁**：完整 `./dev` gate、workspace-wide clippy/all-features、release-grade dogfood、macOS/Linux 真实终端未运行。受影响包的 fmt/check/tests 已通过；`cargo fmt --all` 在本机触发 Windows path-too-long `os error 206`，包级 `cargo fmt -p ... --check` 通过。

> **收口轮更新（见 §9.5）**：workspace fmt 逐包检查、all-features check、clippy、release build 与 `./dev verify` 已执行；`cargo fmt --all` 在长路径下因 290 个文件参数合计 42,065 字符超过 Windows 32,767 命令行长上限而报 206，使用 `git archive HEAD` 解压到 30 字符短路径后同一命令 **PASS**（junction 无效，cargo 会 canonicalize）。workspace 测试在 CI 等价环境中 7481 passed / 51 failed；51 条失败已逐类归因为宿主 Low token、缺失 Python、`/tmp` 不可写或 baseline 既有，不是本次改动路径。完整分类见 §9.5。

## 8. 复现与证据索引

启动测试 TUI：

```powershell
# 审计工作区脚本（不在仓库内）
windows-audit\scripts\run-tui.cmd <repo>
```

关键证据：

- W01/W03/W04/W06/W07/W09：`evidence/final/`
- 基线：`evidence/baseline/`
- Dogfood A–D：`evidence/dogfood/`
- resize 原始事件：`evidence/logs/w06-resize-events.log`
- 会话 A 摘要与 trace：`evidence/logs/session-a-show.txt`、`trace-a.json`
- 中文路径 cargo 对照：`evidence/logs/d-default-target.log`、`d-nospace-target.log`
- 环境：`evidence/env/environment.txt`
- 性能：`evidence/perf/`
- 收口轮（权限基线、编码、门禁、最终候选 Dogfood、连续运行）：`evidence/closure/`

所有截图为真实 Windows 窗口捕获，未使用 mock、脚本伪造或合成 UI。模型输出与运行时警告保留原文；独立验证结果见第 4 节。

## 9. 最终收口（2026-11-14）

> 本节记录对 `windows/tui-quality` 专项的最后一轮工程收口。第 1–8 节的事实与证据保留不动；本节只补充新执行的基线复现、权限契约、编码回归、最终候选 Dogfood、完整门禁和剩余边界。

### 9.1 两个 app 集成测试：基线复现与真实原因

基线（`b1f002d`，未改动工作区）在**未构建 `leveler-confine.exe` 的测试环境**中复现：

| 测试 | 结果 | 观测 |
| --- | --- | --- |
| `cargo test -p leveler-app --test execution_ownership user_shell` | 2 failed | `echo done` 返回 `failed`；`sleep 30` 返回 `failed`；运行时拒绝非 FullAccess spawn |
| `cargo test -p leveler-app --test user_shell` | 8 failed | 同一拒绝原因；`snapshot` 中的 `output_tail` 明确写出 `Windows FS sandbox is not available (capability write=unsupported)` |

根因不是 TUI/编码回归，而是**测试前置条件缺失**：

- Windows 上 `probe_sandbox_capabilities().write` 只有在 `leveler-confine.exe` 可解析时才是 `WriteRestricted`。
- 测试二进制位于 `.target/debug/deps`；`launcher_path()` 会检查其父目录 `.target/debug`。CI 的 Windows job 在测试前执行 `cargo build -p leveler-win-confine --all-features`，所以 CI 能找到 launcher；本机基线没有构建它。
- 第二个平台缺口是测试命令本身：`sleep 30` 在 Windows 上不是程序。改用 `leveler_test_support::sleep_shell_line(30)`（Windows 为 `ping -n 31 127.0.0.1`）。
- 测试使用 `PermissionProfile::Assisted` 创建会话，但 `Application::create_session` 持久化的是默认 Assisted；客户端默认值不能替代会话行。测试现在按场景使用 `create_session_with_mode`。

构建 debug launcher 后（CI 等价条件）：

| 测试 | 结果 |
| --- | --- |
| `execution_ownership`（Assisted，launcher 存在） | **12 passed**（CI 等价环境完整运行 13 passed，含新增切换用例） |
| `user_shell`（Assisted，launcher 存在） | **15 passed** |

在不支持写入后端的 Windows 环境中（临时移除 launcher），新增的契约测试断言：

- 拒绝是终态 `failed`，`exit_code=None`；
- 实时 stderr 明确包含缺失能力和 `FullAccess` 出路；
- 命令的副作用文件不存在（没有子进程启动）；
- 会话持久化模式仍是 `assisted`（没有静默降权）；
- 会话执行租约已释放。

证据：`evidence/closure/baseline-app-execution-ownership-user-shell.log`、`baseline-app-user-shell-suite.log`、`assisted-debug-launcher-*.log`、`unsupported-backend-refusal.log`、`unsupported-backend-switch.log`。

### 9.2 Windows 权限契约

| 项 | 结论 | 证据 |
| --- | --- | --- |
| 首次启动默认 | TUI 状态与新建会话均为 `Assisted`（产品默认） | `leveler-tui/src/state.rs`；`create_session` |
| 权限选择界面 | 三档：请求批准 / 替我审批 / 完全访问 | 现有 overlay，未改动 |
| 能力提示 | 权限 picker 不预读 OS 能力；`doctor` 报告 `write=Unsupported` 并给 Warn | `doctor` 输出 |
| 不支持时 | 运行时明确拒绝，不静默降权；错误现在同时进入实时视图与重连记录 | `interactive.rs`；契约测试 |
| 用户切换 | 显式 `SetPermissionProfile` 后可重试成功；切换带 observed version | `switching_to_full_access_makes_a_refused_shell_runnable` |
| FullAccess | 用户明确选择时无审批、无 sandbox 限制；用户 Shell、ownership、取消、转后台均正常 | `a_full_access_user_shell_moves_to_the_background_and_stops` |
| Assisted（有 launcher） | 真实低完整性写入限制；命令、ownership、取消正常 | `an_assisted_user_shell_matches_the_write_backend_capability` 的 supported 分支 |

**权限测试契约**（`execution_ownership.rs`）：

- `an_assisted_user_shell_matches_the_write_backend_capability`：同一测试在两种真实主机能力下都运行，不断言固定结果，而是断言**能力与结果一致**。
- `switching_to_full_access_makes_a_refused_shell_runnable`：拒绝后由用户显式切换，下一次调用真实执行。
- 其余 U1/U2/U3/U4 ownership 用例按 `user_shell_profile()` 选择：有后端用 Assisted，无后端显式 FullAccess；没有删除断言或统一切换掩盖失败。

### 9.3 输出编码专项回归

实现：`leveler-execution::encoding::OutputDecoder`。

- 每次读取先尝试合法 UTF-8；合法即原样输出。
- Windows 上非法 UTF-8 使用 `GetConsoleOutputCP()`；当其为 0/65001 时回退 `GetOEMCP()`；`MultiByteToWideChar` 两阶段解码。
- 跨分片：UTF-8 尾部不完整序列最多保留 3 字节；DBCS 尾随 lead byte 保留 1 字节，并用 `MB_ERR_INVALID_CHARS` 验证前缀；EOF 时 `finish()` 给出最终结果。
- 接入：模型工具实时/最终输出（`command.rs`）、后台日志（`background.rs`）、用户 Shell 实时输出（同一 reader）、用户 Shell 捕获 tail/decoded copy（`user_shell.rs`）、TUI 完整输出 pager（`output_view.rs` 优先 `combined.utf8.log`）。

覆盖与结果：

| 场景 | 结果 | 证据 |
| --- | --- | --- |
| 合法 UTF-8 原样 | PASS | `valid_utf8_is_unchanged`；`a_real_cmd_stream...` |
| CP936 中文（cmd `type` 原字节） | PASS | `gbk_bytes_decode_to_chinese`；`a_real_cmd_stream...` |
| 宿主 CP 65001 / 子进程 GBK | PASS（回退 OEM 936） | 同上；真实 Windows 控制台 |
| PowerShell 原生字节、跨分片 | PASS | `a_multibyte_character_split_across_pipe_reads_stays_one_character` |
| 分片 UTF-8 | PASS | `a_utf8_character_split_across_reads_reassembles` |
| 非法/不完整字节 | PASS（不吞后续文本） | `a_malformed_gbk_byte...`、`an_incomplete_gbk_character...`、`a_malformed_byte_does_not_swallow_following_output` |
| 长输出/截断头尾 | PASS | `a_large_gbk_capture_keeps_head_and_tail_readable`；截断 head 修复 |
| 后台日志 | PASS | `a_background_user_shell_log_decodes_gbk` |
| 用户 Shell 原始捕获 + 解码副本 | PASS | `a_gbk_capture_is_decoded_for_display_and_byte_exact_on_disk` |
| TUI 完整输出 | PASS | `capture_prefers_the_decoded_presentation_copy` |
| macOS/Linux 真实终端 | **NOT VERIFIED**（本机无宿主） | 非 Windows 分支保留 lossy，分片逻辑有跨平台单测 |

保存语义：原始 `stdout.log` / `stderr.log` / `combined.log` 保持逐字节精确；`combined.utf8.log` 只是展示副本。现有 `a_capped_shell_keeps_the_complete_stream_on_disk` 继续覆盖完整原始捕获。

### 9.4 最终候选真实业务 Dogfood

**此前 A–D 的身份核查**：四组 meta 均记录 `windows-audit/candidate/debug/leveler.exe`；该二进制 `--version` 为 `1.0.26 (99fef72af5a9-dirty) UNTRUSTED`，SHA-256 `788E...`。因此 A–D **不是** b1f002d 最终候选，不能作为最终候选业务验收证据；它们保留为专项过程证据。

**inventory “已完成，但有警告” 核查**：截图 `b-17-complete.png` 的运行时原文为：

- R4 `[tests]`：`cargo test` exit 0，但运行时没有结构化测试结果，所以测试覆盖仍未验证；
- R5 `[deliverable]`：`cargo fmt --check` 只有模型自述，没有机械检查；
- R8 `[content]`：中文总结未进入运行时证据。

这是**验证缺口/正常警告**，不是交付缺口：独立 `cargo test` 6 passed，serial 6 passed，测试文件未修改。

**新最终候选 Dogfood**（使用收口后的 release 候选 `1.0.26 (5a4fc17480a6)`）：

- 业务夹具：`windows-audit/dogfood/最终 账单`（中文+空格路径），多文件 Rust 项目，4 个失败契约测试；基线 `cargo test` 0 passed / 4 failed，CLI 输出 `合计: 1234 分`（固定 demo 值）。
- 真实 TUI（Windows Terminal，120×30，FullAccess 由 CLI 显式指定，真实 provider/model `deepseek/deepseek-v4.1-flash-1017`）。
- Session `21411d1e-e2ad-44c3-bf56-96fd2a3b36c3`，Goal `96fcfe6a-2957-4f63-a6e9-ce962331e488`。
- 结果：3 m 47 s（TUI），23 轮，61 次工具，921,437 tokens（TUI）/ 924,853（session）；9 步 Plan。
- 覆盖：多文件修改（6 个文件，144 insertions / 28 deletions）、中文/空格路径、`cargo test` stdout/stderr、Plan（1/8 → 6/8 → 9 步）、运行中 Steer、后台任务、独立业务测试、完成交付、回顾稳定展示。
- Steer 原文“请在「说明 文档.md」中明确记录 CSV 支持 BOM 和空行；最终总结里列出每个类别金额与总计；仍然不要修改测试文件”，TUI 显示“已进入当前任务”，最终文档确实写入 BOM/空行说明与金额表。
- 最终状态：**已完成，但有警告**。唯一警告是 R12 `[deliverable]`“用中文总结每个类别的金额、总计和测试结果”没有机械检查；这是运行时的证据纪律，不是交付失败。`※ 回顾` 显示“修复四模块与文档，测试 4/4 通过，合计 6118 分”。
- 后台任务：`!` 用户 Shell 成功转后台并产生真实终态；该 `ping` 命令在 agent harness 的网络沙箱中报告“传输失败”并 exit 1，产品如实显示失败（不是伪造成功）。连续运行中用不依赖网络的 `powershell Start-Sleep` 后台任务验证了自然完成/成功路径。
- 独立验证：`cargo test` 4 passed / 0 failed，exit 0；`cargo run --bin billing_cli -- "数据 示例.csv"` 输出 `图书: 3598 分 / 文具: 1500 分 / 食品: 1020 分 / 合计: 6118 分`，exit 0；缺失文件 stderr + exit 1；`git diff --stat` 6 files changed；`tests/billing_contract.rs` diff 0 行。
- 观察（非缺陷）：`git diff --stat` 把中文文件名显示为八进制转义（Git `core.quotepath` 默认行为），模型正确解释为“说明 文档.md”；不是 CodeLeveler 解码问题。
- 结果：**PASS（业务交付与独立验证）**，证据见 `evidence/closure/dogfood-final/`。

### 9.5 工程质量门禁

| 门禁 | 命令 | 结果 |
| --- | --- | --- |
| workspace 格式 | 逐包 `cargo fmt -p <pkg> -- --check`（35 包） | **PASS** |
| `cargo fmt --all` | 同机复现 + 短路径工作区 | **PASS（短路径）**：长真实路径下 290 个源文件参数合计 42,065 字符，超过 Windows 32,767 命令行长上限（os error 206）；`git archive HEAD` 解压到 30 字符路径后同一命令 exit 0。junction 无效（cargo 会 canonicalize 回长路径）；逐包检查也通过 |
| all-features 编译 | `cargo check --workspace --all-features --locked` | **PASS**（1m18s） |
| Workspace 测试 | CI 等价环境 + `RUST_TEST_THREADS=2` | **7481 passed / 51 failed / 97 ignored**；51 条失败记录对应 50 个 distinct tests（`parallel` 父用例会以子进程重跑同一 binary），全部归类为宿主/环境或 baseline 既有，见下表 |
| Clippy | `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | **PASS**（2m47s；先修复 `display_path` 既有 `needless_return`） |
| 仓库 dev gate | `cargo run -p xtask -- verify`（已 staged 受影响包） | **FAIL**：3 个 lib target 的失败与上表同一环境/baseline 簇；受影响集成测试均通过 |
| Windows release build | `cargo build --release --locked -p leveler-cli -p leveler-win-confine` | **PASS**（14m32s，干净树，`1.0.26 (5a4fc17480a6)`；无 `-dirty`） |
| Windows security canaries | `cargo test -p leveler-tools windows_` / `-p leveler-execution windows_` | **BLOCKED**：测试子进程为 Low integrity，ACL/低完整性 canary 需要 Medium |
| Release lab | `./dev release` | **NOT RUN**：本 checkout 没有外部实验室 `eval/` |

51 条失败记录（50 个 distinct tests；app parallel 父子进程各一条）的分类：

| 数量 | 位置 | 原因 | 对照 |
| --- | --- | --- | --- |
| 1 | `leveler-agent multi_agent_test` | Zero-scope child 的 MiC 边界在测试进程本身为 Low 时不可证明 | baseline `b1f002d` 同样失败；`child-token-integrity.txt` |
| 1 | `leveler-app --lib parallel` | 子进程 SQLite 无法打开临时 DB | baseline `b1f002d` 同样失败 |
| 3 | `leveler-cli parallel_collaboration_entry` | 同上 SQLite 临时 DB | 环境 |
| 2 | `leveler-cli eval_isolation` | 同上 SQLite 临时 DB | 环境 |
| 11 | `leveler-cli cli_surface_contract` | 测试写死 `/tmp`；Low 子进程不能写 Medium `C:\tmp` | 环境 |
| 8 + 5 | `leveler-cli schedule_show_projection` / `schedule_update_grant` | 同一 `/tmp` | 环境 |
| 11 | `leveler-execution --lib` | ACL/低完整性 canary 与 zero-scope 写入测试需要 Medium token | `child-token-integrity.txt` |
| 3 | `leveler-tui --lib` | 测试使用 `HOME` 构造 POSIX 路径；本次为完成 HOME 补全测试设置 Windows HOME | 同一二进制 `HOME` 未设置时 **1631 passed / 0 failed** |
| 1 | `leveler-tui shell_completion_paths` | 测试 HOME 为空目录，`~/` 无可列内容 | 填充 HOME 后 **17 passed** |
| 4 | `leveler-web --lib` | fixture 调用 `python3`；本机只有 Store 别名 stub | 环境 |

受影响范围（本次新增/修改）的实际结果：`execution_ownership` **13 passed**、`user_shell` **15 passed**、`leveler-tui --lib` 在未设置 HOME 时 **1631 passed**、`snapshots` 修正后通过、`leveler-tools --lib` **394 passed**、`leveler-agent task_commit_test` 在修正 `NoDefaultCurrentDirectoryInExePath` 后通过（整包 74 tests 中仅 multi-agent 环境用例仍失败）。

测试环境说明：Claude Code 的子进程以 **Low integrity** 运行；`%USERPROFILE%`、`C:\tmp` 等 Medium 路径不可写。为此使用工作区内的 `TMP/TEMP/LEVELER_HOME/HOME`，并补齐 MinGit 缺少的 `sleep`/`yes` 测试夹具。该限制与产品权限模型无关；不得把因环境失败的测试标为通过。

### 9.6 连续运行与资源

在 Windows Terminal、120×30、FullAccess 候选下连续运行约 **11 分钟**，两次进程（中途 Ctrl+C 退出后按 session 恢复）共 63 个 10 秒采样：

| 阶段 | 时长 | 样本 | WS MB | Private MB | 线程 / 句柄 |
| --- | --- | --- | --- | --- | --- |
| 首进程（19:45:28–19:49:09） | ~3m40s | 23 | 26.7–33.1 | 11.6–16.0 | 20–21 / 185–193 |
| 恢复进程（19:49:32–19:56:03） | ~6m30s | 40 | 27.6–29.1 | 11.3–12.7 | 22 / 193–197 |

- 长输出：`for /L %i in (1,1,3000)` 输出 3000 行，滚动后显示“上方还有 5056 行”，Home/End 回到底部，无残影。
- Resize：窗口像素 1752×936 → 1200×700 → 1600×900 → 1752×936；TUI 重排、footer 截断不溢出。ConPTY 下 `console-size.ps1` 读不到 cells（`console=x`），因此记录的是真实窗口像素与截图，不声称精确 cols/rows。
- 输入响应：tick-1…tick-5 每次即时执行；`!echo` 成功路径稳定。
- 后台状态：ping 失败任务与 PowerShell sleep 成功任务均正确结算；后台失败计数保留。
- Session 生命周期：`/new` 创建 `ebbf5edc-770f-4119-8630-344b6f0a1757`；退出后 `--session ebbf5edc...` 恢复，transcript 与输出完整。
- 内存无单调增长信号；恢复进程空闲 6.5 分钟 CPU 增量 0.5 s。样本量小，用于发现数量级/泄漏信号，不作统计结论。
- 证据：`evidence/closure/continuous/`（截图、CSV、resize 日志、launch.json）。

### 9.7 未验证环境

- Windows 11：**NOT VERIFIED**（无实机）。
- 标准用户账户：**NOT VERIFIED**。`net user` 只有 Administrator / CSDN / DefaultAccount / Guest / WDAGUtilityAccount；当前会话为 Medium、`admin=False`、Administrators deny-only。创建标准用户需要管理员/系统授权，本轮未执行。
- macOS/Linux 真实终端：**NOT VERIFIED**（无宿主，WSL 未安装）。
- 其他宿主（VS Code、Tabby、ConEmu、SSH/PTY）：NOT VERIFIED。
- Python 依赖的 web fixture 测试：**BLOCKED**（本机只有 Windows Store `python3` 别名 stub，无真实 Python）。
- 需要 Medium token 的 ACL / 低完整性 canary：**BLOCKED**（agent shell 的子进程为 Low integrity，见 `child-token-integrity.txt`）。
- `/tmp` 路径测试：**BLOCKED**（Low 子进程不能写 Medium `C:\tmp`）。

### 9.8 剩余问题与发布判断

**P0：无。**

**P1（发布证据缺口 / 未验证）：**

1. **Windows 11、标准用户、macOS/Linux 真实终端仍为 NOT VERIFIED。** Windows 10 的 TUI/宿主/权限结论不得外推。标准用户需要管理员创建账户；本轮未获得授权，也未修改系统。
2. **Windows 零写权限 child 的 shell mutation 边界未能机械证明。** `multi_agent_test::an_unclaimed_child_cannot_mutate_through_a_shell_command` 在 baseline 与本轮都失败；原因是 agent shell 的子进程为 Low integrity，测试工作区也是 Low，MiC write-up 边界不存在。正常 Medium 用户会话下该属性应由 `leveler-confine` 的低完整性 token 建立，但本轮没有 Medium 测试环境，不能声称已验证。需要 CI/普通宿主补跑该 canary。
3. **需要 Medium token 的 ACL / 低完整性 security canaries 未运行**（`windows_acl`、`windows_confine::windows_canaries`）。本机子进程 Low，无法设置/恢复标签。这是 Windows 安全门禁的明确缺口，不能标记通过。

**P2（宿主/展示/测试限制，不影响本次交付结论）：**

- 权限 picker 不预读 OS 能力；缺少 launcher 时不会提前提示，但 `doctor` 报 Warn，运行时拒绝已进入实时视图。
- ConHost 默认字体缺 `⑂`；U+00B7 在长样式行按宽字形绘制（已修可复现案例，不声称修复所有宿主字体问题）。
- Windows Terminal 原生 Ctrl+V 丢换行/非 BMP emoji；Alt+V 是应用层替代。
- Git `core.quotepath` 把中文路径显示为八进制转义；模型在 dogfood 中正确解释，不是解码失败。
- 中文/空格仓库在默认 target dir 下 MinGW ld 失败；使用无空格 target dir 通过（工具链限制）。
- ConPTY 下外部 `console-size.ps1` 读不到 cells；连续运行 resize 以真实窗口像素与截图记录，未声称精确 cols/rows。
- release build 有既有 `unused exited`（debug_assertions 外）警告；clippy dev all-targets 通过。
- 测试 teardown 偶发 “Tokio context is being shutdown” 噪声；不影响断言结果。
- 完整 `cargo fmt --all` 在本机受 Windows 命令行长限制阻塞；逐包检查通过。

**合并条件：**

- 此分支在 Windows 10 测试范围内满足合并的代码质量条件：**受影响测试全部通过、clippy 通过、workspace check 通过、候选构建干净**。
- 但 P1 中的未验证环境/安全 canary 必须由 CI（Linux/macOS/Windows 正常 Medium 宿主）或外部实验室补足；在这些证据出现前，不应把本分支的通过范围外推为全平台合并/发布资格。
- 从本机证据看，**可以合并到主分支进入 CI 验证**（代码改动与测试契约完整）；**不满足正式发布条件**（Windows 11、标准用户、macOS/Linux 未验证，release lab 未运行）。

**最终状态：**

- 代码提交：`5a4fc17480a63e14b29fa785778fcde92ca3b940`
- 候选：`1.0.26 (5a4fc17480a6)`，`leveler.exe` SHA-256 `CA1283E9...0800B9`，`leveler-confine.exe` `F7FB...845F`，ZIP `59AB...048E`
- 工作区：代码提交后干净；本轮文档/证据提交另记。
- 未 push、未打 tag、未发布；无远端变更。
