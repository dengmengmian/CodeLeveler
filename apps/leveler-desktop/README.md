# CodeLeveler Desktop

桌面客户端目前需要从源码运行，正式发布包仍只包含命令行程序。

## 启动

需要 Rust、Node.js、npm 和 Git。在仓库根目录执行：

```sh
cargo build -p leveler-cli --bin leveler
cd apps/leveler-desktop
npm ci
npm start
```

默认使用 `target/debug/leveler`，也可以用 `LEVELER_BINARY` 指定二进制的绝对路径。先通过 `leveler login` 配置模型；桌面端沿用已有配置。

## 可以做什么

- 创建、继续、搜索、归档和恢复会话，切换模型与权限。
- 查看工具执行、文件改动、计划、后台任务和会话轨迹。
- 处理审批、回答问题、取消任务，关闭后恢复会话。
- 创建和管理自动化任务，授权后运行并查看执行记录。
- 从工作区文件预览选择“引用到消息”，把相对路径加入草稿。Agent 按当前权限读取文件。图片可通过输入框的“添加图片”发送给支持视觉的模型。
- 使用文件预览、浏览器和内置终端，调整主题、字号和面板布局。

## 当前限制

- 计划模式切换和会话引用尚未接入；插件系统未开放。反馈会复制到剪贴板并打开 GitHub Issues，由用户自行提交，不会自动上传日志。
- 文件预览暂不支持 PDF 和 Office；工具卡片中的长输出会截断，桌面端暂无完整输出入口。
- 内置终端按当前系统用户权限运行，不受 Agent 权限模式的写入和网络限制，也不会请求审批。Windows 暂不支持内置终端。
- 切换会话期间的后台输出不会补回，任务状态仍会保留。
- 子 Agent 状态可查看，子会话打开和输入暂未接入。
- Windows 运行、macOS 原生全屏和真实输入法长文本尚未验证；「在应用中打开」目前仅在 macOS 验证。

安装命令行和配置模型见[首页](../../README.zh-CN.md)。

## 开发检查

运行 `npm run build`、`npm run typecheck`、`npm run lint` 和 `npm test`。构建本地 Runtime 后，使用 `npm run test:smoke` 验证会话恢复、归档恢复、图片附件、自动化运行和定时任务工具卡。冒烟使用隔离配置、临时工作区和测试专用模型服务，不访问现有会话或外部模型。

自动化的“立即运行”通过调度工具使用 `leveler schedule dispatch --occurrence <id> --follow`：只执行该 run-now 槽位并跟随到终态；不会开启系统后台唤醒，也不会继续派发未来的定时任务。桌面设置中的 Runtime 更新只针对命令行运行时，不更新桌面客户端。
