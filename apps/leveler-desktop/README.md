# CodeLeveler Desktop

Electron 是桌面壳。任务、对话、工具、审批和持久化来自现有 Rust Runtime。Renderer 通过 sandbox preload 的固定 IPC 方法访问 Main，Main 只启动 `leveler desktop-bridge` JSONL 客户端。Runtime 的 discovery、spawn、adopt、revive 和 handoff 复用 Rust Runtime Host。

本版本按 DeepSeek Harness Desktop `0.2.1-rc.2`（参考 HEAD `687f282e`）的实际界面重新实现 Desktop 外壳与交互；视觉规格来自对参考 Electron 的真实截图、DOM 几何和计算样式，源码由 CodeLeveler 独立编写。详细对照、差异和未验证项见 [docs/DESKTOP_UI_PARITY.md](../../docs/DESKTOP_UI_PARITY.md)。

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
- **Conversation**：会话头部、用户气泡、Assistant Markdown、思考、工作步骤与工具调用、完整工具输出、确认改动 diff、审批 / 用户问题 takeover、运行与失败状态、消息复制、会话搜索、历史提问、重命名、模型/权限切换、上下文统计。
- **Composer**：28px 圆角输入卡、附件上传、权限 / 模型 / 计划模式入口、命令面板、@ 引用提示、发送 / 停止、按会话保存草稿。
- **右侧边栏**：Start 引导、多标签、分栏、标签拖动/跨栏移动、栏宽调整、浮窗及返回停靠区、全屏、按会话持久化布局。
- **Settings**：800×684 modal，通用设置、模型、内置插件、Agent 预设；主题、字号、工作步骤展示/收起、性能与用量等。
- **浅色 / 深色 / 跟随系统**，以及 1024 / 768 / 520 等窄窗口布局。

## 功能接入状态

| 状态 | 功能 |
| --- | --- |
| **CONNECTED** | 会话创建/打开/恢复、历史与流式消息、Reasoning、专用工具卡片（命令/读取/编辑/搜索/网页/待办/通用）、审批、用户问题、模型切换、权限模式、重命名、归档、Git 变更与 diff、原生隔离 Browser、Browser 浮窗/全屏/多标签/系统浏览器打开、图片附件、上下文统计、Plan、子智能体只读状态、后台任务状态、字体设置、系统浏览器链接、打开配置文件。 |
| **UI_ONLY** | Files 文件树、Document Preview（可打开但无文件读取）、交互式 Terminal、Deliverables 文件清单、插件目录与插件市场、自动化任务调度、语言、快捷键编辑器、排队发送 UI、Session Log 上传、添加模型 Provider、Agent 预设切换、置顶 / Fork / 取消归档、@ 文件/会话引用、登录 / 意见反馈。 |
| **NOT_VERIFIED** | 真实模型 Provider 下的工具链、审批与长会话（本轮真实 Provider 仅验收了普通对话/resume）；Windows 运行、macOS 原生全屏、真实 IME 长文本、原生窗口级截图对比。 |

本轮真实 Electron 已观测：会话创建/打开、消息、Reasoning、工具调用与专用卡片、审批拒绝/取消、取消/resume、diff、Plan、Browser 导航/浮窗/全屏/外部打开、上下文统计、右栏分栏/浮窗/全屏。真实 Provider 验收覆盖新建、发送、流式回复、会话切换、关闭/重启同 Runtime 复用和窄窗；工具/审批/长会话仍使用测试 Provider 或合成桥接。

UI_ONLY 功能不伪造数据或成功结果：列表为空时显示 Runtime 未提供接口，动作禁用或明确提示「未接入」。

## 验证

```sh
npm test                # 状态、IPC、安全、展示投影；当前 125 项通过
npm run build           # 原生 JS 语法检查
npm run typecheck       # strict checkJs
npm run lint            # ESLint
```

真实验收使用隔离 `LEVELER_HOME`。本版本的对照截图、overlay、difference 和功能状态表在 `acceptance-output/ui-parity-round2-2026-10-20/`（忽略目录）。已通过的新 UI 脚本：`test:acceptance`（真实 Provider）、`test:interactions`、`test:conversation`、`test:composer`、`test:long-session`、`test:session-actions`、`settings-queries`、`upload-deadline`、`views`，以及新增 `scripts/browser-dock.mjs`、`scripts/ui-capture.mjs`。

仍绑定旧 UI 的其他脚本（`product`、`browser-native`、`commands-settings`、`project-interactions`、`responsive-long-home`、`responsive-theme`、`settings-branches`、`sidebar-compact`、`workbench`、`conversation-runtime`）尚未迁移，不能作为本版本回归证据。`test:acceptance` 的真实 Provider 验收覆盖普通对话与 Runtime 复用，不覆盖真实模型下的工具链、审批和长会话。

## 已知限制

- Files / Preview / Terminal / Deliverables 没有 Desktop bridge 的独立目录读取、文件读取、PTY 或交付物清单接口；界面完整呈现，但数据来源为真实的未接入状态，不生成假数据。
- 工具卡片覆盖命令、读取、编辑、搜索、网页、待办和通用 IN/OUT；Browser 工具按参考走通用卡片。
- Conversation 头部的面包屑、视图 tab 和部分工具位没有逐像素覆盖参考的全部 slot。
- 右栏浮窗支持 Files/Changes/Plan/Preview/Subagent/Browser；Browser 浮窗、全屏和多标签已用真实 `WebContentsView` 验收。
- 参考项目的部分动效（侧栏收起序列、右栏滑入、hero 动画等）尚未逐项对齐。
- 真实 Provider 只完成了普通对话/resume 验收；工具链、审批和长会话仍使用测试 Provider 或合成桥接。
