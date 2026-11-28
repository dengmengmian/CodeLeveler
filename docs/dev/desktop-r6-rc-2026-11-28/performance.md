# 性能（R6-E）

日期：2026-11-28
结论：**只记录原始测量，不做无对照的“提升/回退”结论。**

## 1. 方法

| 指标 | 方法 |
| --- | --- |
| Desktop 启动到 UI | Playwright 启动安装版，到 `#brand-version` 出现并连接；单次样本 |
| Runtime 启动 | 同一安装版启动后，`runtime_info` 返回 pid；未单独计时 |
| 真实任务墙钟 | A1 chat+steer 任务从提交到 terminal；单次 |
| CLI 启动 | `target/release/leveler --version`，重复 5 次取中位数（见下） |
| 内存 | 安装版全部进程 RSS（main/renderer/GPU/utility/bridge）；未做长会话/多 Session 压力 |

环境：macOS 26.6.2 / Apple Silicon arm64；真实 HOME/provider；未关闭其他本机负载。

## 2. 结果

| 指标 | 值 | 说明 |
| --- | --- | --- |
| Desktop 启动（安装版） | **1.24–1.47 s** | 两次安装版运行；到品牌版本出现 |
| A1 chat+steer 完成 | **59 s** | 真实模型；11 次 model request、22 个工具事件 |
| A1 Steer 投递 | 1 次，`consumed` | 不增加额外回合 |
| CLI `--version` | **38.5 ms** | 5 次中位数（样本 37.7–101.5 ms） |
| 安装版全部进程 RSS | **~425 MB** | 主 167 MB + renderer 117 MB + GPU 84 MB + utility 43 MB + bridge 21 MB；启动后空载单次 |
| 长会话内存 | **NOT VERIFIED** | 未做 1h+ 会话/大量图片压力 |
| 与 v1.0.29 对比 | **不可比** | v1.0.29 无 Desktop 产物；CLI 与 Desktop 不是同一 workload |

## 3. 结论

本轮没有建立可比较的上一版 Desktop 性能基线，不能声称“启动更快/更慢”或“内存优化”。
A1 的 59s 是任务正确性与进度证据，不是性能基准。正式发布前如需性能门禁，应在同一机器、
同一模型、同一任务、相同样本数下建立 vNext 对照。
