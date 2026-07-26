# leveler-mobile

手机端远程控制客户端（iOS / Android，Flutter）。控制你**自己的**开发机上的 CodeLeveler：切换项目、对话、处理审批、取消回合。

---

## ⚠️ 这份代码从未被编译或运行过

写它的环境里**没有 Flutter / Dart 工具链**（`flutter` 与 `dart` 都不存在）。因此：

| 项 | 状态 |
| --- | --- |
| `flutter pub get` | **未跑过** |
| `flutter analyze` | **未跑过**——很可能有编译错误 |
| `flutter test` | **未跑过**——包括本目录下的 golden 测试 |
| iOS / Android 真机 | **未跑过**，没有内测包 |
| 依赖 API 是否与所选版本一致 | **未验证**（`cryptography` / `flutter_secure_storage` / `web_socket_channel` 的调用是按记忆写的） |

CI 里加了 `mobile` job，**它会是第一个真正编译这份代码的地方**。第一次很可能是红的。把它当作「待修的初稿」，不要当作「已完成的 APP」。

设计文档把「真机跑通」列为 Phase 1 出门条件（K23）。**这条尚未满足。**

---

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
| 界面 | `lib/ui/*.dart` | 配对、项目列表、会话、审批、设置 |
| 测试 | `test/*.dart` | golden 一致性、事件应用规则、id/载荷负例 |

## 明确没做的

- **扫二维码**：只做了粘贴。电脑端 `leveler remote pair` 打印的就是可粘贴的一行，设计里把粘贴列为等价路径。加扫码要引入相机插件，而这份代码还没被编译过，不适合再叠一个未验证依赖。
- **附件上传**：电脑端的 `upload_attachment` 也还没实现。
- **推送通知、生物识别锁、产物下载**：Phase 2（PR12）。
- **内测分发**（TestFlight / Play 内测轨）：需要真机构建，见上。

## 安全上的硬规则（代码里已体现）

1. 私钥只在 Keychain / Keystore，不进日志、不进备份（iOS 用 `first_unlock_this_device` 且 `synchronizable: false`）。
2. `runtime_pubkey` 在**配对时锚定**；此后所有 `sender=runtime` 的帧都用它验签，**绝不**用 relay 后来给的密钥。
3. 验签失败的帧**不进任何 UI 状态**，并以「无法验证」而非「网络错误」呈现——两者对用户意味着完全不同的事。
4. RPC 响应必须复用请求的 `stream_id`（该值在签名内），否则拒收：relay 不能拿另一个请求的合法响应来顶替。
5. 只读配对在客户端就挡住发送，而不是让用户打完字再被电脑端拒绝。

## 本地怎么跑（等你有工具链时）

```bash
cd apps/leveler-mobile
flutter pub get
flutter analyze
flutter test          # golden 测试会去读 ../../testdata/signed_envelope.golden.json
flutter run
```

golden 测试读的是仓库根部 `testdata/signed_envelope.golden.json`——**与 Rust 侧同一份文件**。协议改了而这里没跟上，它会红。
