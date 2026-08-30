# Runtime Version Consistency

## 事故

一个已经修好一天的 bug 看起来复发了。

```
磁盘上的二进制被换成新构建
    ↓
8/28 11:03 启动的守护进程仍在跑（macOS 保留它原来的映像）
    ↓
14:01 新开的会话连上了那个守护进程
    ↓
两边都显示 v0.2.0-beta.1
```

`wait_task` 的 bounded-wait 修复（`36f2a547`，8/28 12:17 提交）比那个守护进程晚了
**73 分钟**。会话跑的是修复前的代码：`unwrap_or(600).max(1)`、`Err("wait timed out")`。
界面上完全看不出来——只有把 `strings` 输出和进程启动时间对起来才能确定。

**换掉磁盘上的文件，不等于正在跑的东西被更新了。**

## 复用了什么

这个闭环几乎没有新造零件：

| 已有 | 用途 |
| --- | --- |
| `run_cmds.rs::ensure_default_runtime` | 唯一的 bootstrap（TUI 与 CLI 共用），比对就插在"连上已有 daemon"之后 |
| `RuntimeInfo` | 已有 `version` / `pid` / `health`，加法字段扩展 |
| `RuntimeHealth` | 已有 `accepting_work` / `active_turns` / `shutting_down` |
| `BackgroundTaskRegistry` | 后台任务真值，补了一个 `alive_count()` |
| daemon 的 SIGTERM/SIGHUP graceful shutdown | 退休走同一条路径，不新造关闭机制 |
| `ActiveTurns` + `TurnAdmissionError` | 拒绝新活复用既有准入语义 |
| `leveler-cli/build.rs` | **移动**到 `leveler-core`，不复制 Git 管道 |

## BuildIdentity

```rust
BuildIdentity { version, revision, dirty }
```

住在 `leveler-core`，因为守护进程要上报**它自己**是什么构建——只报 `CARGO_PKG_VERSION`
的话，同版本不同代码永远分辨不出来，而那正是事故本身。

匹配规则刻意保守：

| 情况 | 结果 |
| --- | --- |
| 干净 A vs 干净 A | 匹配 |
| 干净 A vs 干净 B（同 version） | **不匹配** ← 事故这一类 |
| 干净 A vs 脏 A | **不匹配** |
| 脏 A vs 脏 A | **不匹配** |
| 未上报身份（老 daemon） | **Unknown**，绝不读成"相同" |

脏构建不被它的 commit 标识：同一 commit 的两个脏树可以是任意不同的代码。

## 退休：一个机制覆盖两种情况

```
ShutdownWhenIdle { reason }   # BuildMismatch | UpdateReady（预留给 Updater）
    ↓
accepting_work = false        # 先停收活，否则永远等不到 idle
    ↓
已在跑的活继续跑，不取消、不 kill
    ↓
排空后进程自己退出（走既有 graceful shutdown 令牌）
```

客户端**不 kill、不轮询**：它只发请求，然后等 socket 消失。排空由 runtime 主导，
因为只有它知道"还有活"是什么意思。

### 安全退休判据（不要简写成"idle"）

```
RUNTIME_SAFE_TO_RESTART_PREDICATE =
    活跃 turn 数 == 0            （既有 ActiveTurns 真值）
    AND
    存活后台任务数 == 0          （既有 BackgroundTaskRegistry 真值）
```

**只看 turn 是不够的**：那次 `cargo check` 活过它的 turn 半小时，按"无 turn 即闲置"
就会在它头上结束进程，把活扔掉。

## 验证期发现并修掉的缺陷

`shutting_down` 原本**只到达健康上报**，准入路径不看它。退休中的 runtime 嘴上说不收活、
实际照收 → 新 turn 不断进来 → 永远到不了 idle → **替换永远不会发生**。

修法：把同一个 `Arc<AtomicBool>` 交给本来就掌管准入的 `ActiveTurns`，加
`TurnAdmissionError::Retiring`。一处上报、一处执行，而不是两套"是否在收尾"的概念。
提交：`1b93bde4982d`。

## bootstrap

```
连上已有 runtime
    ↓
Current   → 静默复用（绝大多数情况，零开销）
Unknown   → 不动它，报可执行的错误（可能正握着活）
Outdated  → ShutdownWhenIdle → 等它走 → 起新的 → 重新握手校验身份
```

替换后**必须**证明自己是期望的构建，**一次**，不重试——反复起回错误构建只该报告，不该成环。

远程客户端被拒绝：手机和笔记本的构建差异是兼容性问题，不是结束这台机器上活的权限。

## 正常 UI 不变

不显示 SHA、PID、启动时间。诊断信息走既有 `/status`。普通用户不该需要理解 inode。

## 未来 Updater 的接缝

```
安装新产物 → ShutdownWhenIdle(UpdateReady) → 排空 → 退出 → 起新的 → 校验 BuildIdentity
```

后半段的生命周期已经在这里了，Updater 只需要补前半段（发现、下载、校验、安装）。

**更新完成的定义**（现在就写死，避免重演事故）：不是"磁盘上的文件换了"，而是
**运行中的 runtime 的 BuildIdentity == 期望的 BuildIdentity**。

本次不实现：发现、下载、安装、回滚。

## 证据状态

确定性测试见 `RUNTIME_VERSION_CONSISTENCY_CLOSURE` 的机器可读门。
**macOS A/B 真实 dogfood 未跑**，因此闭环状态是 `PENDING_REAL_DOGFOOD`，不是 PASS。
