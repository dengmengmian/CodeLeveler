# Phase 1 验收清单（PR11f）

设计对 Phase 1 出门的定义（K23）：**「缺 APP 真机路径不得宣称 Phase 1 完成」**。这份清单把验收路径逐条列出来，标注**现在处于哪一档**：

| 档 | 含义 |
| --- | --- |
| ✅ **自动化** | 有测试守着，CI 每次都会跑 |
| 🖐️ **已手工验过** | 我在这台机器上真跑过一次，但没有自动化 |
| ⏳ **待真机** | 代码已写、能编译，但需要设备或模拟器才能验，**尚未做** |
| ⛔ **被阻塞** | 依赖尚未存在的东西 |

**当前总状态：Phase 1 未完成。** 后端链路可自证；APP 已在 iOS 模拟器上跑通完整配对验收。**差的是真机**——模拟器不是真机，且 Android 与内测包均未做。

---

## 一、配对

| 验收项 | 档 | 依据 |
| --- | --- | --- |
| 电脑生成配对载荷（含 128-bit secret + runtime_pubkey） | 🖐️ | `leveler remote pair`，实跑输出见 PR7 记录 |
| 手机提交 `pair/complete` | 🖐️ | 手工用 curl 模拟手机侧完成 |
| 电脑显示 device 指纹并要求人工确认 | 🖐️ | `leveler remote confirm`，实跑 |
| 指纹两端算法一致 | ✅ | `testdata/signed_envelope.golden.json` 的 `device_fingerprint`，Rust 与 Dart 各有测试读同一份 |
| 拒绝配对不留任何本机记录 | ✅ | `relay_api.rs::confirming_a_pairing_requires_the_runtimes_signature` |
| **手机上粘贴 → 显示指纹 → 用户比对** | ✅ | `integration_test/pairing_flow_test.dart` 在模拟器上真跑，并断言显示的是**电脑那把密钥**的指纹；扫码未做 |
| **手机不能自己给自己配对** | ✅ | 同上：电脑故意等 8 秒，测试在这段窗口内断言手机仍未配对 |

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
| 配对后进入项目页（含签名 RPC 验签） | ✅ | 同上，测试末尾断言到达项目页且连接未进入 `untrusted` |
| **手机上切换项目，界面不串台** | ✅ | `multi_project_test.dart` 在模拟器上真跑：两个在线项目，A 的会话与消息都不出现在 B |

## 三、对话与流式

| 验收项 | 档 | 依据 |
| --- | --- | --- |
| 手机 deliver 穿过不受信 relay 抵达 runtime | ✅ | `end_to_end.rs::a_phone_command_crosses_the_relay_and_reaches_the_runtime` |
| 回程 ack 可用 QR 锚定的 runtime_pubkey 验签 | ✅ | 同上（`recv_verified`） |
| 助手输出下行 | ✅ | `end_to_end.rs::runtime_events_reach_the_phone_signed` |
| 订阅 lag → `resync_required` 并关流 | ✅ | `end_to_end.rs::a_lagged_subscriber_is_told_to_resync_and_the_stream_ends` |
| 杀 APP 进程后重连 + snapshot | ✅ | `phase1_acceptance.rs` 第 6 步；APP 侧 `multi_project_test.dart` 末段（重建控制器与 socket，只留存储） |
| `command_id` 重试有界 | ⚠️ | APP 里已写（上限 16、重连原 id 重发），能编译，**没有单测** |
| **真机蜂窝网络下的流式体验** | ⏳ | 需要真机 |

## 四、审批

| 验收项 | 档 | 依据 |
| --- | --- | --- |
| 远程 `ApproveAlways` 被拒 | ✅ | `end_to_end.rs::a_disallowed_command_is_refused_across_the_whole_chain` |
| remote-only 审批 120s 自动拒绝 | ✅ | `approval_timeout.rs` 7 条 + `phase1_acceptance.rs` 第 5 步 |
| 本机有人时不超时 | ✅ | `approval_timeout.rs::a_local_ui_keeps_the_approval_alive` / `both_attached_means_no_countdown` |
| 本机 UI 离开后重新武装计时 | ✅ | `approval_timeout.rs::the_countdown_starts_when_the_last_local_ui_leaves` |
| **APP 审批界面无「始终允许」** | ✅ | `pairing_flow_test.dart` 在模拟器上断言该按钮不存在，且屏幕上有解释 |
| 澄清空答=跳过 | ⏳ | 代码里已写，未验 |
| 审批 pending + runtime 重启后**不**恢复 | ✅ | `leveler-app/tests/pending_approval_restart.rs`：重启后不重建待批审批（按下去解决不了任何东西），且未批准的命令确实没执行；APP 侧在快照没有它时清掉卡片 |

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
| `flutter pub get` / `analyze` / `test` 通过 | 🖐️ | Flutter 3.44.8 实跑：依赖解析通过、`analyze` 无问题、**25 条测试全绿** |
| golden 向量跨语言一致 | 🖐️ | `envelope_golden_test.dart` 读 Rust 同一份 `testdata/signed_envelope.golden.json`；牙口已验（互换 canonical 字段即红） |
| **APP 在 iOS 模拟器上跑起来** | ✅ | iPhone 17 Pro / iOS 26.5；两条集成用例由脚本一条命令跑完 |
| **模拟器端到端配对验收** | ✅ | `scripts/simulator_pairing.sh` 一条命令：真 relay + 真 agent + 真确认 |
| **模拟器上跑完一整段会话** | ✅ | 同一脚本续跑：新建会话 → 中文提问（Markdown 渲染，标题/列表/代码块）→ 英文提问 → 审批卡片 → 允许一次 → `rm` 真的执行（脚本核对 scratch.txt 已消失）→ 收尾回答。模型由 `scripts/scripted_provider.py` 固定，所以断言的是「屏幕该显示什么」而不是「模型这次说了什么」 |
| **`/remote loc` 用户路径** | ✅ | `scripts/tui_remote_pairing.py`：pty 里跑真 TUI，输入 `/remote loc` → 屏幕上真画出二维码 → 手机读载荷 → TUI 显示手机指纹并等按键 → `y` → 上面那整段会话再走一遍。指纹由载荷里的公钥现算，不是从同一块屏幕上抄的 |
| **APP 内切换项目不串台** | ✅ | `multi_project_test.dart`：电脑上开两个仓库，A 建会话发消息 → 退出 → 进 B，断言 A 的会话不在 B 的列表里、B 的对话里没有 A 的消息 |
| **杀 APP 后 resync** | ✅ | 同一用例：整棵 widget 树拆掉、换一个 `AppController`、新 socket，只有存储留下来；重开会话后之前说的话由**快照**带回。进程本身没真杀（集成测试会一起死），所以「iOS 冷启动后还给不给 keychain」这一层没验 |
| **取消当前回合** | ✅ | `pairing_flow_test.dart`：让模型给一段慢回答，流到一半按停止；provider 端看到连接被切断，文本停止增长且没跑到最后一句 |
| **项目离线时 APP 的显示** | ✅ | `offline_and_revoke_test.dart`：停掉一个 daemon，列表里它仍在、标为「离线（电脑上未运行）」，另一个照常可用 |
| **撤销后 APP 的表现** | ✅ | 同一用例：电脑撤销后，手机下一次取列表即失败并显示「已被撤销…请重新配对」；配对**不**自动清除——一次失败不该把网络抖动变成重新配对 |
| **待批审批 + 进程重启** | ✅ | `leveler-app/tests/pending_approval_restart.rs`：重启后待批审批**不**重建——按下去解决不了任何东西的按钮不该出现；APP 侧对应地会在快照里没有它时清掉卡片 |
| Android 构建 | ⛔ | 无 Android SDK |
| iOS TestFlight 包 | ⛔ | 缺签名证书（平台目录、CocoaPods、模拟器 runtime 均已就位） |
| Android 内测 APK | ⛔ | 无 Android SDK |
| 真机蜂窝闭环 | ⛔ | 无设备 |

---

## 结论

**可以说：** host + relay + CLI 这条链在完整性上自证，验收路径的**后端部分**已自动化（`phase1_acceptance.rs` 一次走完），CLI 全流程手工实跑通过。

**不能说：** Phase 1 完成。iOS 模拟器上这条链已经从 `/remote loc` 一路走到审批执行，但 Android 与真机（签名、蜂窝网络、真 relay）仍然一次都没跑过。中文**输入法**也只验到了「文本进得去、显示得出来」这一层：模拟器上的 enterText 走的是文本输入通道，不是拼音候选框。

**下一步最短路径：** 一台真 iPhone + 开发者签名，把 `tui_remote_pairing.py` 的同一套流程在真机蜂窝网络下跑一遍；以及装 Android SDK 补上另一端。
