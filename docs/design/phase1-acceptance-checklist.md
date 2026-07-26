# Phase 1 验收清单（PR11f）

设计对 Phase 1 出门的定义（K23）：**「缺 APP 真机路径不得宣称 Phase 1 完成」**。这份清单把验收路径逐条列出来，标注**现在处于哪一档**：

| 档 | 含义 |
| --- | --- |
| ✅ **自动化** | 有测试守着，CI 每次都会跑 |
| 🖐️ **已手工验过** | 我在这台机器上真跑过一次，但没有自动化 |
| ⏳ **待真机** | 代码已写、能编译，但需要设备或模拟器才能验，**尚未做** |
| ⛔ **被阻塞** | 依赖尚未存在的东西 |

**当前总状态：Phase 1 未完成。** 后端链路可自证；APP 能编译、协议层测试全绿，但从未在真机或模拟器上跑起来过。

---

## 一、配对

| 验收项 | 档 | 依据 |
| --- | --- | --- |
| 电脑生成配对载荷（含 128-bit secret + runtime_pubkey） | 🖐️ | `leveler remote pair`，实跑输出见 PR7 记录 |
| 手机提交 `pair/complete` | 🖐️ | 手工用 curl 模拟手机侧完成 |
| 电脑显示 device 指纹并要求人工确认 | 🖐️ | `leveler remote confirm`，实跑 |
| 指纹两端算法一致 | ✅ | `testdata/signed_envelope.golden.json` 的 `device_fingerprint`，Rust 与 Dart 各有测试读同一份 |
| 拒绝配对不留任何本机记录 | ✅ | `relay_api.rs::confirming_a_pairing_requires_the_runtimes_signature` |
| **手机上扫码/粘贴 → 显示指纹 → 用户比对** | ⏳ | 粘贴路径已写并能编译，未在设备上看过；扫码未做 |

## 二、多项目

| 验收项 | 档 | 依据 |
| --- | --- | --- |
| 项目列表 ≥2，签名信封 | ✅ | `projects.rs::the_project_list_is_signed_by_the_runtime` |
| 两个项目分别 deliver，互不可见 | ✅ | `projects.rs::each_projects_commands_reach_only_that_project` |
| **事件不串台** | ✅ | `end_to_end.rs::events_do_not_cross_between_two_open_projects`（双向验，且断言另一条流静默） |
| 单个项目 offline 隔离，其余可用 | ✅ | `projects.rs::an_offline_project_is_isolated_from_the_rest` |
| 未知 project 的 stream 被拒 | ✅ | `end_to_end.rs::a_stream_for_an_unknown_project_is_closed` |
| `leveler remote projects` 列出状态 | 🖐️ | 实跑（空注册表场景） |
| 真注册表 + 两个活 daemon | ✅ | `projects.rs::the_router_attaches_to_live_daemons_and_reports_the_rest_offline` |
| **手机上切换项目，界面不串台** | ⏳ | 代码已写并能编译，未在设备上验过 |

## 三、对话与流式

| 验收项 | 档 | 依据 |
| --- | --- | --- |
| 手机 deliver 穿过不受信 relay 抵达 runtime | ✅ | `end_to_end.rs::a_phone_command_crosses_the_relay_and_reaches_the_runtime` |
| 回程 ack 可用 QR 锚定的 runtime_pubkey 验签 | ✅ | 同上（`recv_verified`） |
| 助手输出下行 | ✅ | `end_to_end.rs::runtime_events_reach_the_phone_signed` |
| 订阅 lag → `resync_required` 并关流 | ✅ | `end_to_end.rs::a_lagged_subscriber_is_told_to_resync_and_the_stream_ends` |
| 杀 APP 进程后重连 + snapshot | ✅ | `phase1_acceptance.rs` 第 6 步 |
| `command_id` 重试有界 | ⚠️ | APP 里已写（上限 16、重连原 id 重发），能编译，**没有单测** |
| **真机蜂窝网络下的流式体验** | ⏳ | 需要真机 |

## 四、审批

| 验收项 | 档 | 依据 |
| --- | --- | --- |
| 远程 `ApproveAlways` 被拒 | ✅ | `end_to_end.rs::a_disallowed_command_is_refused_across_the_whole_chain` |
| remote-only 审批 120s 自动拒绝 | ✅ | `approval_timeout.rs` 7 条 + `phase1_acceptance.rs` 第 5 步 |
| 本机有人时不超时 | ✅ | `approval_timeout.rs::a_local_ui_keeps_the_approval_alive` / `both_attached_means_no_countdown` |
| 本机 UI 离开后重新武装计时 | ✅ | `approval_timeout.rs::the_countdown_starts_when_the_last_local_ui_leaves` |
| **APP 审批界面无「始终允许」** | ⏳ | 代码里已确保（`ApprovalChoice` 只有三项），未在真机上看过 |
| 澄清空答=跳过 | ⏳ | 代码里已写，未验 |
| 审批 pending + runtime 重启后恢复 | ⛔ | 属 `leveler-app` 行为，远程侧测不出；见 PR9 说明 |

## 五、撤销与离线

| 验收项 | 档 | 依据 |
| --- | --- | --- |
| 撤销后 token 立即失效 | ✅ | `relay_api.rs::revoking_a_device_invalidates_its_tokens_at_once` |
| 撤销关闭在途流 | ✅ | `forwarding.rs::revoking_a_device_closes_its_live_stream` |
| 运行中的 agent 下一帧即拒 | ✅ | `admission.rs::a_revocation_lands_on_the_next_frame_of_a_running_agent` |
| host 全离线 → 503 + Retry-After，不排队 | ✅ | `rpc_proxy.rs::an_rpc_for_an_offline_host_is_refused_immediately` |
| in-flight RPC 遇 agent 掉线立即结束 | ✅ | `rpc_proxy.rs::an_agent_disappearing_mid_rpc_ends_the_request` |
| **手机上看到「已撤销」并清除本地密钥** | ⏳ | 设置页已写「清除配对」，未验 |

## 六、APP 交付

| 验收项 | 档 | 依据 |
| --- | --- | --- |
| `flutter pub get` / `analyze` / `test` 通过 | 🖐️ | Flutter 3.44.8 实跑：依赖解析通过、`analyze` 无问题、**16 条测试全绿** |
| golden 向量跨语言一致 | 🖐️ | `envelope_golden_test.dart` 读 Rust 同一份 `testdata/signed_envelope.golden.json`；牙口已验（互换 canonical 字段即红） |
| **APP 在真机/模拟器上跑起来** | ⛔ | 平台目录未生成；无 Android SDK、无 iOS 模拟器 runtime、无 CocoaPods、无设备 |
| iOS TestFlight 包 | ⛔ | Xcode 26.6 在，但缺平台目录、CocoaPods、模拟器 runtime 与签名证书 |
| Android 内测 APK | ⛔ | 无 Android SDK |
| 真机蜂窝闭环 | ⛔ | 无设备 |

---

## 结论

**可以说：** host + relay + CLI 这条链在完整性上自证，验收路径的**后端部分**已自动化（`phase1_acceptance.rs` 一次走完），CLI 全流程手工实跑通过。

**不能说：** Phase 1 完成。APP 的编译与协议测试已经绿了，但第六节后四项仍被阻塞在「没有平台目录 / 没有 Android SDK / 没有 CocoaPods 与模拟器 runtime / 没有设备」——**这个 APP 一次都没跑起来过**，界面是否可用完全未知。

**下一步最短路径：** 平台目录 + CocoaPods（iOS）或 Android SDK，然后让 APP 第一次真正跑起来。`analyze` 与 `test` 已经绿了，证明的是「能编译、协议逻辑对」；它不证明这个 APP 能用。
