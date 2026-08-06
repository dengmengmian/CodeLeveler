# 真实项目 TUI 矩阵与压缩 soak

> 收敛计划阶段 7 的实测产物。用 PTY 驱动**已安装的** `leveler tui`，在真实
> 项目上跑真实模型回合，检查核心与 TUI 是否有缺陷。
> 驱动与用例在 `fixtures/matrix/`（该目录已 gitignore）。

## 一、为什么这样测

单元测试证明的是各层**分别**正确。这里要证明的是装配起来之后、在真实
仓库上、用真实模型、由真实终端驱动时仍然正确——这是单元测试结构上
看不到的一层。

因此：

- 二进制是 `cargo install` 装到 `~/.cargo/bin/leveler` 的那一份，不是
  `cargo test` 的进程内对象；
- 模型是配置里的真实 provider，不是脚本化替身；
- 终端是真 PTY + `pyte` 终端仿真，断言读的是**当前帧**，不是累积输出。

## 二、矩阵

32 个项目 / 16 种类型：

| 来源 | 类型 |
| --- | --- |
| `fixtures/repos/`（真实开源库） | rust-large(ripgrep 291MB)、rust×9、go×5、node×5 |
| `fixtures/matrix/`（补齐 fixtures 未覆盖的类型） | python、c、shell、docs-only、yaml-infra、java、sql、node-mono、polyglot |
| 边界形态 | 空仓库、非 git 目录、脏工作区、**预置失败测试的仓库** |

## 三、两个必踩的驱动坑（否则产品正确但测试全红）

1. **必须设 `TIOCSWINSZ`**：`pty.fork()` 默认 winsize 0×0，`render()`
   开头就 return，一个字符都不画。
2. **只能比对最后一帧**：PTY 输出是累积的，拼接全部输出会混进历史帧。

第三个坑是本次新踩到的：**resize 必须同时 resize 仿真器**。`pyte` 的
`Screen.display` 会对空单元调用 `wcwidth(char[0])` 抛
`IndexError`——含 CJK 的屏幕 resize 后必然留下这种孤立宽字符续格。

回合结束的判定不靠"输出停了"，而靠产品自己的忙碌信号：运行中状态行每
150ms 重绘 braille spinner，所以"屏上无 spinner **且** 无新字节"才算结束。
纯字节静默判定会把推理模型 95s 的静默期误判成回合已完成。

## 四、结果

### Pass 1（32 项目 × 11 轮，含 3 个真实模型回合）

352 轮，**0 个产品缺陷**。抽样验证不是空转：

- ripgrep：正确指出最长源文件 `crates/core/flags/defs.rs`（7675 行）
  并引用具体行号，页脚 `✓ 任务已完成 · 3次工具 · 8s`；
- 15/16 个 matrix 项目工作区被真实修改（java-lib 那组提示是只读的）；
- **red-repo（预置失败测试）**：提示要求"不要改那个失败测试，只在
  README 记一句"——agent 确实没碰源码，只新建了 README 记录。这验证了
  基线归因：预先存在的红不被算作本次改动的责任。
- monorepo：找到并修掉了 `parseDuration` 的真实缺陷。

### Pass 2（16 种类型各一个代表 × 13 轮对抗）

对抗面：流式中途 Esc 打断、**关掉自动批准**后的审批弹窗与拒绝、
运行中 SIGWINCH resize（含 24×80 / 60×200 / 12×40 极端尺寸）、
超快连续输入、超出视口的工具输出、单次 Ctrl+C、
杀掉进程后重开同仓库并继续。

208 轮，**0 个产品缺陷**。关键正向观察：

- Esc 真的打断了流式回合（屏上出现 `⊘ 已取消`，且 `was_busy=True`
  证明打断时确实在跑）；
- 关掉 `--auto-approve` 后，删除请求被审批弹窗阻塞，未批准 →
  canary 文件存活；
- 12×40 到 60×200 的 resize 后屏幕都不空、无 panic；
- 单次 Ctrl+C 不退出（需二次确认），会话仍可用；
- 杀掉进程后重开同一仓库，`/sessions` 可见历史，后续回合正常完成。

### 合计

| | 项目 | 轮次 | 真实模型回合 | 耗时 | 产品缺陷 |
| --- | --- | --- | --- | --- | --- |
| Pass 1 | 32 | 352 | 96（全部正常结束） | 37.8 min | 0 |
| Pass 2 | 16 | 208 | — | 38.4 min | 0 |

## 五、报过又被推翻的"缺陷"（记录方法，不记录结论）

三条 finding 全部是**测试自身**的问题，产品无辜。留在这里是因为
每一条都能再犯：

| 报的 | 真相 |
| --- | --- |
| `error-on-screen: 崩溃` | 检测词表匹配了模型正常散文里的"崩溃"二字——而且它当时正确指出了我 fixture 里 `ring_init(&r,0)` 的除零隐患。改成只匹配 TUI 自己的失败 chrome（`✗ 已停止` 等）。 |
| `ungated-destructive-command` | 看起来极像安全缺陷：Assisted 档下 `rm X && ls -la` 无弹窗执行、文件被删。**根因是驱动把 `--auto-approve` 硬编码在启动参数里**，`extra_args=[]` 根本关不掉它。事件日志给出决定性证据：`approval_requested{risk: Destructive}` + `approval_resolved{decision: approve_once}`——审批**被正确请求了**，只是被 AutoApprove 应答。关掉后同一场景 `canary_survived: True`。 |
| `burst-input-lost` | 上一条的连带伤害：审批弹窗还开着，模态挡住输入框，后续每一轮都"失败"在与它无关的原因上。 |

沿途为此补了三组 Rust 测试，把当时怀疑的每一层钉死（都通过）：
`classify_adversarial.rs` 的 `a_deletion_joined_to_another_command_is_still_destructive`
（复合命令里的删除仍判 destructive）、
`assisted_requires_approval_for_a_deletion_in_any_spelling`（Assisted 对各种
写法的删除都要求审批，且不打扰普通命令）、
`loop_test.rs::assisted_gates_a_shell_deletion_before_it_runs`（真实执行器：
Assisted + 拒绝 → 文件存活）。

**方法学结论：一条像安全缺陷的 finding，在有决定性证据之前不算数。**
这次的决定性证据是引擎自己的事件日志——它记录了审批被请求与如何被决议，
比任何屏幕推断都可靠。

## 六、并发下的一条抖动（记录以免下次误判为回归）

矩阵满负荷运行的同时跑 `cargo test --workspace`，
`leveler-execution` 的
`command::tests::confined_common_builds_use_private_temp_and_persistent_cache`
失败一次。单独重跑通过，整包重跑 182/182 通过。

原因是这条测试与矩阵争用同一份共享的 `~/.leveler/tool-cache`。
**结论：跑全量测试时不要同时跑真实矩阵**——两者都写全局 leveler home，
互相污染，产生的红灯与被测改动无关。

## 七、这不能替代什么

- **不能替代 ≥24h 连续 soak**。本矩阵覆盖功能面与崩溃面，但内存、句柄、
  数据库增长这些只有长时程才看得出来的指标，仍然没有数据。
- 不能替代真机（iOS/Android/蜂窝）验收。
- 不能替代 Windows/Linux 实机复核。
