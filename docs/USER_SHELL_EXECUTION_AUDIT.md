# User Shell Execution (`!command`) — Audit

基线 `26f0569`。改前审计：十问实答 + 复用/并发/生命周期三项设计判定。
全部结论来自当前代码逐点核对（file:line 已验证）。

## 十问实答

| # | 问题 | 答案 |
| --- | --- | --- |
| 1 | shell 构造 | `shell_invocation`：Unix `sh -c`、Windows `cmd /C`（shell_command.rs:94-103，硬编码、无 $SHELL）。**发现重复**：authorization.rs:183-193 有第二份同映射（审批分类用），必须收敛 |
| 2 | spawn owner | `CommandRunner::run(ProcessRequest, CancellationToken)` 是唯一前台入口（command.rs:721）；BackgroundTaskRegistry 是**第二个** spawn 实现（env 逻辑复制粘贴、无 NO_COLOR 组、Windows 用 taskkill 非 Job 对等）——不可作为 User Shell 基座，会加固重复 |
| 3 | 环境 | `EnvSnapshot` + `env_clear` 重建（command.rs:1095-1131）：内建密钥 denylist + 后缀启发 + `deny_env`（provider api_key_env）+ allow_env 覆盖 + NO_COLOR 组；sandbox 缓存重定向（host_cache.rs:701）。无 proxy 特殊处理（直传） |
| 4 | cwd | `workspace.resolve(rel)`（写语义、防符号链接逃逸、敏感路径检查），默认 `"."` = repo root |
| 5 | 进程树取消 | Unix `process_group(0)` + PDEATHSIG(Linux) + `killpg SIGTERM→100ms→SIGKILL` + 2s 有界等待 + 退出后 reap 孤儿；Windows JobObject + KillOnDrop；测试覆盖孙进程（command.rs:3276/3324） |
| 6 | 权限/沙箱 | `confines_workspace() && !unrestricted_fs` → write_root + FilesystemIntent；deny_network → seatbelt/bwrap `--unshare-net`；.git 写保护；fail-closed intent 门先于任何 spawn |
| 7 | guard | 有 hang guard（`refuse_shell_script`：`&`/nohup/start/凭据文件/#吞命令），**无 interactive/TTY guard**；实际缓解 = `stdin(null)` + timeout。无 PTY 概念 |
| 8 | 流式? | **前台全缓冲**（`read_capped` 到进程退出才返回）；background 流入 256KiB 合并 log（stdout/stderr 不可分、无增量读 API、轮询式）。唯一实时信号是 CommandProgress 心跳（3s，label+elapsed，无字节） |
| 9 | 生命周期 owner | BackgroundTaskRegistry（进程级、cap 4、终态保留 64、id=bg-N）；前台无 owner（调用即所有） |
| 10 | EventLog | **支持 session 级事实**：`append(event, turn_id: Option, forward)`，`None`=无 turn 归属；leveler-app 有先例（parallel.rs 带 OwnershipToken 追加 TaskStarted）。persist-before-forward、transient 不落库由 append 统一处理 |

关键协议/UI 事实：

- `BackgroundTaskStarted/Exited` 是**死变体**（全仓无发射点）——只作形状先例。
- `spawn_btw`（interactive.rs:788）是非 turn 长操作的完整先例：events_for + 自有
  lifecycle 三连（BtwStarted/Delta/Completed/Failed），不占 ActiveTurns。
- `ActiveTurns` 单会话单槽（admit→token/finish/cancel）；`admit_context_op` 已示范
  非 turn 工作复用该槽 —— **恰好实现 §10 要求的 Idle/AgentTurn/UserShell 互斥**。
- TUI：slash 路径已示范"入输入历史、不入对话转录"（composer.take() vs
  push_user_if_new 分离）；`SubAgentBlock` 是非 Tool 可展开块的先例（两个 toggle
  fn 都要加臂）；Tools screen 是 master/detail 模板；净化链
  `sanitize_terminal_output`（ANSI/OSC 剥离）+ 渲染层 line sanitizer 齐备。
- 新 ClientCommand 的全部强制触达面（都是穷举 match）：session_id()/roundtrip/
  schema/remote policy(Deny 组)/bridge command_kind/interactive send/protocol.ts。
- Snapshot 附加字段先例齐全（`#[serde(default)]`），golden 测试按字面构造会编译失败
  提醒更新。
- Web：controller default 臂吞未知类型，新事件零运行时改动；只更新 TS union；
  **不得**把 shell 事件加进 TURN_TERMINAL_TYPES（会误触发消息队列 flush）。

## 设计判定

### D1 共享执行基座（§4 禁第二 executor）

`CommandRunner` 增加**流式入口**（同一 ProcessRequest/sandbox/env/树杀路径，
`read_capped` 加可选 chunk 观察者；现有 `run` = 无观察者特例）。User Shell 与
agent shell 汇于同一 host 执行边界：

```
User Shell ─┐
            ├─→ shell_invocation（收敛为 leveler-execution 单份，authorization 复用）
Agent shell ┘        ↓
              ProcessRequest（同 env/sandbox/cwd/树杀策略）
                     ↓
              CommandRunner::run / run_streaming
```

不用 BackgroundTaskRegistry（合并流/无增量读/复制的 env 逻辑/taskkill 非对等）。

### D2 并发策略（§10）

复用 `ActiveTurns` 槽位（admit_context_op 模式）：Idle/AgentTurn/UserShell 三态
互斥天然成立，无新队列。busy 时 `!cmd` 被拒并本地化提示（TUI 先行提示 + runtime
admit 双保险）。`user_shells: UserShellStore`（新小服务，仿 CheckpointStore）持
per-session 活跃执行（UserShellId + cancel token）+ 有界完成历史（≤16）；
`CancelUserShell{session, execution_id}` 只在 id 匹配时取消——不误杀 turn，
`CancelCurrentTurn` 不触碰 shell。

### D3 生命周期与投影（§11-12）

- 新 `UserShellId`（string_id! 4 行，wire 透明）。
- EngineEvent 新增：`UserShellStarted{execution_id,command,cwd}`（持久，LocalOnly，
  public_projection=None——命令行属本地敏感面）、`UserShellOutput{execution_id,
  stream,chunk}`（**transient**，不落库）、`UserShellFinished{execution_id,
  exit_code,duration_ms,status}`（持久，LocalOnly）。四个接缝（is_transient/
  data_class/public_projection/EventBridge 穷举）编译器强制表态。
- worker 全部事件走 `EventLog::append(event, turn_id=None, forward=bridge.forward)`
  —— 复用引擎的 persist-before-forward 与 transient 跳库；**不建 app 自定义事件旁路**
  （§11.2）：worker 自持一个 `EventBridge`（同一投影类型/同一穷举 match）面向
  per-session sender。
- RuntimeEvent 新增（additive，协议 1.4→1.5）：`UserShellStarted/Output/Exited`；
  Snapshot 增 `#[serde(default)] user_shells: Vec<UiUserShell>`（active + 有界完成
  历史）满足 §16 重连门（不做跨进程 storage 重设计；runtime crash 后按现有 cleanup
  杀死并标记，遵守 §44）。

### D4 安全语义（§5-7）

用户显式输入 = 授权该次调用 → **无审批弹窗**；host 安全全额保留：同 profile 写
confinement、deny_network sandbox、env scrub、树杀、workspace resolve。不提升
session profile、不产生 turn grant。`refuse_shell_script` guard 对 user shell
同样生效（保守复用；MVP 非交互，文档明示 vim/top/ssh 不保证）。timeout：user
shell **无默认 timeout**（`ProcessRequest.timeout=None`，用户有 x stop；截图语义
"Runtime 21m+"）。

### D5 TUI（§18-26）

- submit.rs 在 `/` 检测**之前**按原始首字符 `!` 路由（不 trim；`!` 空命令→本地化
  提示；入 composer 历史、不入对话/模型历史——slash 同模式）。
- `TranscriptItem::UserShell(UserShellBlock)`（id/command/status/有界输出尾/
  exit/duration/started_elapsed/expanded）；两个 toggle fn 加臂；conversation/build
  hits 表登记；**点击 UserShell 行为 = 打开 Shell Details**（reducer 按 item 类型
  分派，零 geometry 改动）。
- 新 `Screen::Shell`：status/runtime(实时增长)/Source=User/cwd/command/output/
  exit；Esc=back（shell 继续）、x=stop（仅 running，发 CancelUserShell）；
  `!cmd` 提交后直达该屏。Ctrl+C：shell active 且无 turn 时取消 shell（带测试）。
- 输出链：chunk 先 `sanitize_terminal_output` 再有界累积（tail 截断带标记）。
- 不碰 Tools screen/tool taxonomy/geometry/workbench。

### 有界缓冲（§14）

runtime 侧 per-execution tail 缓冲（64 KiB，截断标记），驱动 snapshot 与 Details；
chunk 事件本身 transient 小块（4 KiB 读粒度）。完成后保留 tail 供重连/disclosure。
不新造日志子系统。

## Change Radius 预告

protocol(additive)+ids / engine(3 变体 4 接缝) / execution(流式入口+shell_invocation
下沉) / app(use case + UserShellStore + bridge 臂) / tui(submit 路由+block+adapter+
Screen::Shell) / tests / docs。**不动**：Agent Loop、tool taxonomy、TUI geometry、
provider、context、ExecutionKind/ClientOrigin。

## Non-Goals

PTY/交互式终端、Web/Remote 入口（协议兼容即可）、跨进程 process adoption、
Agent tool details 重设计、per-Agent-tool kill、Executions 浏览器。
