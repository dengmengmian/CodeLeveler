# CodeLeveler 移动端

移动端是冻结的源码预览，尚未提供 TestFlight、Play 或其他正式安装包。

iOS / Android 客户端用于远程连接自己的开发机，查看项目与会话、运行任务、处理审批和预览产物。

## 使用前须知

- 目前仅验证过 iOS 模拟器配对流程；Android 构建、真机和蜂窝网络未验证。
- 配对使用粘贴载荷，暂不支持扫码。
- 远程会话流量经过 relay。消息有签名，但尚无端到端加密；评估时应自行部署 relay。
- 附件上传、推送通知和生物识别锁尚未实现。

## 从源码运行

需要 Flutter 工具链。iOS 还需要 Xcode 和 CocoaPods；Android 需要 Android SDK。

```sh
cd apps/leveler-mobile
flutter pub get
flutter run
```

通过 Xcode 打开时使用 `ios/Runner.xcworkspace`。

命令行的安装和配置见[首页](../../README.zh-CN.md)。
