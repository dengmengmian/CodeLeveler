# TUI Stability 门禁（spec §4 用户体验指标）

> 结论先行：TUI 稳定性由**已存在的两套测试**担当门禁，它们走 TUI 的真实客户端路径（不绕过 UX），
> 检测卡死/空转/输出丢失。本目录不重复造场景，而是把这两套测试**登记为质量系统的一等门禁**。

## 门禁测试

| 测试 | 覆盖 | 断言 |
|------|------|------|
| `crates/leveler-app/tests/tui_path_soak.rs` | short / long / goal 三种交互，累计模型轮数 ≥80，scripted mock provider | **空转/挂起当成功 = 硬失败**（墙钟超时无终态事件）；每种模式必达终态 |
| `crates/leveler-tui/tests/tui_session_e2e.rs` | TestBackend 无头驱动：打 slash 命令、喂 RuntimeEvent、断言屏幕内容 | 输入→输出→工具展示→多轮，渲染正确、不丢历史 |

## 运行

```sh
# TUI 稳定性门禁（release 层必跑；quick 层可选）
cargo test -p leveler-app  --test tui_path_soak
cargo test -p leveler-tui  --test tui_session_e2e
```

两者当前均为**绿**。crash / freeze / 输出丢失会让对应断言失败。

## 与质量系统的关系

- 现在：作为 `cargo test` 门禁纳入 CI（见 `AGENT_EVAL_SYSTEM_DESIGN.md` §7 CI 集成）。
- 待接（round 2）：把 soak 的"是否达终态 + 轮数 + 有无 hang"折算成 `QualityScore.tui_stability`
  的 0..1 分量（占 10% 权重）。当前该分量为 `None`，按诚实性原则不计入分母，**不以假值充数**。

## 为什么不在这里造 YAML 场景

`leveler eval` 的 case 执行器驱动的是 orchestrated/direct agent 循环，不是 TUI 客户端路径；
把 TUI 塞进 `EvaluationCase` 需要改执行器（round 1 只扩 Eval、不改 Agent/executor）。
现有两套测试已经在正确的层级驱动 TUI，重复造反而更差。
