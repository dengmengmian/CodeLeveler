# 更新日志

英文版：[`CHANGELOG.md`](CHANGELOG.md)

发布说明：[`docs/RELEASE.zh-CN.md`](docs/RELEASE.zh-CN.md)。

## [1.0.12] - 2026-10-07

把终端的执行展示冻结成一份只有唯一权威的 contract，并让一个会话在所有传输路径
上只有一条轴。终端、Web、Desktop 和 App 从同一批 runtime 事实推出同一棵语义树；
回答背后的 work / bookkeeping 分类只存在一份、通过 wire 传递，而不是复制进四个
渲染器。Auto 权限与普通开发对齐，交互式打开的会话在两条传输上都是 Chat。

### 新增

- 执行展示 contract，用语言无关的 fixture 语料库冻结（`testdata/execution_presentation/v1/`，C1..C14），每个 surface 都对着它自查：终端是参考实现，另有 Web、Desktop 和 App
- 工具行按真实执行轮次分组，同一轮里观测到的并发批次作为 batch 保留，运行中会标出阶段
- 真实的轮次标题：只有每个可见调用都成功才声称「全部完成」，没有正文的回合以 `no_final_answer` 结束，而不是绿色的「已完成」
- 统一的回答生命周期：已提交的回答不会被 `update_plan` / `update_goal(complete)` 覆盖，其后真实的工作会把它降级；该分类以 wire 事实 `answer_effect` 到达每个 surface（无法识别的工具按工作处理）
- `/btw` 侧问有自己的只读 surface，不会进入主会话记录、回答或计划
- Auto 权限覆盖普通开发：临时文件、进程与系统观测、可重定位的只读 Git（`-C`、`--git-dir`、`--work-tree`）与普通网络；破坏性操作仍然询问

### 变更

- `--permission` 不再有默认值：不传时新会话使用项目 / 默认 profile，resume 保留会话持久化的 profile；传入则显式覆盖
- 交互式会话在所有传输路径上都是 Chat（`leveler` / `leveler tui` 走 daemon 套接字和 `--in-process` 都是）；`leveler run` 保持 Goal 默认值，resume 保留持久化的轴
- 客户端协议升到 minor 14（major 仍为 1），新增字段全部可选
- 窄终端的状态 chip 先保住 collaboration 与 permission，再缩短模型名；过渡性叙述在视觉上退到回答之下
- Web、Desktop 和 App 不再把模型的原始推理当作会话记录渲染

### 修复

- 权限变更可能被静默撤销：暂存的回合会在每次构建 engine 时把它捕获的权限模式写回会话的 live profile，于是 UI 可能显示 `full`，而策略层读到的仍是旧的 `assisted`，并对破坏性命令发出审批
- 重连会丢掉运行中工具的执行轮次，导致重连的客户端按工具种类和时序重新猜轮次
- 命令的失败原因可能是 runtime 自己写的行（`[execution policy] …`、`exit: N`、超时），而不是命令自己的输出，并且该行还被算作输出
- 比状态条更宽的在忙行会塌缩成裸 spinner，并丢掉耗时、工具数和 token 信息
- 带前缀的文本（`※ 回顾:`）按字符数换行，窄终端上会裁掉宽字符
- 窄状态 chip 先丢掉 collaboration 和 permission，再丢模型名
- ProtocolRepair 可能重新以用户消息出现在重放历史和会话快照里
- permission profile 变化时待审批没有作废，选中的模式也无法跨 resume 保留
- 用户 shell 已完成的输出尾部在 live 投递断开时丢失
- closeout 折叠可能折到属于更早轮次的消息
- runtime host 的 revive 竞态可能再 spawn 一个 runtime，而不是 adopt 正在服务的那个
- `leveler update` 在校验下载时只重试一次瞬时 exec 失败

## [1.0.11] - 2026-10-02

重做了模型请求的组装、折叠和计量方式：控制上下文与会话记录分离，每一段提示词
都有唯一的来源和权威，每一次模型尝试都记入请求账本。

### 新增

- 单一「已投影请求」：链路编码与上下文统计读取同一份结果，报告出的输入规模与实际发送的字节不会再互相矛盾
- 逐次尝试的请求计量：失败、重试、压缩摘要和子任务调用都会记入请求账本和资源预算
- **CodeLeveler Desktop**：`apps/leveler-desktop/` 下的 Electron 客户端。Renderer 只能通过 sandbox preload 的固定 IPC 面访问 Electron Main，Main 只能通过内部 `leveler desktop-bridge` JSONL 适配器访问 Runtime，因此 Runtime 的 discovery、spawn、adopt、revive 和 handoff 仍由 Runtime Host 拥有。它从仓库构建；发布包内仍只有 `leveler` 二进制
- **所有模型共用一套 Thinking Level**：`/thinking` 与 `/thinking <level>` 可为当前会话设置 `auto`、`off`、`minimal`、`low`、`medium`、`high` 或 `max`，`~/.leveler/config.toml` 里写的是同一套词，可全局设置也可按模型设置。默认是 `high`。`max` 表示「该模型声明的最高档」，而不是某个固定的服务商取值，因此它会随模型变化重新解析，而不是钉死某个 route 恰好读取的参数

### 变更

- 每一段投递的提示词都带唯一的来源、权威和生命周期。Provider 可以改变某一段在链路上的表示形式，但不能改变它的顺序、来源或权威
- 上下文折叠压力按真实的已投影请求判定；作用于某个目录的项目规则在折叠后被逐字保留
- 恢复时的消费量由该作用域已持久化的请求事实重建，后台子任务不会再继续花费过期的余额快照
- 规则投递按请求额度分配：即使规则文件只有一部分能逐字放入，每条规则仍保持权威
- 附件导入结果携带产生它的那条命令的身份，客户端因此可以把一次成功保存或一次失败对应回自己的请求，而不必按附件名猜测；该字段在链路上可选
- 以 `runtime` lifetime 启动的后台任务由 Execution Host 拥有，而不是由启动它的那个 runtime generation 拥有：dev server 或 watcher 因此可以跨版本更新存活，替换上来的 generation 会重新接上它
- `auto` 的含义是「不覆盖」，不再是 CodeLeveler 自己的调参：它不会再变成模型 profile 声明的 default，后者只留给 harness 自己发起的调用。模型无法精确表达的档位也不再被四舍五入到相邻档位——它不可用，请求不带覆盖，`leveler doctor` 会列出该模型真正支持的档位
- 会话的 Thinking Level 会到达主请求，`/btw` 旁问继承它，内部压缩保留自己的策略。`off` 会变成 route 级别的关闭，而不是在 executor policy 与请求之间丢失

### 修复

- 缩减上下文的 route 若声明了比自身 `context_window` 更大的 `max_output_tokens`，会把整段补全预留在窗口上，把可用输入容量压到 `0`，导致运行在第一批工具调用后即以 `context management failure: ... capacity 0` 中止。容量为 0 不是模型声明的界限，因此现在只按质量边界折叠
- 请求前产生的上下文快照现在会送达调用方的 observer，不再被丢弃
- 模型消费作用域切换时，不再丢弃当前任务纪元剩余的命令预算
- 批次在一轮中途被取消时，已完成命令的消费会被结清
- 子任务渲染出的 spawn brief 会持久化，并在 resume 时复用
- `/btw` 的只读调用改为通过 ToolHost 准入管线
- 后台任务写入被限制在 OS 执行边界内，任务变更在整个工作负载结束后才结算，运行中的 stdout、stderr 通道设有上限并在截断处给出明确标记
- 记忆列表现在会报告导致它的 store 失败，而不会把一次失败的读取当作空列表
- 状态栏与 Web 客户端显示的是统一词汇里的档位（`high`、`max`），而不是服务商的参数；`xhigh`、`reasoning_effort`、`output_config` 和 `budget_tokens` 不再有任何路径出现在用户看到的内容里
- 对已删除的会话调用 `SetThinkingLevel` 会返回 `SessionNotFound`，而不是更新零行却报告成功
- 仓库内置模型未声明档位时会解析到全局默认值，因此内置的 `high` 也能到达 YAML/model 加载路径

## [1.0.0] - 2026-09-18

第一个稳定版。从这版起 CodeLeveler 遵循语义化版本。

### 新增

- 自升级：启动时检查最新的**稳定版** GitHub Release，校验 SHA-256，替换当前二进制并重启。失败不会阻止启动
- `leveler update`（别名 `upgrade`）手动检查与安装，支持 `--check`、`--force`、`--version <tag>`
- TUI 中的 `/update`，任务运行期间会被拒绝
- `~/.leveler/config.toml` 新增 `[update]`：`auto_update`、`check_interval_hours`

### 变更

- 发布产物固定为 `leveler-v<version>-<target>.tar.gz|zip` 及其 `.sha256`；发布工作流拒绝与工作区版本不一致的 tag

### 修复

- 工具调用较多的长会话可能在之后每一轮都失败，报 `HTTP 400 invalid_request`（"Messages with role 'tool' must be a response to a preceding message with 'tool_calls'"）。上下文组装现在始终让工具调用与其结果成对；丢失配对的上下文快照会被忽略，改用已保存的会话记录；仍违反配对的请求在发送前被拒绝，并报告为内部会话协议错误

## [0.1.0-beta.1] - 2026-09-17

面向 macOS、Linux 和 Windows 的公开 beta。

### 这一版有什么

- 终端界面（`leveler tui`）、网页界面（`leveler web`）和命令行（`leveler run`）
- 会话保存在本机，之后可以继续
- 自定义 Agent：一个目录，里面是 `agent.yaml` 和 `instructions.md`
- 移动端，以及宿主机上的远程桥（`leveler remote`）
- 写文件和执行命令需要批准
- 命令隔离：macOS 用 Seatbelt，Linux 用 bubblewrap，Windows 用 Low integrity

### 已知限制

- 预编译包没有苹果或微软的官方签名。macOS 或 Windows 第一次运行时可能会拦截，需要你允许一次
- 1.0 之前，命令和配置还可能变。`run`、`resume`、`tui` 打算保持稳定；`eval` 和 `remote` 不一定
- Windows 还不能按命令断开网络。需要断网隔离的命令会被拒绝，而不会在网络仍开放时继续跑
- Windows 没有本地 daemon 套接字。会话和 `resume` 仍然可用
- Windows 上受隔离的 `!command` 结束后才打印输出，不会边跑边刷

安装与使用见 [`README.zh-CN.md`](README.zh-CN.md)。
系统怎么分层见 [`docs/ARCHITECTURE.zh-CN.md`](docs/ARCHITECTURE.zh-CN.md)。
