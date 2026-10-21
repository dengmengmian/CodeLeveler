# CodeLeveler Desktop

Electron 是桌面壳。任务、对话、工具、审批和持久化来自现有 Rust Runtime。Renderer 通过 sandbox preload 的固定 IPC 方法访问 Main，Main 只启动 `leveler desktop-bridge` JSONL 客户端。Runtime 的 discovery、spawn、adopt、revive 和 handoff 复用 Rust Runtime Host。

本版本按 DeepSeek Harness Desktop `0.2.1-rc.2`（参考 HEAD `687f282e`）的实际界面重新实现 Desktop 外壳与交互；视觉规格来自对参考 Electron 的真实截图、DOM 几何和计算样式，源码由 CodeLeveler 独立编写。对照截图与验收证据在 `acceptance-output/ui-parity-round2-2026-10-20/`（忽略目录）。

```sh
cargo build -p leveler-cli --bin leveler
cd apps/leveler-desktop
npm ci
npm start
```

默认使用仓库的 `target/debug/leveler`。可通过 `LEVELER_BINARY=/absolute/path/to/leveler` 指定二进制；`LEVELER_HOME` 和现有模型配置由 bridge 继承。未配置 provider 的错误会显示在对话中。

## 界面结构

- **Shell**：macOS `hiddenInset` 标题栏、交通灯区域、vibrancy 侧栏、可收起左侧栏；窄窗口侧栏改为浮层。
- **左侧栏**：品牌区、新会话、插件 / 自动化任务入口、工作区与会话列表、搜索、分组/排序/归档视图选项、添加工作区、账号菜单。
- **Conversation**：会话头部、`对话 / 轨迹` view tabs、用户气泡、Assistant Markdown、思考、工作步骤与工具调用、工具输出预览、确认改动 diff、审批 / 用户问题 takeover、运行与失败状态、消息复制、会话搜索、重命名、模型/权限切换、上下文统计。
- **Trajectory**：按轮次分组的 durable 事件账本、请求编号、输入/模型/工具时间线、时长与调用折叠、轨迹搜索、事件检查器（概述 / 参数 / 结果 / Schema / 计时）与宽度拖拽；缺失的 Token、系统提示词和计时明确显示不可用。
- **Composer**：28px 圆角输入卡、附件上传、权限 / 模型 / 计划模式入口、参考命令菜单（添加 / 指令）、@ 引用提示、发送 / 停止、按会话保存草稿（重启后恢复）。
- **右侧边栏**：Start 引导、多标签、分栏、标签拖动/跨栏移动、栏宽调整、浮窗及返回停靠区、全屏、Open In App、按会话持久化布局。
- **Settings**：800×684 modal，通用设置、模型、内置插件、Agent 预设；主题、字号、工作步骤展示/收起、性能与用量等。
- **浅色 / 深色 / 跟随系统**，以及 1024 / 768 / 520 等窄窗口布局。

## 功能接入状态

| 状态 | 功能 |
| --- | --- |
| **CONNECTED** | 会话创建/打开/恢复、历史与流式消息、Reasoning、专用工具卡片（命令/读取/编辑/搜索/网页/待办/通用）、审批、用户问题、模型切换、权限模式、重命名、分叉、归档、Git 变更与 diff、原生隔离 Browser、Browser 浮窗/全屏/多标签/系统浏览器打开、图片附件、上下文统计、Plan（含跨重启恢复）、子智能体只读状态、后台任务状态（运行/完成/失败/停止）、字体设置、系统浏览器链接、打开配置文件、`对话 / 轨迹` view tabs、Trajectory 账本与检查器、Open In App（会话头 / Files / Preview / Review / 交付）、独立 `···` 菜单、参考命令菜单、运行中排队/插话发送、只读 Files/Preview/Review、内置 PTY Terminal、回答中的已确认文件改动卡片、Runtime/Bridge 断线自动重连。 |
| **UI_ONLY** | Deliverables 文件清单、插件目录与插件市场、自动化任务调度、语言、快捷键编辑器、Session Log 上传/下载、添加模型 Provider、Agent 预设切换、置顶 / 取消归档、@ 文件/会话引用、登录 / 意见反馈提交。 |
| **NOT_VERIFIED** | Windows 运行、macOS 原生全屏、真实 IME 长文本；真实子智能体创建与子会话嵌入。 |

本轮真实 Electron 已观测：会话创建/打开、消息、Reasoning、工具调用与专用卡片、审批、取消/resume、diff/Review、Plan 卡、Browser 导航/浮窗/全屏/外部打开、上下文统计、右栏分栏/浮窗/全屏、`对话 / 轨迹` 切换、Trajectory 账本/时间线/检查器/分页/区间、Open In App 菜单与记住选择、`···` 菜单、参考命令菜单、反馈弹窗、真实 Fork。真实 Provider（deepseek-flash / glm-5.3）验收覆盖工具调用、跨 Provider 模型切换、需批准外部写入审批、完全权限零审批、取消、关闭/重启 Resume 和权限切换。

UI_ONLY 功能不伪造数据或成功结果：列表为空时显示 Runtime 未提供接口，动作禁用或明确提示「未接入」。

## 验证

```sh
npm test                # 状态、IPC、安全、展示投影、Trajectory、Open In App；当前 143 项通过
npm run build           # 原生 JS 语法检查
npm run typecheck       # strict checkJs
npm run lint            # ESLint
```

真实验收使用隔离 `LEVELER_HOME`、Electron profile 与 workspace。

当前 `scripts/` 验收入口全部通过：`interactions`、`conversation`、`files-and-tabs`、`composer`、`session-actions`、`settings-queries`、`commands-settings`、`settings-branches`、`project-interactions`、`sidebar-compact`、`responsive-theme`、`responsive-long-home`、`upload-deadline`、`views`、`browser-dock`、`browser-native`、`product`、`long-session`（合成渲染）、`background-tasks`、`recovery`、`ui-capture`，以及真实 Provider 的 `acceptance` 与 `real-provider-files`。`workbench.mjs` 已标记 SUPERSEDED 并直接运行 `files-and-tabs.mjs`。`conversation-runtime.mjs` 是 release/RC 资格通道，要求干净候选与按 HEAD 构建的二进制，不属于日常验收。

证据（截图、JSON、日志）位于 `acceptance-output/`，该目录按约定不入库。

## 已知限制

- Files / Preview 已接入真实只读目录与文件读取（文本、代码、Markdown、JSON/YAML/TOML、CSV、图片、行号跳转）。PDF / Office 仍为 UI_ONLY；界面明示不可预览，不伪造内容。
- 内置终端通过 Rust Runtime 接入同一个 Execution Host，支持 Unix PTY；macOS 已实测，Windows 明确不支持。要求会话有工作区；旧宿主缺少终端能力时明确报错，不在 Electron 中另行启动 Shell。关闭标签等待进程退出；输入结果未知时不会自动重发。
- 工具卡输出来自 Runtime 的 1200 字符预览；超长输出以省略号截断，当前没有完整输出查看入口。
- 回答中的文件卡片来自当前回合已确认的宿主文件改动，支持预览和 Open In App；模型正文中的文件名本身不能创建交付事实。消息时间缺失时不显示，不补造时间。
- Deliverables、插件市场、自动化调度、Session Log、登录/反馈提交没有 Desktop bridge 接口，界面完整呈现真实未接入状态。
- Plan 在回合内真实创建、更新和审阅，并随 Runtime snapshot 跨 Desktop 重启恢复；Runtime 没有计划审批与接管语义，审阅面板只读。
- 后台任务支持运行中输出、停止、完成和失败；回合结束的快照刷新不会丢掉已结束任务，popover 随事件更新。极短任务的终端结果依赖 Runtime 的 exit 事件。Bridge 只订阅当前会话，切到其它会话期间产生的输出不会回填；切回后任务状态仍在，但这段时间的输出不可见。
- Desktop bridge 断开后自动重连并恢复已确认会话；有界重试用尽后显示错误并提供手动重试。
- 会话工作区文件夹不存在时，打开会给出「工作区文件夹不存在或无法访问」的产品提示；恢复该文件夹后可重新打开并读取历史。
- 子智能体只读状态与血缘可用；打开子会话、向子智能体输入未接入。真实子智能体的创建/执行本轮未验证。
- 用户问题（`request_user_input`）默认不向模型广播；模型需先启用 `host_interaction` capability 才能提问。回答后问题与选项不写入会话正文，只保留在工具记录中。
- Trajectory 的历史 Token 用量、绝对墙钟、Provider TTFT 与工具 Schema 不在 Runtime durable log 中，界面显示「不可用」而不是补零；已实现 50 条尾部窗口分页和时间区间拖拽，虚拟滚动未实现。
- 参考项目的部分动效（侧栏收起序列、右栏滑入、hero 动画等）尚未逐项对齐。
- Open In App 使用安装目录中的应用清单与记住的选择；应用目录与打开路径仅 macOS 实测，Windows/Linux 会隐藏该入口。文件场景不查询每个后缀的 OS 默认关联，「显示文件位置」按真实文件执行。
- 真实 Provider 已完成消息、工具调用、文件编辑、审批、取消、Resume、模型与权限切换、关闭/重启接管验收；Windows 运行、macOS 原生全屏与真实 IME 仍未验证。
