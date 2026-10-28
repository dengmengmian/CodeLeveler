# CodeLeveler Desktop

桌面客户端目前需要从源码运行，正式发布包仍只包含命令行程序。

## 启动

需要 Rust、Node.js、npm 和 Git。在仓库根目录执行：

```sh
npm --prefix crates/leveler-web/web ci
npm --prefix crates/leveler-web/web run build
cargo build -p leveler-cli --bin leveler
cd apps/leveler-desktop
npm ci
npm start
```

默认使用 `target/debug/leveler`，也可以用 `LEVELER_BINARY` 指定二进制的绝对路径。先通过 `leveler login` 配置模型；桌面端沿用已有配置。

## 可以做什么

- 创建、继续、搜索和归档会话，切换模型与权限。
- 查看工具执行、文件改动、计划、后台任务和会话轨迹。
- 处理审批、回答问题、取消任务，关闭后恢复会话。
- 使用文件预览、浏览器和内置终端，调整主题、字号和面板布局。

## 当前限制

- 插件市场、自动化调度、登录与反馈提交等部分入口尚未接入，界面会提示不可用。
- 文件预览暂不支持 PDF 和 Office；工具卡片中的长输出会截断，桌面端暂无完整输出入口。
- 内置终端按当前系统用户权限运行，不受 Agent 权限模式的写入和网络限制，也不会请求审批。Windows 暂不支持内置终端。
- 切换会话期间的后台输出不会补回，任务状态仍会保留。
- 子 Agent 状态可查看，子会话打开和输入暂未接入。
- Windows 运行、macOS 原生全屏和真实输入法长文本尚未验证；「在应用中打开」目前仅在 macOS 验证。

安装命令行和配置模型见[首页](../../README.zh-CN.md)。
