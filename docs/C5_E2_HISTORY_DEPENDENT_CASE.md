# C5-E2 History-Dependent Case

日期：2026-08-10。基线 `ee5ccb2`。**结论前置：case 两轮 shape 均 VALID 且功能可解，
但 6 个候选 probe 预算内机制零触发 —— `C5-E2 SHAPE INCONCLUSIVE`，未冻结。
副产物是一个对 C5 全局重要的实测结论：见 §The Real Finding。**

## Case（`evals/context/c5-e2-history-dependent.yaml`）

fixture `telemetryd-e2`（`gen_e2_fixture.py`，seed 20260810）：1500 文件（scale 家族 filler，
其自带 window 缺陷已治愈）+ **96 个真实 adapter**（~12KB/个，1.1MB relevant），四维种子
dialect（fallback/ordering/aliases/scale）只住在代码里。任务：每 adapter 如实 `Snapshot()`
+ `inventory` 子命令。验收：**行为交叉核对** —— 动态生成 96-adapter zz 测试，驱动 `Parse`
判声明真伪（说谎败在行为）。reference 由生成器从已知 quirk 发出。

## Validity

两轮 shape（56→96、事实移尾部）各自过完整六项闸门 = **VALID**；期间闸门抓住一次
"reference patch 丢新文件"（intent-to-add 教训第二次出现）。

## Shape Probes（预算 6 候选，全部用尽）

| 轮 | 形状 | 功能 | 机制 | agent 实际行为 |
| --- | --- | --- | --- | --- |
| 1（3 探针） | 56 adapter，事实在文件顶部 | 双臂 6/6，~44 轮 | compact=expand=**0**，峰值 ~300KB 载荷 ≈12 万 est | 范围读 parse 段即得全部事实 |
| 2（3 探针） | 96 adapter / 1.1MB，事实藏尾部 9KB 机械之后 | 双臂 6/6，控制 58 轮 vs 候选 40 轮 | compact=expand=**0**，峰值 ~414KB 载荷 | grep 表名（`metricAliases`/`missingValue`）→ 定点小范围读尾部 |

依规未做第三轮（不降档、不加跑、不禁工具）。

## The Real Finding（比 case 本身重要）

**一个具备 grep/范围读的合格 agent，在 1.1MB 分散事实的 96 模块任务上只需 ~12 万 token
转录。** 两轮 reshape 都被外科式检索击穿：只要事实有可 grep 的标识符（表名、函数名），
定点检索就把上下文需求压到折叠阈值的一半以下。结合 S3 的发现（全部既有 benchmark
转录仅数万 token），这指向一个未被证伪的假设：

> **256k 级折叠压力在"事实可检索"的任务类里可能根本不存在。** S3/S4 机制的自然受众
> 是"事实抗拒外科检索"的任务 —— 跨文件推理链、非命名语义、需要同时持有大量上下文
> 做全局一致性判断的形状 —— 而不是"文件多、字节多"。

同时两轮 12/12 functional PASS 也再次确认：现有产品在这种规模上**不需要**大上下文机制
也工作得很好 —— 这本身是 C5 值得记录的正面结果。

## 下一步 shape 方向（记录，未实施 —— 需新预算）

事实必须**不可命名检索**：例如 dialect 由多处代码流交互涌现（无集中表、无统一命名）、
或义务要求跨 adapter 的全局一致性比对（必须同时持有多个实现）。这类形状的 authoring
成本显著更高，且需先验证"人类专家也需要大上下文"才不算 harness 造景。

## Freeze Decision

```
status: INCONCLUSIVE — mechanism not exercised within probe budget
S3 Ablation Ready: NO
S4 Evaluation Carrier Ready: NO
```

case 保留为 VALID 的常规长任务（它本身是合格 benchmark，只是不构成 context 压力载具）。
