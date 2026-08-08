# C1.6 — Verification Toolchain Isolation

日期：2026-08-08。分支 `feat/coding-real-task-completion-c1`。

**结论前置：VERIFICATION_TOOLCHAIN_ISOLATION = PASS。** 根因不是"沙箱少给了写权限"，而是 **CodeLeveler 把宿主的编译缓存 wrapper 继承进沙箱，却只能给它一个随命令消亡的临时目录**；wrapper 启动的守护进程活得比该命令久，从此每次编译都失败。修复是**显式中和已知的纯缓存 wrapper**（配置 + 环境变量两条路径），沙箱边界一寸未放开。

## 1. Root Cause（逐条实测，非文本推断）

| # | 事实 | 证据 |
| --- | --- | --- |
| A | 宿主 `~/.cargo/config.toml` 被逐字节复制进沙箱 CARGO_HOME overlay | `sync_cargo_config`（host_cache.rs:398）读 `config`/`config.toml`/`credentials*` 并 `private.write(name, bytes)` |
| B | 因此 `rustc-wrapper = "sccache"` 进入沙箱 cargo | 本机 `~/.cargo/config.toml` 的 `[build] rustc-wrapper = "sccache"` |
| C | **sccache 守护进程继承了每命令的临时 scratch** | 活动进程实测：`TMPDIR=/Users/mengmian/.leveler/codeleveler-sandbox-nwrC2D/tmp`，而该目录**已被删除**（`SandboxPaths.scratch: TempDir` 随命令结束 drop） |
| D | TMPDIR 缺失时 sccache 硬失败 | `sccache: error: failed to start server process … No such file or directory at path "/definitely/missing/dir/sccacheHHv3h8"` |
| E | 孤儿守护进程使**后续每次编译**失败 | 故意制造后，本仓 `cargo check` 全线 `(exit status: 254)`；ripgrep 门禁记录 `sccache: error: Failed to create temp dir … No such file or directory` |
| F | **失败发生在 wrapper 的 server 侧**，client 环境正常也无济于事 | client 用健康 TMPDIR、server 持已删除 TMPDIR → 编译仍走该 server |

**失败的确切路径**：`$TMPDIR`（= 每命令 scratch 的 `tmp` 子目录）。不是 `SCCACHE_DIR`、不是 `XDG_CACHE_HOME`、不是 `HOME`、不是 Darwin 共享 temp。

**为什么是间歇性的**：是否失败取决于守护进程当时的状态（是否已被某个沙箱命令启动、其 scratch 是否已被回收）。这正好解释 ripgrep 的 acceptance 在多次运行间时过时不过。

## 2. 第二条来源（本轮最关键的发现）

只清理 overlay **不够**。Cargo 的配置发现会**从 cwd 向上遍历每一级 `.cargo/config.toml`**；工作区位于 `$HOME` 之下时，被清理掉的那个宿主文件**作为祖先目录配置照样生效**。

实测：overlay 已中和（沙箱内 `cat $CARGO_HOME/config.toml` 显示注释替换、`RUSTC_WRAPPER` 为空），而真实仓门禁**仍然**失败于
`process didn't exit successfully: `sccache /Users/…/rustc …` (exit status: 254)`。

因此修复必须同时覆盖**调用环境**：给子进程设空的 `RUSTC_WRAPPER`（Cargo 对该变量的优先级高于任何配置文件）。

## 3. Cargo Config 继承模型（§6 的回答）

现状 contract 是**"完全继承宿主 Cargo semantics"**（逐字节复制，只做大小与符号链接的安全检查）。本轮**不重构**这一模型，只补上一条明确例外：**已知的纯缓存 wrapper 不被继承**，并把原因写在沙箱配置文件里，让差异自解释而非神秘消失。

## 4. Chosen Fix — Strategy A/B 之中的窄化 B（含 Final Closure 的有效性判定）

```rust
/// 纯编译缓存 wrapper：哈希调用、命中则取缓存、否则 exec 真正的 rustc。
/// 去掉它改变的是构建速度，不是构建产物 —— 所以只有这些可以被中和。
const CACHE_ONLY_RUSTC_WRAPPERS: &[&str] = &["sccache", "cachepot"];
```

1. `sync_cargo_config` 复制配置时，把 `rustc-wrapper` / `rustc-workspace-wrapper` 中**值为已知纯缓存 wrapper** 的行替换为解释性注释（保留 `# original:` 原文），其余一字不动。
2. **按 Cargo 的真实优先级计算"有效 wrapper"**（`effective_rustc_wrapper`）：环境变量 → 从 workspace 逐级向上的最近一个 `.cargo/config(.toml)` → 最后才是 `$CARGO_HOME` 配置。**只有当有效值是已知纯缓存 wrapper 时**，才为子进程设 `RUSTC_WRAPPER=""` / `RUSTC_WORKSPACE_WRAPPER=""`（Cargo 对该变量的优先级高于任何配置文件）。
3. 名称识别兼容裸命令、两种路径分隔符与 `.exe` 后缀。配置读取器只认 `[build]` 表下的这两个标量（注释行、其它表下的同名键、显式空值都不算设置）。

**为什么必须是"有效值"而不是"任意来源出现过"**：外层目录写了 sccache、而更近一级把它换成带真实构建语义的未知 wrapper 时，Cargo 用的是**更近的那个**。若只要在任何一层看见 sccache 就清空环境变量，就会把用户真正生效的 wrapper 一并废掉。测试 C 正是锁这一点。

**Why not the alternatives**

- **Strategy A（给 sccache 一个稳定运行目录）**：失败点是守护进程持有的 `$TMPDIR`，而 C1.3 的契约要求每次检查的 temp root 是**每命令专属且用完即删**。要让 A 成立就得把 temp root 变成长期共享的，直接推翻 C1.3 的测试 G/H。sccache 也没有独立于 `TMPDIR` 的 server-temp 配置项。
- **Strategy C（修隔离环境、保留 wrapper）**：同上——问题不在权限，而在生命周期，隔离环境无法让一个已经存在的守护进程改用新目录。
- **泛化成"删除所有 rustc-wrapper"**：明确拒绝。wrapper 可能承担 instrumentation、交叉编译、自定义编译器语义；未知 wrapper **原样继承**（有测试锁定），即使它可能在沙箱内失败 —— 可见的失败好过被悄悄改变的构建语义。

## 5. Security Boundary（§4 / §11）

**没有放开任何写权限**：未开放 `/var/folders/<user>/T`、`~/.cache`、`HOME`、`~/.cargo`、系统共享 temp。可写集仍是 workspace + 每命令 scratch + Leveler 自有 tool cache。

C1.3 的四条契约测试原样通过：专属 temp root 在工作区外、三变量同值、每检查独立且用完即删、越权路径仍被拒；`macos_mktemp_without_a_template_bypasses_tmpdir` 依旧钉着那条平台限制 —— **没有为了 sccache 反转 C1.3 的任何安全决定**。

## 6. Deterministic Fake-Wrapper Tests（不依赖机器安装 sccache）

`wrapper_fixture` 造一个**必定失败**的假 wrapper 脚本（打印 `wrapper: error: Failed to create temp dir` 并 exit 1），配上宿主 CARGO_HOME 配置，然后走**生产 verify 路径**跑 `cargo build`：

fixture 把假 wrapper 按 **Cargo 优先级分层**植入：workspace 自身 `.cargo/config.toml` > 外层祖先目录 > 宿主 `CARGO_HOME`。

| 编号 | 测试 | 布局 | 断言 |
| --- | --- | --- | --- |
| **A** | `an_ancestor_only_cache_wrapper_cannot_fail_a_verification_gate` | 仅外层祖先 = `sccache`（host 配置与 env **都没有**） | 构建**成功** ← 本轮补上的缺口 |
| **B** | `an_ancestor_only_unknown_wrapper_is_still_inherited` | 仅外层祖先 = `team-instrumenting-rustc` | 构建**失败**（wrapper 照常执行） |
| **C** | `a_nearer_unknown_wrapper_wins_over_an_outer_cache` | 外层 `sccache` + 近层未知 | 构建**失败**（不因外层见过 sccache 就清空） |
| **D** | `a_nearer_cache_wins_over_an_outer_unknown_wrapper` | 外层未知 + 近层 `sccache` | 构建**成功**（近层生效值是缓存） |
| — | `a_host_cargo_home_cache_wrapper_is_neutralized` | 仅宿主 CARGO_HOME = `sccache` | 构建**成功**（原始形态仍成立） |
| **E** | `a_workspace_without_any_wrapper_builds_normally` | 无任何 wrapper | 行为逐字不变 |
| **F** | `a_real_compile_error_still_fails_the_gate` | 外层 `sccache` + 源码写坏 | 门禁**仍然失败**（不吞编译错误） |
| — | 6 条单元测试 | — | 裸名/绝对路径/`.exe`/`rustc-workspace-wrapper` 识别；`[build]` 表外同名键、注释、显式空值不算设置；未知 wrapper 与无 wrapper 配置**逐字节不变** |

## 7. Real sccache Evidence（同机、同守护进程、同命令的 before/after）

制造孤儿守护进程（`TMPDIR=/tmp/c16orphan` 启动后删除该目录），对真实 ripgrep fixture 走**生产 Verifier 路径**跑 `cargo check --offline`：

| | 结果 |
| --- | --- |
| **Before**（env 覆盖尚未加入，仅清理 overlay） | `status=Failed` · 证据：`process didn't exit successfully: sccache …/rustc … (exit status: 254)` |
| **After**（配置 + env 双路径中和） | **`status=Passed`**，证据为空 |

守护进程实测环境 `TMPDIR=/tmp/c16orphan`（已删除），cache dir `~/Library/Caches/Mozilla.sccache`，server 模式（`Listening on address 127.0.0.1:4226`）。

该场景固化为 `probe_real_repo_gate_under_host_rustc_wrapper`（`#[ignore]`，编译 ripgrep 依赖树，按需 `--ignored` 运行）。

## 8. ripgrep 最小复验（§10）

未重跑 100 轮 Agent、未烧模型。直接以生产 `Verifier` 对 ripgrep fixture 执行与 seq 679 同类的 gate：**Passed**。

## 9. Regression

`cargo fmt --check` ✅ · `cargo check --workspace --all-targets` ✅ 0 error · `cargo test --workspace --no-fail-fast` ✅ 0 failed · execution sandbox 测试 ✅ · verifier 测试 ✅ · 真实 rustc-wrapper 门禁测试 ✅（1 passed）。

## 10. 残留缺口 —— 已在 Final Closure 中关闭

初版只在"宿主配置或环境变量命中"时才中和，因此**祖先目录独有**的缓存 wrapper 不会触发（测试 A 当时不存在：旧 fixture 无论是否植入祖先配置，都会先在宿主配置里写一份，env 覆盖可能是被宿主那份武装的）。现已改为按 Cargo 真实优先级计算有效 wrapper，测试 A/B/C/D 覆盖 ancestor-only 与两个方向的优先级。

仍然成立的边界：判定只看 `build.rustc-wrapper` / `build.rustc-workspace-wrapper` 两个标量与对应环境变量；不解析 `[target.*]`、不处理 `--config` 命令行覆盖，也不构建 Cargo 配置框架。

## 11. Localization Commitment（§12）

**本轮未实现**。C1.5A 已证明其因果（first edit 提前 35-44%、pre-edit 降低 38-46%、零 edit failure、零 repair 增量），记录为 **Validated Coding Efficiency Improvement**，留待 C2 / 后续能力优化，除非 C1 Final Gate 数据表明不做它就无法满足完成率门槛。

## 12. FINAL

**VERIFICATION_TOOLCHAIN_ISOLATION: PASS**

**C1 STATUS: READY_FOR_FINAL_GATE**
