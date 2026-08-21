# leveler-mobile

**状态：FROZEN（tag `mobile-beta-mvp`）。** 功能投入停止，等真实 Beta 用户用过再决定下一轮。权威说明：[`docs/MOBILE_FREEZE.zh-CN.md`](../../docs/MOBILE_FREEZE.zh-CN.md)。

手机端远程控制客户端（iOS / Android，Flutter）。控制你**自己的**开发机上的 CodeLeveler：切换项目、跑任务、处理审批、中途干预、预览已登记产物。

产品收口（工作台，不是 Chat App）：[`docs/MOBILE_UI_UX_CLOSURE.zh-CN.md`](../../docs/MOBILE_UI_UX_CLOSURE.zh-CN.md)。  
Steer / 产物 / Task Detail：[`docs/MOBILE_RUNTIME_ALIGNMENT.zh-CN.md`](../../docs/MOBILE_RUNTIME_ALIGNMENT.zh-CN.md)、[`docs/MOBILE_BETA_CLOSURE.zh-CN.md`](../../docs/MOBILE_BETA_CLOSURE.zh-CN.md)。不推翻本目录已有配对 / 验签 / 会话栈。

---

## 验证到哪一步了

装上 Flutter 3.44.8（Dart 3.12.2）之后跑过：

| 命令 | 结果 |
| --- | --- |
| `flutter pub get` | ✅ 64 个依赖解析通过 |
| `flutter analyze` | ✅ **No issues found** |
| `flutter test` | ✅ 全绿，含跨语言 golden、Steer、事件覆盖、产物卡片 |
| iOS 模拟器（iPhone 17 Pro / iOS 26.5）构建并启动 | ✅ 装进模拟器跑起来了，中文渲染正常，设备密钥真的写进了 Keychain |
| **模拟器上的完整配对验收** | ✅ `scripts/simulator_pairing.sh`：真 relay + 真 agent + 真人工确认，一条命令跑完 |
| Android | ❌ 无 Android SDK，未构建 |
| 真机蜂窝 / 内测包（TestFlight、Play） | ❌ 未做：需要真设备与签名证书 |

**golden 一致性是真的，不是空转。** 牙口验过：把 canonical string 里 `sender_id` 与 `recipient_id` 的位置互换，`envelope_golden_test.dart` 立刻转红并打印精确差异；换回即绿。也就是说这份 Dart 实现与 Rust 侧对着**同一份答案卷** `testdata/signed_envelope.golden.json`：1 条正例逐字节匹配 canonical string，5 条负例（伪造签名、错 recipient、relay 改写 recipient、id 含 `|`、时间戳过期）逐条被拒，指纹算法与电脑端打印的一致。

**模拟器验收跑通了什么：** `./scripts/simulator_pairing.sh <udid>` 启动 relay、注册本机、起 agent、打印配对载荷，然后驱动模拟器上的 APP 粘贴载荷、核对指纹、提交配对，**在电脑确认之前的 4 秒里断言手机仍未配对**（电脑端故意等 8 秒才接受），确认后 APP 取得 token、签名 RPC、验签返回的项目列表并进入项目页。指纹断言比对的是电脑端 `leveler remote status` 打印的那一串——不是「有个指纹」而是「是这台机器的指纹」。

**仍然没有做到的：** 真机、蜂窝网络、内测包、Android。模拟器不是真机：网络栈、后台策略、推送、生物识别在真设备上会不一样。设计里 Phase 1 出门要的是**真机**路径（K23），**那一条仍未满足**。

## 已经写出来的部分

| 层 | 文件 | 说明 |
| --- | --- | --- |
| 信封 | `lib/protocol/envelope.dart` | canonical string、签名、验签、时钟窗、id 字符集。与 Rust 侧共用一份 golden 答案 |
| 会话线协议 | `lib/protocol/wire.dart` | Upstream 封闭、Downstream 开放（未知事件忽略 + 触发 resync） |
| 命令 | `lib/protocol/commands.dart` | **只**构造允许的命令；没有「始终允许」，没有删除/改名/回滚会话 |
| 配对 | `lib/protocol/pairing.dart` | 载荷解析 + 指纹对照 |
| 密钥 | `lib/crypto/keys.dart`、`lib/crypto/store.dart` | Ed25519、指纹、Keychain/Keystore、清除配对 |
| 网络 | `lib/net/relay_client.dart`、`lib/net/session_socket.dart` | REST + WSS；**每一帧先验签再交给 UI** |
| 状态 | `lib/domain/session_state.dart`、`lib/domain/app_controller.dart` | 事件应用规则、连接状态、只读配对拦截 |
| 界面 | `lib/ui/*.dart` | 配对、项目列表、**会话列表**、会话（含审批/澄清）、设置 |
| 测试 | `test/*.dart` | golden 一致性、事件应用规则、id/载荷负例（16 条，全绿） |

## 明确没做的

- **扫二维码**：只做了粘贴。电脑端 `leveler remote pair` 打印的就是可粘贴的一行，设计里把粘贴列为等价路径。
- **Android 构建**：平台目录已生成（最低 API 29 = Android 10），但本机没有 Android SDK，没构建过。
- **附件上传**：电脑端的 `upload_attachment` 也还没实现。
- **未确认指令的重发队列没有单测**：上限 16 条，重连后用**原 `command_id`** 重发（电脑端按该 id 去重，所以重发是同一条指令而不是第二条）。逻辑写了，没跑过。
- **推送通知、生物识别锁**：未做。产物卡片/预览/`fetch_attachment` 已接；系统 Share sheet 未接。
- **内测分发**（TestFlight / Play 内测轨）：需要真机构建，见上。

## 安全上的硬规则（代码里已体现）

1. 私钥只在 Keychain / Keystore，不进日志、不进备份（iOS 用 `first_unlock_this_device` 且 `synchronizable: false`）。
2. `runtime_pubkey` 在**配对时锚定**；此后所有 `sender=runtime` 的帧都用它验签，**绝不**用 relay 后来给的密钥。
3. 验签失败的帧**不进任何 UI 状态**，并以「无法验证」而非「网络错误」呈现——两者对用户意味着完全不同的事。
4. RPC 响应必须复用请求的 `stream_id`（该值在签名内），否则拒收：relay 不能拿另一个请求的合法响应来顶替。
5. 只读配对在客户端就挡住发送，而不是让用户打完字再被电脑端拒绝。

## 用 Xcode 打开

```bash
open ios/Runner.xcworkspace     # 必须是 .xcworkspace，不是 .xcodeproj（有 CocoaPods）
```

**如果 Xcode 报 `Command PhaseScriptExecution failed with a nonzero exit code`：**
多半是 `ios/Flutter/Generated.xcconfig` 里的 `FLUTTER_TARGET` 指向了一个已被删除的临时文件——`flutter test integration_test/...` 会把它改成测试入口且不还原。真正的错误藏在日志深处（`listener.dart: No such file or directory`），表面那句什么都没说。

修复：

```bash
flutter build ios --simulator --debug     # 会把 FLUTTER_TARGET 写回 lib/main.dart
```

`simulator_pairing.sh` 现在退出时会自己还原，所以只有手动裸跑集成测试才会碰到。

## 本地怎么跑（等你有工具链时）

```bash
cd apps/leveler-mobile
flutter pub get
flutter analyze       # 应当 No issues found
flutter test          # golden 测试会去读 ../../testdata/signed_envelope.golden.json
```

iOS 构建还需要 CocoaPods（已装）与 Xcode 的 iOS 平台组件；Android 需要 Android SDK。

golden 测试读的是仓库根部 `testdata/signed_envelope.golden.json`——**与 Rust 侧同一份文件**。协议改了而这里没跟上，它会红。
