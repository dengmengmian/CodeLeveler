# CodeLeveler 1.0.11

英文版：[`RELEASE.md`](RELEASE.md)

这一版把终端的执行展示冻结成一份只有唯一权威的 contract，并让一个会话在所有
传输路径上只有一条轴。终端、Web、Desktop 和移动 App 现在从同一批 runtime
事实推出同一棵语义树——执行轮次、真实的轮次状态、以及这一回合真正提交的回答；
而回答背后的 work / bookkeeping 分类只存在一份、通过 wire 传递，不再被复制进
四个渲染器。
Auto 权限也向普通开发对齐：临时文件、进程与系统观测、可重定位的只读 Git，
以及普通网络访问都不再询问，而破坏性操作仍然会问。
交互式打开的会话是一条 Chat 会话，无论它走 daemon 套接字还是 in-process；
`leveler run` 仍然驱动 Goal。

## 新增

- **执行展示 contract，用 fixture 冻结。** 一个回合投影出的语义树——可选的
  assistant 文本、带成员与真实状态的执行轮次、可选的最终回答、回合终态——由
  `testdata/execution_presentation/v1/` 里的语言无关语料库（C1..C14）钉住，
  每个 surface 都对着同一份语料自查：终端（参考实现，跑真实 reducer）、Web、
  Desktop 和移动 App。一个 fixture 可以声明 live 路径、reconnect snapshot 和
  durable history replay；所有声明的路径必须投影出同一棵树。
- **工具行按真实执行轮次分组。** 终端把一次模型响应里的调用归到一个轮次标题下，
  而不是平铺列表；同一轮里观测到的并发批次作为一个 batch 保留，轮次运行中会
  标出阶段。
- **真实的轮次标题。** 只有每个可见调用都成功时才会声称「全部完成」；已结束的
  轮次里若有取消、失败或未知的调用会如实说明；没有正文的回合以
  `no_final_answer` 结束，而不是绿色的「已完成」。
- **统一的回答生命周期。** 已提交的回答不会被 bookkeeping（`update_plan`、
  `update_goal(complete)`）覆盖；而其后真实的 read / search / edit / shell
  会把它降级为进行中。这个分类只有一个 owner（`leveler_tools::acts_on_answer`），
  并以 wire 事实（`answer_effect`）到达每个 surface；surface 不会根据工具名
  自行判定，无法识别的工具一律按「工作」处理。
- **`/btw` 侧问有自己的 surface。** 侧问的只读工具活动只出现在侧问 surface，
  永远不会进入主回合的会话记录、回答或计划。
- **Auto 权限覆盖普通开发。** 写临时文件（`/tmp`、`$TMPDIR`）不再以 `EPERM`
  失败；进程与系统观测可运行；可重定位的只读 Git（`git -C <dir> status`、
  `--git-dir`、`--work-tree`）仍然是读操作，不再升级为 ASK；Auto 的普通网络
  ALLOW 也会真正落到 sandbox 上，并以 permission DENY 失败作为负向对照。
  破坏性操作仍然询问。
- Web 客户端与移动 App 也会显示桌面风格的轮次树，包括用命令自己的失败原因
  （已去掉 runtime 的执行行）。

## 变更

- **`--permission` 不再有默认值。** 不传时，新会话使用项目 / 默认 profile，
  而 resume 会保留会话自己持久化的 profile；传入则是显式覆盖。之前缺少该参数
  时按 `assisted` 处理，这也让 resume 无法区分「未改动」和「设为 assisted」。
- **交互式会话在所有传输路径上都是 Chat。** `leveler` 与 `leveler tui` 过去走
  daemon 套接字时记录 `chat`，而 in-process 时记录 `goal`，因为内嵌路径是按
  进程默认值解析轴的。现在交互轴只声明一次、两条传输都写它；`leveler run`
  保持 Goal 默认值，resume 则保留会话创建时的轴。
- **客户端协议升到 minor 14**（major 仍为 1）。新增字段都是可选的：省略它的
  peer 保持原有行为，Web / Desktop / App 的连接方式不变。
- 窄终端的状态 chip 会先保住 collaboration 与 permission，然后才缩短模型名，
  这两个控制项不再是最先消失的东西。
- 过渡性叙述在视觉上退到回答之下；混合工具的轮次只给一个简化标签，不再按种类
  罗列。
- Web、Desktop 和 App 不再把模型的原始推理当作会话记录渲染；运行状态仍然会
  说明模型正在思考。

## 修复

- **重连会丢掉运行中工具的执行轮次。** live view 在折叠进 reconnect snapshot
  时丢掉了轮次标识，于是重连的客户端只能按工具种类和时序重新猜轮次，甚至把
  第二个轮次焊到第一个上。现在 snapshot 会声明 runtime 早已知道的轮次。
- **命令的失败原因可能是 runtime 自己写的行。** preview 里混着命令的输出和
  runtime 写的行（`exit: N`、流标题、`[execution policy] …`、超时）。这些行
  说明的是「怎么跑的」，把它们当成「为什么失败」，就会把 sandbox 的写入限制
  说明当成 `git grep` 无匹配退出 1 的原因，还会把该行算作输出。现在这些行只
  分类一次，并从失败原因、展开正文和输出行数里排除；超时会明确写成超时。
- 比状态条更宽的在忙行会塌缩成一个裸 spinner，并静默丢弃它后面的耗时、工具数、
  token 信息。
- 带前缀的文本（`※ 回顾:`）按字符数换行，窄终端上会把宽字符裁掉。
- 窄状态 chip 会先丢掉 collaboration 和 permission，再丢模型名，把两个优先级
  更高的控制项藏了起来。
- **ProtocolRepair 可能重新变成用户消息。** goal closeout 仍然会在 resume 时
  送给模型，但重放的历史和会话快照不再把它投影成用户撰写的文本；live 客户端
  原本就已经隐藏它。
- 审批状态可能在触发它的 profile 消失后仍然存在：permission profile 变化时
  待审批会被作废，选中的模式也能跨 resume 保留。
- 用户 shell 已完成的输出尾部在 live 投递断开时会丢失（移动 App）。
- closeout 折叠可能折到属于更早轮次的消息；现在会被限制在自己的轮次内。
- runtime host 的 revive 竞态可能再 spawn 一个 runtime，而不是 adopt 已经在
  服务的那个。
- `leveler update` 在校验下载时只重试一次瞬时 exec 失败。

## 已知限制

- 预编译包没有苹果或微软的官方签名。macOS 或 Windows 第一次运行时可能
  会拦截，需要你允许一次
- Windows 还不能按命令断开网络。需要断网隔离的命令会被拒绝，而不会在
  网络仍开放时继续跑
- Windows 没有本地 daemon 套接字。会话和 `resume` 仍然可用
- Windows 上受隔离的 `!command` 结束后才打印输出，不会边跑边刷
- 非终端 surface 的 durable-history 重放仍不完整：Web 与 App 没有
  `query_session_history` 消费者，Desktop 无法仅凭历史重建一个回合。各自的
  conformance 测试会断言这些缺口，而不是把它藏起来

安装与使用见 [README](../README.zh-CN.md)。
更新见 [README · 更新](../README.zh-CN.md#更新)。
系统怎么分层见 [架构说明](ARCHITECTURE.zh-CN.md)。
