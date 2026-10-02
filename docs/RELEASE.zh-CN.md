# CodeLeveler 1.0.11

英文版：[`RELEASE.md`](RELEASE.md)

这一版重做了模型请求的组装、折叠和计量方式：控制上下文与会话记录分离，
每一段提示词都有唯一的来源和权威，每一次模型尝试都记入请求账本。
已安装的稳定版可以自动更新，也可以执行 `leveler update` 或 `/update`。

## 新增

- **CodeLeveler Desktop**：`apps/leveler-desktop/` 下的 Electron 客户端。任务、
  对话、工具、审批和持久化仍然来自 Rust Runtime。Renderer 只能通过 sandbox
  preload 的固定 IPC 面访问 Electron Main，拿不到 Runtime 凭据，也没有 Node
  访问权；Main 只能通过内部 `leveler desktop-bridge` JSONL 适配器访问 Runtime，
  因此 Runtime 的 discovery、spawn、adopt、revive 和 handoff 仍由 Runtime Host
  拥有。它从仓库构建（在 `apps/leveler-desktop` 下 `npm ci` 后 `npm start`）；
  发布包内仍只有 `leveler` 二进制。
- 桌面客户端提供任务导航、支持安全 Markdown 的对话视图、可关闭的工作台
  （Plan、Changes 和手动 Browser 标签）、按会话生效的模型与权限菜单，以及
  有上限的附件上传。
- Desktop bridge 对它转发的每条命令都按各自的明确上界校验：仅 PNG 的图片附件
  上限 20 MiB、64 位十六进制摘要、边长 1..=2048 像素，非空且有界的 query id，
  有界的可观测窗口，经校验的 agent 名，以及仅作用于所选会话的模型、权限、重命名
  和归档命令。其余一律拒绝。

## 变更

- 提示词组装让每一段投递内容都有唯一的来源、权威和生命周期。Provider 可以
  改变某一段在链路上的表示形式（system message 或顶层 system 字段），但不能
  改变它的顺序、来源或权威；控制上下文不再作为会话记录的一部分传递。
- 链路编码与上下文统计读取同一个「已投影请求」，报告出的输入规模与实际发送的
  字节不会再互相矛盾。折叠压力按已投影请求判定，作用于某个目录的项目规则在
  折叠后被逐字保留，历史推理遵循路由自己的回放契约。
- 每一次模型尝试都记入请求账本和资源预算，包括失败、重试、压缩摘要和子任务
  调用。恢复时消费量由该作用域已持久化的请求事实重建，后台子任务不会再继续
  花费过期的余额快照。
- 规则投递按请求额度分配：即使规则文件只有一部分能逐字放入，每条规则仍保持
  权威。
- 附件导入结果现在携带产生它的那条命令的身份，客户端因此可以把一次成功保存
  或一次失败对应回自己的请求，而不必按附件名猜测。该字段在链路上可选，raw-send
  导入不带它。
- 以 `runtime` lifetime 启动的后台任务由 Execution Host 拥有，而不是由启动它的
  那个 runtime generation 拥有：dev server 或 watcher 因此可以跨版本更新存活——
  启动它的 generation 退出时不会停掉它，替换上来的 generation 会重新接上它。
  较早协议 major 的 Execution Host 继续管理它已经在跑的服务；跨属主控制仍然被
  拒绝，而不是降级放行。
- 发布产物中记录了仓库的 `./dev` 开发入口，用于本地验证和发布资格判定。

## 修复

- 缩减上下文的 route 可以声明比自身 `context_window` 更大的
  `max_output_tokens`。把整段补全预留在窗口上，曾把可用输入容量压到 `0`，
  于是每个请求都被判为超出硬容量，运行在第一批工具调用后即以
  `context management failure: ... capacity 0` 中止。容量为 0 不是模型声明的
  界限，因此现在只按质量边界折叠；预留能放进窗口的 route 行为不变。
- 请求前产生的上下文快照现在会送达调用方的 observer，不再被丢弃。
- 模型消费作用域切换时，不再丢弃当前任务纪元剩余的命令预算。
- 批次在一轮中途被取消时，已完成命令的消费会被结清，不再随被取消的那一轮丢失。
- 子任务渲染出的 spawn brief 会持久化在子任务 spec 上并在 resume 时复用，
  恢复后的子任务看到的是它启动时的那份 brief。
- `/btw` 的只读调用改为通过 ToolHost 准入管线，不再绕过它。
- 后台任务写入被限制在 OS 执行边界内，任务变更在整个工作负载结束后才结算，
  运行中的 stdout、stderr 通道设有上限，并在截断处给出明确标记，不会无限增长。
- 记忆列表现在会报告导致它的 store 失败，而不会把一次失败的读取当作空列表。

## 已知限制

- 预编译包没有苹果或微软的官方签名。macOS 或 Windows 第一次运行时可能
  会拦截，需要你允许一次
- Windows 还不能按命令断开网络。需要断网隔离的命令会被拒绝，而不会在
  网络仍开放时继续跑
- Windows 没有本地 daemon 套接字。会话和 `resume` 仍然可用
- Windows 上受隔离的 `!command` 结束后才打印输出，不会边跑边刷

安装与使用见 [README](../README.zh-CN.md)。
更新见 [README · 更新](../README.zh-CN.md#更新)。
系统怎么分层见 [架构说明](ARCHITECTURE.zh-CN.md)。
