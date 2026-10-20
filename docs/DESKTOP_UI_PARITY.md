# Desktop × DeepSeek Harness 对照报告（第二轮）

日期：2026-10-20  
参考：`/Users/mengmian/Develop/app/other/deepseek-harness`，HEAD `687f282e8625c6b8d11ff58211a90524be2e4317`，Desktop `0.2.1-rc.2`，Electron `44.7.0`。  
目标：`/Users/mengmian/Develop/app/dengmengmian/CodeLeveler`，采集时源码基于 `963f03c` 工作区，收口提交 `207a459`。

## 结论

第二轮完成了右侧 Dock、工具卡片、Settings、Browser 浮窗/全屏、系统浏览器打开、配置打开、字体设置和验收脚本迁移。核心页面几何与参考对齐，多数截图差异在 3%–5%；**整体仍不宣布 1:1 PASS**：

- Files、Preview、Terminal、Deliverables 仍缺少 Desktop bridge 的目录/文件读取、PTY 和交付物清单能力，保持 UI_ONLY。
- Settings 浅色仍有 **21.95%** 的 >16 像素差异，主要来自文字逐像素位置/抗锯齿、控件文案和“未接入”标记，不是面板几何错误。
- 真实 Provider 只验收了普通对话、会话切换、Runtime 复用和窄窗；工具链、审批、长会话仍使用测试 Provider 或合成桥接。
- 一组旧脚本仍绑定旧 DOM，未迁移，不能作为本轮回归证据。

## 截图与差异

证据目录：`apps/leveler-desktop/acceptance-output/ui-parity-round2-2026-10-20/`（git 忽略）。截图来自真实 Electron webContents，不是浏览器打开的 Web 版。

| 对照 | 尺寸 | 平均绝对差 | >16 像素占比 | 说明 |
| --- | --- | ---: | ---: | --- |
| Home 1280 浅色 | 2560×1640 | 3.50 | 4.10% | 品牌、会话数据、权限文案差异 |
| Home 1280 深色 | 2560×1640 | 3.76 | 3.89% | 同上 |
| Home 1024 浅色 | 1024×768 | 8.08 | 10.78% | 参考为旧轮采集，数据/文字状态不同 |
| Home 520 浅色 | 520×600 | 8.12 | 9.77% | 同上 |
| Start 1280 浅色 | 2560×1640 | 3.65 | 4.36% | 卡片、图标、快捷键、位置一致 |
| Files 1280 浅色 | 2560×1640 | 4.08 | 4.65% | 参考有真实目录树；目标是真实“无工作区/未接入”状态 |
| Terminal 1280 浅色 | 2560×1640 | 3.94 | 4.53% | 参考有真实 zsh；目标为 UI_ONLY 空态 |
| Browser 1280 浅色 | 2560×1640 | 3.81 | 4.39% | 空地址状态 |
| Settings 1280 浅色 | 2560×1640 | 12.61 | 21.95% | 行位置对齐，残留文字/控件差异，未收敛 |
| Settings 1280 深色 | 2560×1640 | 3.01 | 2.49% | 深色差异较小 |

每个可比较项同时生成 `*-side-by-side.png`、`*-overlay.png`、`*-difference.png`；指标在 `metrics.json`。实际截图覆盖 Home 1440/1280/1024/1023/960/768/700/520、Home/Settings 深色 1280、Start/Files/Terminal/Browser/Settings 1280、Start/Settings 1024/520。参考侧在 1024/520 的 Home 使用上一轮冻结参考，其余为本轮实拍。

### 几何

| 组件 | 参考 | 本轮 | 结果 |
| --- | --- | --- | --- |
| 左侧栏 / Header | 280px / 76px | 280px / 76px | 一致 |
| 会话列 / Composer | 680px / 712×114 | 680px / 712×114（1280 无右栏） | 一致 |
| 右栏宽度（1280，45%） | 576px | 576px | 一致 |
| Dock 标签 | 38px strip，28px chip | 38px / 28px，最小 100px 外宽 | 一致 |
| Start 卡片 | 380×67，gap 14，radius 20 | 380×67，gap 14，radius 20 | 一致 |
| Settings modal | 800×684 @ y=68 | 800×684 @ y=68 | 一致 |
| Settings nav / 内容 | 188 / 612 | 188 / 612 | 一致 |
| 外观卡片 | 181×83 @ y=313 | 181×83 @ y=313 | 一致 |
| 字号行 / 收起行 | y=429 / 505 | y≈430 / 506 | ≤1px |
| 深色底 / 输入面 | `#151517` / `#2c2c2e` | `#151517` / `#2c2c2e` | 一致 |

Settings 浅色 21.95% 的差异没有归因成“字体抗锯齿”就结束：side-by-side 显示行位置基本对齐，但文字字形落点、控件宽度（权限文案）、语言“未接入” badge、代码工作视图开关禁用态和参考不同。该页仍判定为**视觉未完全通过**。

## 本轮实现

- 右侧 Dock：标签最小宽度、关闭最后一个普通标签回落 Start、Start 不可关闭、键盘 roving、拖出浮动、浮窗在面板收起/全屏时保持可见、按会话持久化。
- Files/Preview/Terminal/Deliverables：补全参考结构（路径、刷新、打开方式、文件类型、预览工具栏/渲染器/换行/重读、终端状态栏、交付聚合卡），无底层能力处显示真实未接入状态，不伪造文件/Shell/产物。
- Browser：真实 `WebContentsView` 浮窗、全屏、多标签、显式系统浏览器打开、错误“修改地址”、地址草稿保留、关闭最后一个标签回落 Start；`browser-dock.mjs` 用真实原生页体验收。
- Conversation：命令/读取/编辑/搜索/网页/待办专用卡片、通用 IN/OUT、退出码/diff 统计、复制失败反馈、分支入口（未接入）、滚动容器修正、变更文件卡。
- Settings：字体字号/字体族/代码与终端字号、工作步骤和收起选项、排队/插入发送行为、系统浏览器链接、打开配置文件、标题/控件几何；系统链接和配置文件真实接通。
- Composer：修复 `#send` 未绑定、工作区草稿、繁忙时默认排队及 Cmd/Ctrl+Enter 互补行为。
- 打开外部链接和打开配置文件的 Main IPC 只接受合法 HTTP(S) 或已存在配置路径。

## 接入状态

### CONNECTED（本轮真实运行观测）

会话创建/打开/恢复；消息/Reasoning/工具；命令、读取、编辑、搜索、网页、待办和通用工具卡；审批拒绝/取消；取消与 resume；模型切换；权限切换；重命名/归档；附件上传与图片 Provider 输入；Git diff 错误/重试/空态；Plan；Browser 导航、后退/前进/刷新、系统浏览器打开、多标签、浮窗、全屏；右栏分栏/调宽/浮窗/全屏；上下文统计；系统浏览器链接；打开配置文件；字体设置。

### UI_ONLY（视觉完成，底层未接入）

Files 目录树；Preview 文件读取/渲染数据；交互式 Terminal/PTY；Deliverables 文件清单；插件目录/市场；自动化调度；语言切换；快捷键编辑；排队消息面板；Session Log 上传；添加 Provider；Agent 预设切换；置顶/Fork/取消归档；@ 文件/会话引用；登录/意见反馈。

### NOT_VERIFIED

真实 Provider 下的工具链、审批、长会话；Windows；macOS 原生全屏；真实 IME 长文本；原生窗口级截图（当前为 webContents；Browser 原生页面不在 CDP 截图内）。

## 回归

### 单元与静态检查

- `npm test`：125/125 通过。
- `npm run build`：通过。
- `npm run typecheck`：通过。
- `npm run lint`：通过。

### 迁移并通过的验收脚本

| 脚本 | 运行方式 | 结果 | 证据 |
| --- | --- | --- | --- |
| 真实 Provider 会话/Runtime 复用 | `LEVELER_HOME=<isolated> npm run test:acceptance` | PASS | `evidence/real-provider/` |
| 审批/取消/resume/过期审批 | `npm run test:interactions` | PASS | `evidence/interactions-evidence.json` |
| Markdown/搜索/链接/长流 | `npm run test:conversation` | PASS | `evidence/conversation-evidence.json` |
| Composer/附件/模型/权限/草稿 | `npm run test:composer` | PASS | `evidence/composer-evidence.json` |
| 合成 100 轮/300 工具 | `npm run test:long-session` | PASS | `evidence/long-session-evidence.json` |
| 会话创建/模型/归档 | `npm run test:session-actions` | PASS | `evidence/session-actions-evidence.json` |
| 既有设置查询 | `node scripts/settings-queries.mjs` | PASS | `evidence/settings-queries-evidence.json` |
| 上传未知结果/放弃 | `node scripts/upload-deadline.mjs` | PASS | `evidence/upload-deadline-evidence.json` |
| 真实 Plan/Git diff | `node scripts/views.mjs` | PASS | `evidence/views-evidence.json` |
| Browser 浮窗/全屏/多标签/外部打开 | `node scripts/browser-dock.mjs` | PASS | `evidence/browser-dock-evidence.json` |

真实 Provider 运行使用隔离 `LEVELER_HOME`；密钥只在本地运行期存在，未写入本报告或证据 JSON。该运行覆盖普通对话、会话切换、关闭/重开同 Runtime、无工作区、窄窗，不覆盖真实模型工具链。

### 仍未迁移

`product.mjs`、`browser-native.mjs`、`commands-settings.mjs`、`project-interactions.mjs`、`responsive-long-home.mjs`、`responsive-theme.mjs`、`settings-branches.mjs`、`sidebar-compact.mjs`、`workbench.mjs`、`conversation-runtime.mjs` 仍绑定旧 DOM/旧信息架构，未执行。它们不能作为本轮回归证据。

## 源码

| 文件 | 变更 |
| --- | --- |
| `src/index.html` | 新 Shell、Composer、右栏、Settings、takeover |
| `src/renderer.mjs` | 完整渲染器：会话/工具卡/Composer/Dock/Settings/搜索/Browser |
| `src/styles.css` | 视觉系统、浅/深色、响应式、Dock/浮窗/工具卡/字体设置 |
| `src/icons.mjs` / `src/ui.mjs` | 独立 SVG 图标、菜单/浮层/Toast/Tooltip/Dialog |
| `src/state.mjs` / `src/markdown.mjs` | 每轮失败行；表格横向滚动包装 |
| `src/main.cjs` / `src/preload.cjs` | 系统链接、打开配置文件；资源 allowlist |
| `scripts/*` | 迁移或新增验收脚本 |

未修改 Web、未引入参考项目源码依赖、未改 Runtime 架构。

## Git

- 收口提交：`207a459 desktop: close remaining reference UI gaps`
- 报告/README 单独提交（见最终回复的 SHA）。
- 未创建分支、未 push、未发版。
- 工作区仍有其他并行改动的 `.cargo/config.toml`、仓库根 `README.md`、`.github/workflows/sync-atomgit.yml` 和 `scripts/sweep-debug-artifacts.sh` 删除，未纳入本轮提交。

## 剩余优先级

1. Settings 浅色逐区域收敛；消除可见 badge/禁用态带来的差异，或明确视觉例外。
2. Files/Preview/Terminal/Deliverables 的 bridge 能力（目录读取、文件读取、PTY、交付物清单）或正式接受 UI_ONLY。
3. 真实 Provider 下的工具链、审批、长会话与 IME 验收。
4. 迁移剩余旧脚本，恢复完整回归 gate。
5. 参考动效、Conversation header 全 slot、工具卡细节与 Windows 验证。
