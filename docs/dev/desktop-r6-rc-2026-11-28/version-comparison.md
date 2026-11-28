# 上一正式版 vs 当前 RC

日期：2026-11-28

## 1. 身份

| 项 | 上一正式版 | 当前 RC 候选 |
| --- | --- | --- |
| 版本/tag | `v1.0.29` | 未发布；Cargo 仍 `1.0.29`，git `de1c8c36` + 本轮改动 |
| 源码 | `f972f9f6` | `de1c8c36` + dirty |
| 产物 | CLI 归档（macOS/Linux/Windows） | CLI RC + macOS Desktop `.app/.zip/.dmg`（未签名） |
| Desktop | **无**（仅源码） | 新增安装包 |
| 平台 | CLI：三平台；Desktop：源码 macOS 验证 | Desktop：macOS 构建/验收中；Windows **BLOCKED** |

## 2. 能力对比

| 维度 | v1.0.29 | 当前 RC | 依据 |
| --- | --- | --- | --- |
| Agent 能力 | 既有 Runtime/Goal/Steer/工具 | 未改变控制语义；新增预算停止文案与全局 limits 回退 | `event_bridge`、`global_config` |
| Desktop 功能 | 无正式产物 | R1–R6-0：Provider/Model Picker、Chat/Plan/Goal、MCP、Skills、手动 Review、Memory、图片/文件/Diff、设置/命令 | R1–R6 报告 + 本轮打包 |
| MCP | 有（CLI/TUI） | Desktop 管理与真实调用（R3a） | R3a |
| Skills | 有 | Desktop 信任/调用（R3b） | R3b |
| Review | 有 | Desktop 手动触发（R3c） | R3c |
| Memory | 有 | Desktop 四命令（R6-0） | R6-0 |
| Provider | 有 | Desktop 配置/凭据接管（R2a） | R2a |
| 多模态 | 有 | Desktop 粘贴/拖放/工具图片/历史图片（R5） | R5 |
| 长任务 | Goal/Steer | Chat+Steer 真实复跑 59s PASS；媒体/预算治理加强 | 本轮 A1/A2/A3 |
| macOS | CLI 归档 | 新增 Desktop 安装包（未签名，本地） | 本轮 C |
| Windows | CLI 归档 | **仍只有 CLI；Desktop 无产物/无验收** | 本轮 B |
| 安装升级 | CLI 更新器 | Desktop 无更新器；正式升级 **BLOCKED** | 本轮 C4/C5 |
| 性能 | 未测 | 未做与 v1.0.29 的可比基准 | 本轮性能缺口 |
| 测试 | v1.0.29 gate | 新增 22 media tests、5 media_gc tests、budget/config tests、22 memory integration | 本轮 |
| 已知风险 | Windows 未真机、无公证 | 同上 + Desktop 无签名/更新 | 本轮 |

## 3. 结论

当前 RC 的主要增量是 **Desktop 产品面从“源码运行”变成可安装产物**，以及媒体存储生命周期、
预算终态文案、凭据 Debug/崩溃脱敏。Agent 核心控制语义没有重写。

但：

- Windows Desktop 没有产物或真机证据；
- macOS 没有正式签名/公证；
- Desktop 自动更新不存在；
- 因此不能把当前 RC 描述为“双平台正式版”，也不能宣称已具备正式分发资格。
