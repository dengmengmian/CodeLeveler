# 远程控制安全门禁记录（PR10）

| 字段 | 值 |
| --- | --- |
| **范围** | `leveler-remote-protocol` / `leveler-remote-agent` / `services/leveler-relay` / `leveler remote` CLI |
| **设计** | `docs/design/remote-app-control.md`（rev.6） |
| **日期** | 2026-07-26 |
| **结论** | **未通过生产宣传门禁。** 后端链路可自证，APP 已在 iOS 模拟器上整链跑通，但 Phase 1 的出门条件包含**真机** APP 路径（K23），真机与 Android 仍未跑过。见「结论与门禁状态」。 |

本文件是**评审记录**，不是营销材料。每一条缓解都指到具体代码与**能跑的测试**；每一条残留风险都写清楚「谁会受影响、在什么条件下」，不写「已按最佳实践处理」。

---

## 一、威胁模型逐条核对

| 威胁 | 严重度 | 代码里的缓解 | 证明它的测试 | 残留 |
| --- | --- | --- | --- | --- |
| 被入侵的 relay 伪造命令 | 关键 | 每个业务帧都是 `SignedEnvelope`；agent 只用本机 `devices.json` 的公钥验签，relay 附带的密钥**只做比对**（`bridge.rs` 准入顺序：信任 → 验签 → 解析 → policy → deliver） | `mock_relay.rs::a_relay_authored_response_is_rejected_by_the_device`、`end_to_end.rs::a_relay_fabricated_frame_is_refused_by_the_agent`、`admission.rs::a_relay_supplied_key_is_never_used_to_verify` | 无（relay 无任何设备/运行时私钥） |
| 配对时 relay 替换 device_pubkey | 关键 | 指纹在本机终端显示并由人比对；**accept 时才**写 `devices.json`；后续永不用 relay 的密钥覆盖 | `relay_api.rs::the_pending_confirmation_exposes_the_device_public_key`、`admission.rs::a_relay_supplied_key_is_never_used_to_verify`、CLI 手工实跑 | 用户不比对指纹就点接受——这是**人**的环节，代码无法代劳，只能把它摆在必经路径上 |
| 伪造 REST snapshot / bootstrap | 高 | `create_session` / `snapshot` / `list_projects` 的响应都是 **runtime 签名信封**；relay 原样转发不拆包 | `mock_relay.rs::a_relay_edited_response_is_rejected`、`rpc_proxy.rs::an_rpc_reaches_the_agent_and_its_signed_answer_comes_back` | 无 |
| 被入侵的 relay 窥读 UI 流量 | 高 | **未缓解**（Phase 1 只有 TLS 传输层） | — | **明确残留**：TLS 终止方可读明文。这正是设计把 self-host 定为隐私默认路径的原因。Phase 2 的 AEAD 未做 |
| 手机被盗 | 高 | access token 15 分钟；refresh 轮换 + 重放视为失窃；本机 `revoke` **下一帧即生效**；远程禁 `ApproveAlways` | `relay_api.rs::a_replayed_refresh_token_costs_the_device_its_session`、`admission.rs::a_revocation_lands_on_the_next_frame_of_a_running_agent` | APP 侧生物锁属 Phase 2；设备私钥的保管取决于手机平台 |
| MITM | 高 | token 只走 `Authorization` 头（禁 query）；断言含时间窗与 nonce | `relay_api.rs::an_auth_assertion_cannot_be_replayed` | TLS 由部署方提供；证书固定未做 |
| 配对码爆破 | 中 | 128-bit CSPRNG secret；哈希存储；constant-time 比较；失败回应与「不存在」一致；`pair/complete` 10 次/分 | `relay_api.rs::a_wrong_pairing_secret_is_refused` | **设计要求的「每 runtime 20 失败/小时后锁定 15 分钟」未实现**，当前只有全局分钟级限流 |
| 审批疲劳 / 无人应答 | 中 | 远程默认 `request_approval`；remote-only 审批 120s 自动拒绝，本机有人时**不**计时 | `approval_timeout.rs` 全部 7 条 | auto-Deny 的 session 取自手机最后一帧（猜错即被 runtime 拒绝，只丢一条日志） |
| 撤销后仍投递 | 高 | relay 删 token + 关在途流；agent **每帧重读** `devices.json` | `relay_api.rs::revoking_a_device_invalidates_its_tokens_at_once`、`forwarding.rs::revoking_a_device_closes_its_live_stream`、`admission.rs::a_revocation_lands_on_the_next_frame_of_a_running_agent` | 无（此前有：agent 缓存设备表，PR7 手工实跑时发现并修） |
| 离线队列放毒 | 高 | relay 结构上没有队列；host 离线一律 503 + `Retry-After`；in-flight RPC 随 agent 断开立即结束 | `rpc_proxy.rs::an_rpc_for_an_offline_host_is_refused_immediately`、`an_agent_disappearing_mid_rpc_ends_the_request` | 无 |
| 串 runtime（跨机转发合法签名帧） | 高 | `recipient_id` 在签名内，验签方核对等于自身 id | `golden_vectors.json` 负例、`admission.rs::a_frame_addressed_to_another_runtime_is_refused` | 无 |
| agent 重启后 ±120s 内 RPC 重放 | 中 | rpc `stream_id` 绑定 + 时间窗；`deliver` 有 `command_id` 幂等兜底 | — | **已知残留**：agent 重启会清空 seq/nonce 记忆。窗口 ≤120 秒 |
| 恶意 APP 供应链 | 高 | 能力表默认拒绝 + 穷尽 match（新命令编译失败）；`ApproveAlways` / `FullAccess` / 删改会话一律拒 | `remote_policy.rs` 全部变体逐条 | APP 分发签名属 PR11f |
| **relay 开放注册（本轮新增核对）** | 高 | 注册需运营者密钥 + 被注册密钥自签；控制面每个请求带**含 action** 的一次性断言 | `relay_api.rs::enrolling_requires_the_operators_secret`、`enrolling_requires_holding_the_key_being_registered`、`beginning_a_pairing_requires_the_runtimes_signature`、`confirming_a_pairing_requires_the_runtimes_signature`、`device_administration_requires_the_runtimes_signature`、`a_runtime_assertion_cannot_be_replayed` | 运营者密钥泄露 = 可注册新 host（但**不能**冒充已注册的 host，因为需要它的私钥） |
| **分隔符歧义（本轮新增核对）** | 中 | 所有 id 在入口即校验 `^[A-Za-z0-9_.:-]{1,64}$`——`pair/complete`、`enroll`、`auth/session` | `relay_api.rs::an_id_that_could_never_be_signed_over_is_refused_at_entry`、`a_runtime_id_that_could_never_be_signed_over_is_refused` | 无（此前：可以注册一个含 `|` 的 device_id，虽不能直接冒充，但会在 relay 上留下一条永远无法发帧的记录） |

### 本轮评审新发现并已修的两项

1. **relay 控制面此前完全无凭据**（`pair/begin`、`pair/confirm`、`devices`、`revoke`）。知道 `runtime_id`（一个名字）即可取走配对密钥、用自己的密钥认领、替主人接受、拿到 routing token，还能撤销别人的设备。E2E 签名让这些帧到 agent 仍被拒，但控制面等于送人。→ 已改为 enroll + 每请求签名断言。
2. **agent 缓存设备表**：运行中配对的手机不被信任，运行中撤销要等重启——CLI 却在嘴上承诺「下一帧即生效」。→ 已改为每次查表读文件。

两项都不是「加固」，是**功能与承诺不符**，各自配了会红的测试。

---

## 二、按类别过一遍（输入 / 注入 / 认证 / 授权 / 密钥 / 加密 / 依赖 / 错误）

| 类别 | 核对结果 |
| --- | --- |
| 输入 | 所有外部 JSON 都是 typed serde；id 入口校验（见上）；帧 ≤1 MiB 由 WS 层限制；envelope 的 `ts` 只接受一种格式，超窗即拒 |
| 注入 | 无 SQL / 无 shell 拼接；canonical string 的分隔符歧义已由 id charset 关闭；日志用结构化字段，不拼接用户输入 |
| 认证 | 三条入口各有凭据：agent tunnel（runtime 签名）、device（设备签名 + nonce）、runtime 控制面（含 action 的断言）。**唯一无凭据的是 `pair/complete`**，那是设计使然（新手机没有任何凭据），由 128-bit secret + 本机确认兜住 |
| 授权 | token 的 `aud` 每次校验；设备只见自己配对的 host；observe 配对拒一切 deliver；能力表默认拒绝 |
| 密钥 | `runtime_key` / `config.toml` / `devices.json` / 审计文件全部 0600，目录 0700；`SigningKey` 的 `Debug` 输出被刻意打码；enrollment secret 只走 env 或 stdin，不进 argv；审计日志哈希 id、不记 body |
| 加密 | Ed25519 + SHA-256（ring，随 rustls 已在依赖树内）；随机数一律 `ring::rand::SystemRandom` / `getrandom`；无 MD5/SHA1/DES |
| 依赖 | 本条线新增的外部 crate 只有 `toml`（工作区既有）与 `tokio-tungstenite`（agent/relay 各自已有）。CI 跑 `cargo deny check` 与 `cargo audit` |
| 错误 | 相同失败给相同回应（错 secret 与不存在的 secret 一致；缺断言与错断言都 401）；错误体只有 code + 一句话，不含栈、不含 SQL、不含内部路径 |

---

## 三、已知问题清单（未修，按影响排序）

| # | 问题 | 影响 | 为什么现在不修 |
| --- | --- | --- | --- |
| 1 | **无 AEAD**：TLS 终止方可读会话明文 | 托管 relay 场景下运营方可读用户对话与工具输出 | Phase 2 项。当前对策是把 self-host 定为默认路径并在文档里说清楚，而不是含糊其辞 |
| 2 | **`leveler-mobile` 未开始** | Phase 1 无法宣称完成（K23） | 见 PR11a–f；本机无 Flutter/Dart 工具链，无法编译验证 |
| 3 | `ClientOrigin` 只进审计行，未进 runtime | `leveler-app` 无法按 origin 做策略或在 UI 标注「来自手机」 | `CommandEnvelope` 没有 origin 字段，贯通它是独立改动 |
| 4 | agent 重启后 ±120s RPC 重放窗口 | 攻击者需先截获一条合法 RPC 且在 120 秒内重放；`deliver` 有幂等兜底 | 需要持久化 seq/nonce，与「relay 不存状态」的取舍相反，留给 Phase 2 权衡 |
| 5 | 配对失败无 per-runtime 锁定 | 只有全局 10 次/分，攻击者可持续低速试（对 128-bit secret 无意义，但会打扰真实配对） | 需要 per-runtime 计数状态；优先级低于上面几条 |
| 6 | `/v1/hosts/{id}/rpc` 无限流 | 已授权设备可刷 agent 隧道 | 需要 per-token 计数；被 token 15 分钟有效期和撤销兜住 |
| 7 | ProjectRouter 与 `web-projects.json` 靠一份抄来的 golden 形状对齐 | web 改格式而未同步，手机端项目列表会静默变空 | 共享类型需要动 `leveler-web`（本轮不擅自重构）；已有一条吃 web 形状的测试作为漂移警报 |
| 8 | 无指标导出器 | 无法接 Prometheus | 仓库无此设施；审计行可聚合 |
| 9 | relay 单进程内存态 | 横向扩展需共享存储 | 文档化前提，非默认部署 |
| 10 | 无二维码渲染 | 配对靠粘贴载荷 | 设计把粘贴列为等价路径；渲染需新依赖 |

---

## 四、fuzz / 负例覆盖

- **签名与信封**：`testdata/signed_envelope.golden.json` 1 正 + 5 负（伪造签名、错 `recipient_id`、id 含 `|`、relay 改写 recipient、原样跨 host 转发），由 `every_golden_case_behaves_as_documented` 真实回放。
- **准入**：14 条 `admission.rs`，每条负例都断言 **runtime 收到 0 条命令**，而不只是返回了错误。
- **控制面**：22 条 `relay_api.rs`，覆盖认证、重放、越权、枚举一致性、id 合法性。
- **转发与代理**：7 条 `forwarding.rs` + 6 条 `rpc_proxy.rs`，含逐字节不变、不串台、撤销关流。
- **随机输入**：`frame_fuzz.rs` 对每一个 relay 能碰到的解析器做确定性变异——信封 JSON、两个方向的 tunnel 控制帧、`RpcRequestPayload`、内层 `UpstreamMessage`、runtime/device 断言头、base64 公钥——共 3.5 万次，断言不 panic，且**任何验签通过的帧其 payload 必须原封未动**。牙口已验：把签名校验短路，第 17 轮即报警。种子固定，失败可复现。
- **仍未做：** 结构化 fuzz（`cargo-fuzz` + libFuzzer 覆盖率反馈）。它要 nightly 工具链，本仓库不要求任何人装；上面那条是每次提交都真跑的弱替代品，不是等价物。

---

## 五、结论与门禁状态

**不得对外宣称「生产就绪的远程控制」。** 理由按顺序：

1. **Phase 1 的完成定义包含真机 APP 路径**（K23）。APP 现在在 iOS 模拟器上整链跑通（配对 → 多项目切换 → 中英文对话 → 审批 → 命令真执行 → 重启 resync），但**真机与 Android 仍未跑过**，因此 Phase 1 未完成。
2. **机密性只到 TLS**。在托管 relay 上，「你的对话运营方看不到」这句话现在**不成立**。self-host 部署下这句话成立，但那要求用户自己运维 relay。
3. 上表第 4、5、6 条是已知的可被利用的粗糙面，虽然影响有限。

**可以说的是：** 后端链路（host + relay + CLI）在完整性上是自证的——relay 无法伪造命令、无法替换密钥、无法把响应错配、无法在撤销后继续投递，每一条都有会红的测试。

**下一次门禁（Phase 1 出门）需要：** PR11f 真机清单勾完 + 本文件第三节第 1、2 条关闭或明确降级。（针对帧解析的随机输入测试已补，见第四节。）
