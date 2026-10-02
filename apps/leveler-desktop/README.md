# CodeLeveler Desktop

Electron 是桌面壳。任务、对话、工具、审批和持久化来自现有 Rust Runtime。Renderer 通过 sandbox preload 的固定 IPC 方法访问 Main，Main 只启动 `leveler desktop-bridge` JSONL 客户端。Runtime 的 discovery、spawn、adopt、revive 和 handoff 复用 Rust Runtime Host。

```sh
cargo build -p leveler-cli --bin leveler
cd apps/leveler-desktop
npm ci
npm start
```

默认使用仓库的 `target/debug/leveler`。可通过 `LEVELER_BINARY=/absolute/path/to/leveler` 指定二进制；`LEVELER_HOME` 和现有模型配置由 bridge 继承。未配置 provider 的错误会显示在对话中。

点击「新建任务」创建 No Workspace 任务；也可直接输入消息后发送。选择文件夹后，下一次发送会创建 Workspace 任务。左侧历史任务来自 Runtime 的全局索引，点击任务恢复正文、工具历史和仍有 live waiter 的审批。Enter 发送，Shift + Enter 换行。关闭窗口退出 Desktop 和 bridge，不发送 Runtime Quit/Shutdown。重开后从历史选择原任务即可 adopt 原 Runtime 并恢复。

```sh
npm run build  # 原生 JS，无 bundler；检查全部源码语法
npm test       # 展示状态与 IPC 边界的最小行为验证
npm run test:interactions # 测试 fixture provider，经真实 Runtime 验证 Workspace / 工具 / 审批
LEVELER_HOME=/isolated/configured/home npm run test:acceptance
```

真实验收需要隔离的 `LEVELER_HOME` 和可用的真实 provider 配置。脚本启动实际 Electron，创建 No Workspace Task、发送真实请求、切换历史、关闭窗口，观测 Runtime PID 存活，再重开并比对 PID、RuntimeId、task owner_epoch 和原消息。证据及截图写入忽略目录 `acceptance-output/`，不输出 provider 配置或凭据。验收结束仍保留 Runtime 运行，符合桌面退出契约。

Desktop 采用任务导航、Conversation 和可关闭的右侧工作台。工作台默认打开固定概览，加号创建手动 Browser 标签；概览入口保留，扩展工作台可占满应用内容，退出恢复原布局和草稿。工作台提供 Runtime 实际提供的 Plan / Changes，变更可从概览请求并刷新。配色沿用 CodeLeveler Web 的 Paper / Graphite 产品主题，WorkBuddy 用于布局与交互参考。浅色、深色、跟随系统均可用；Cmd/Ctrl+B 切换侧栏，Cmd/Ctrl+Shift+I 切换工作台，Escape 关闭工作台。工作台支持拖动或方向键调整宽度并记住用户宽度，窄窗使用浮层。

对话支持安全 Markdown、代码与消息复制、当前正文搜索和标题编辑。模型与权限菜单修改当前会话，等待 Runtime 确认后显示结果，不改全局默认配置。任务归档后从默认索引隐藏，记录保留；当前没有取消归档操作。

输入框加号上传用户选择的附件，与工作空间目录选择分开。Main 限制读取大小，Runtime 保存与处理附件，结果按原命令编号关联。图片仅在当前模型支持 vision 时随消息发送；普通文件当前可上传保存，但不能送给模型读取，界面明确提示并保留草稿。上传结果未知时可按原编号重试，或放弃本地等待；放弃等待不取消 Runtime 导入或删除已保存对象。

Browser 使用 Electron Main 管理的独立临时 `WebContentsView`，支持标签、地址、后退/前进、刷新和主动外部打开。网页不接收 Desktop preload、Node 或 Runtime 凭据；权限、下载、popup 和非 HTTP(S) 页面协议被拒绝。输入域名按 HTTPS 打开，不提供搜索引擎猜测。

**手动 Browser 尚未绑定 Runtime Agent Browser。** Agent 浏览器工具活动只展示调用时报告，点击打开工作台对应详情，不声明实时页面同步。关闭 Desktop 会回收手动网页；Runtime Browser 保留自己的生命周期。没有完整 Files/Preview、Artifacts、PTY/Terminal、Skills/MCP 管理、Monaco 或完整 Workspace UI。

```sh
npm run typecheck       # strict checkJs：Browser policy/controller、展示标签投影
npm run lint            # 全部 Desktop JS 的正确性规则
npm test                # 状态、IPC、安全、展示投影
npm run build           # 原生模块语法检查；不声称打包安装器
npm run test:product    # 实际 Electron + Rust Runtime，test-only provider；真实网页、安全探针、原生窗口截图
npm run test:long-session # test-only JSONL 压力验收：100 turns / 300 tools / 大 reasoning
node scripts/workbench.mjs # 固定概览、扩展恢复、宽度与窄屏焦点
node scripts/views.mjs # 真实 Git 变更、活动计划、错误重试和长补丁
node scripts/browser-native.mjs # 实际原生页面正文、导航、隔离和完整窗口截图
node scripts/upload-deadline.mjs # 真实导入、延迟结果、放弃等待和迟到事件
```

验收脚本使用独立 Electron profile。产品截图使用已有屏幕录制权限的原生窗口捕获，准确包含 Browser 子视图；没有权限时会显式报证据采集失败，不要求授予权限或降低应用安全。完整范围、Browser 审计、证据等级及已知缺口见本文档，以及各验收脚本写入 `acceptance-output/` 的证据。

`test:interactions` 只在测试中启动 fixture provider，并通过 Playwright 在 Electron Main 替换原生 picker 的返回值；它证明 folder token、Workspace 绑定和真实 Runtime 工具/审批接线，不声明实际操作了 OS 文件夹选择窗口。脚本观测真实文件读取预览、危险删除动作待审批、拒绝后文件仍在、切换历史后工具恢复且审批不复活。测试 Runtime 使用独立短 `/tmp` home 和 30 秒 idle timeout。

已有旧任务库可能在全局只读索引中返回 `no such column: owner_boot_id`。Desktop 显示合并诊断并保留来源错误，不自动迁移数据库；新库验收通过不代表旧库兼容问题已修复。
