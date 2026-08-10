# C5 Context Intelligence

日期：2026-08-10。基线 `d3e655d`（main，C1–C4 CLOSED + ICG CONDITIONAL PASS 之后）。
**SPECIFICATION ONLY —— 本阶段零生产代码改动。**

核心立场：**CodeLeveler 不减少模型能力；它知道模型能力，并按任务动态使用。**
1M context 不是要"管住"的风险，是要"调度"的资源。

---

## Motivation

C1–C4 证明了闭环可靠；ICG 证明了组合可用。下一层竞争力不在"更可靠"，在
**信息摄取的调度**：同一个 agent，rename 用 30k、跨模块重构用 800k，
且两种用法都是策略产物而非巧合。

三个外部趋势直接压在这一层：DeepSeek 1M context（本仓已声明未利用满）、
多模型（不同 window/价格/衰减曲线）、多 Agent（context 是要分配的共享预算）。

## Current Limitation（基于 `d3e655d` 代码审计，非历史记忆）

**已经存在且正确的：**

| 层 | 现状 |
| --- | --- |
| 能力描述 | `ModelCapabilities`（bool 面）+ `ModelLimits`（context_window 1048576 / reliable_context 786432 / max_output_tokens 393216）—— **只描述事实，不设限**，正是 §3 要的形态 |
| 策略缝 | `ExecutionPolicyResolver`（policy_resolver.rs）已把"能力"与"运行时策略"分开，含 eval override 缝 |
| 压缩 | `compaction.rs`：锚定折叠 + **handoff briefing**（保留 decisions/failed attempts/真实路径/约束）+ 二次折叠时**增量合并旧 briefing** 而非重推导 + keep-recent 12 |
| 仓库智能 | symbols / find_symbol / find_references / **LSP 长会话** 已在 tools 层 |
| 实测地基 | C2.1：读取结果占终请求 57.4–72.2%，是唯一随任务长度递增的成本；C2.2 证伪"旧读取可无损回收"（轨迹是 broad-then-narrow，旧读会被回引）；C2.3B 证伪 prompt 级纪律 |

**真正的缺口（C5 的对象）：**

1. **预算静态**：`context_budget = profile.limits.reliable_context` —— 每个任务拿同一个数。
   rename 与大重构在策略上不可区分；"用 30k 还是 800k"目前是巧合（工具返回多少累积多少），
   不是决策。
2. **无扩张概念**：预算只能被"撞上→压缩"，没有信号驱动的上调路径。
3. **保留粒度错位**：briefing 保留的是"对内容的转述"；而**工作区文件是唯一无损外部存储**，
   文件内容被总结掉之后只能靠模型转述回忆，无法指针化重读。
4. **双阈值双口径**：chat 预请求 `PRE_REQUEST_COMPACT_THRESHOLD = 24_000` vs 执行环
   `context_budget`（768k）—— C2.1 已记录两条路径口径不一致。
5. **测量偏差**：`estimate_tokens` 对源码低估 16–22%（C2.1 §3）—— 任何以它为单位的阈值
   都偏晚触发；预算越大，绝对误差越大（1M 窗口下可差 15 万 token 量级）。
6. **缓存经济学缺席**：DeepSeek prefix cache 命中实测 96.8%（历史测量）。压缩**重写前缀**
   = 一次性摧毁缓存链；在 input 1 CNY/MTok 的定价下，一次 500k 前缀重写的真实成本
   远超它省下的 token。当前压缩决策完全不知道这件事。

## Model Capability Layer

**决定：保留现有 `ModelCapabilities` + `ModelLimits`，不重建。** 它已经是"只描述事实"的
正确形态。仅两处澄清性补充（S1，非行为变更）：

```
ModelLimits {
    context_window,        // 硬事实：provider 会拒绝的界
    reliable_context,      // 质量事实：超过后长上下文召回可测退化的界（见下）
    max_output_tokens,
    ...
}
+ context_quality: Option<ContextQuality> {   // 可选、逐模型、有测量来源
      degradation_onset,   // 实测退化起点（缺省 = reliable_context）
      measured_at,         // 测量日期与方法引用 —— 没有测量就不许填
  }
```

语义澄清（写进字段文档）：`reliable_context` 是**质量声明不是政策**。策略层可以在
知情的前提下越过它（例如低风险的检索型任务用到 900k），但必须显式记账"越界运行"。
**禁止**任何路径把它当 hard cap 拒绝请求。

## Runtime Context Policy

新增 `ContextPolicy`，落在**现有 `ResolvedExecutionPolicy` 内部**（不建第二个 resolver）：

```
ContextPolicy {
    initial_budget,        // 起步压缩阈值（不是加载量——本产品从不预加载）
    max_budget,            // 本任务允许扩到的上限（≤ context_window）
    expansion,             // 扩张策略：见 Adaptive Expansion
    compaction,            // 触发与方式：见 Intelligent Compaction
    retention,             // 保留分类：见 Intelligent Compaction
}
```

关键设计选择：**预算控制的是"何时折叠/折叠什么"，不是"允许读多少"。**
C2 已证明导航准确性优先于 token 节省（C2.3B 教训）；限制读取 = 降低能力，禁止。
预算唯一作用是管理**已持有**上下文的生命周期与成本。

默认解析（v1，全部可被 eval seam 覆盖）：

| 任务信号 | initial_budget | max_budget |
| --- | --- | --- |
| 缺省 | min(reliable_context, 256k) | reliable_context |
| 显式长任务（goal 多义务 / plan 多步） | reliable_context | context_window，越界记账 |
| 子 agent（Explorer/Worker） | 128k | 256k（多 Agent 预算见下） |

## Context Budget Engine

**决定：v1 用廉价信号 + 保守缺省，不做前置的"任务复杂度预测器"。**
理由是已有证伪：C2.3B 表明加一层"聪明的前置判断"（当时是 prompt 纪律）代价真实、
收益为负。预算引擎 v1 的输入只用**已经免费存在**的信号：

| 输入 | 来源（现有） |
| --- | --- |
| 任务文本形状（义务数、明确的多文件措辞） | goal / plan |
| 仓库规模 | workspace 扫描（list_files 已有缓存路径） |
| 进行中信号 | blast_radius、repeated_read_guard 触发、verification 失败次数 |

输出 `recommended_context_budget` 只在两个时点更新：任务开始（initial）与扩张触发点。
**不逐轮重算** —— 逐轮重算 = 前缀不稳定 = 缓存损失。

## Adaptive Expansion

扩张 = **上调压缩阈值**（允许更多历史留在窗口内），不是预加载。

**扩张信号（任一触发，一次上调一档：256k → 512k → reliable → window）：**

1. **重读压力**：repeated_read_guard 或压缩后 30 轮内对已折叠文件的再读取 ≥3 次 ——
   直接证据表明折叠丢了还需要的东西（C2.2 的 broad-then-narrow 实测正是此形态）；
2. **影响面增长**：blast_radius 报告的受影响文件数跨过档位（>20 文件）；
3. **修复升级**：verification 失败进入 repair，而失败证据引用已折叠区域。

**不扩张的信号（同样重要）：**

- 任务已进入收尾（closeout 阶段）——此时扩张纯浪费；
- 触发源是 distractor 探索（读了就放弃的路径不构成扩张理由）；
- 已在 max_budget —— 此时只能压缩，且必须按保留分类压。

**何时大上下文有害（问题 3 的答案，写死为设计约束）：**
(a) 检索性干扰 —— 无关历史稀释注意力，长上下文召回在 reliable_context 外可测退化；
(b) 成本 —— 每轮请求重复计费整个前缀，1M 前缀 × 50 轮 = 50M input token；
(c) 缓存 —— 见下。因此扩张是**响应证据**的，永远不是"反正窗口大就多留"。

## Intelligent Compaction

现有 briefing 机制保留（它已经做对了"保留决策与失败尝试"）。三个升级：

**1. 保留分类显式化（retention classes）：**

| 类 | 处置 |
| --- | --- |
| user goal / constraints / 显式偏好 | 永不折叠（结构化持有，不进 briefing 赌运气） |
| decisions + 失败尝试 + 验证结果 | briefing 保留（现状） |
| **文件内容（曾读取）** | **指针化**：`path + 行区间 + file_state 指纹` 替代内容。文件在磁盘上，是唯一无损存储；指纹接到现有 stale 跟踪 —— 指针失效（文件已变）时强制重读而非引用记忆 |
| verbose 工具输出 / 重复探索 | 折叠为一行结果引述 |

指针化直接打击 C2.1 测出的头号成本（读取结果 57–72%），且没有 C2.2 证伪的
"删掉就没了"问题 —— 指针可再读。

**2. 缓存感知的触发（cache-aware compaction）：**
压缩重写前缀，摧毁 prefix cache（实测命中 96.8%）。因此：**晚触发、大步长、低频率**
—— 一次折叠到 initial_budget 的 50%，而不是贴着阈值反复小折。扩张优先于压缩：
预算未到 max 时先扩张（保前缀），到 max 才压缩。

**3. 双路径合一 + 测量修正：**
chat 的 24k 预请求阈值改为从同一 `ContextPolicy` 解析（消除双口径）；
`estimate_tokens` 校准（或改用 provider 返回的真实 usage 反馈修正系数）——
这是所有阈值的地基，**S2 的第一件事**，因为 C2.1 已证明它偏 16–22%。

## Repository Intelligence

**决定：v1 不新增任何索引。** symbol/references/LSP 已在；C2 实测 Target Recall 8/8、
Impact 18/19 FULL —— 导航没有可测缺口，为炫技加 dependency graph / semantic index
违反本仓一贯纪律（先有失败测量，后有机制）。

唯一预留：若 C5 eval 出现"大仓跨模块任务因缺全局视图失败"的**可复现**证据，
再评估 call graph——且必须先过与 C2-BV 同级的 benchmark validity。

## Multi Agent Context Strategy

- 每个子 agent 从同一 `ExecutionPolicyResolver` 解析**自己的** `ContextPolicy`
  （Explorer 小预算大并发，Worker 串行中预算）——已有 Role 缝直接承载。
- 父子共享**结论不共享转录**：子 agent 回传结构化摘要（现有 sub_agent 返回值语义），
  父 agent 的窗口不因 spawn 而线性膨胀。
- 多 Agent 总预算 = 加法记账（成本），不是窗口共享（各自独立窗口）——
  记账进现有 budget.rs 的 task 级预算。

## Evaluation Plan（proposal，不建新框架）

复用现有 `leveler eval ablate`（单旋钮消融驱动器 + `policy_override` 注入缝——已落地）。

**C5-E suite（提案，实施期造，全部过六项闸门）：**

| case | 形状 | 验证 |
| --- | --- | --- |
| C5-E1 wide-impact | 跨 30+ 文件的一致性改动（scale fixtures 已有生成器） | 小预算失败/多轮 vs 大预算成功 —— 大上下文正收益的存在性 |
| C5-E2 history-dependent | 后半程义务依赖前半程发现（长任务，folding 敏感） | 指针化保留 vs 现状 briefing 的成功率差 |
| C5-E3 distractor-heavy | 大量无关历史 + 小任务 | **大上下文有害面**：扩张策略不该触发；触发即失败 |
| C5-E4 cache-economics | 同一任务，连续小折 vs 晚大折 | 成本（真实 usage 计费口径）与成功率双指标 |

指标：task success / rounds / tool calls / **真实 input tokens（provider usage，
不用 estimate）** / 每任务成本 / C1–C4 suite 回归。
判定纪律沿用全链惯例：3 reps 预固定、冻结、hash 锁定、identity 全映射；
**每个机制先有 ablation 证明收益，再进产品默认**。

## Migration Plan

| 阶段 | 内容 | 性质 |
| --- | --- | --- |
| S1 | `ContextPolicy` 结构落入 `ResolvedExecutionPolicy`；`context_quality` 字段；双阈值合一（行为等价迁移） | 机械，行为不变 |
| S2 | `estimate_tokens` 校准（usage 反馈系数）；越界记账事件 | 测量修正，先于一切阈值调整 |
| S3 | 扩张信号 + 缓存感知触发（晚/大/低频） | 第一个行为变更，ablate 门禁 |
| S4 | 读取结果指针化保留 | 最大收益项，ablate 门禁 + C2 suite 回归 |
| S5 | C5-E suite + 全量验收 | 判定 C5 CLOSED 与否 |

每阶段独立提交、独立可回滚；S3 起每步跑 C1–C4 blocking regression。

## Risks

| 风险 | 缓解 |
| --- | --- |
| estimate_tokens 偏差放大（1M 下差 15 万级） | S2 前置；所有新阈值以校准后单位定义 |
| 压缩/扩张策略破坏 prefix cache → 成本反升 | 缓存感知触发是硬设计约束；C5-E4 双指标验收 |
| 指针化后指针悬空（文件已变） | 绑 file_state 指纹；失效即强制重读（已有 stale 语义复用） |
| "聪明预算"重蹈 C2.3B（加层负收益） | v1 只用免费信号；每个机制 ablation 先行 |
| 大预算掩盖导航退化（读得多≠找得对） | C2 suite 作为常设回归；Target Recall 不许跌 |
| 多模型下 reliable_context 无实测依据 | context_quality 必须带 measured_at，未测就空 |

## Open Questions

1. DeepSeek 1M 的实际退化曲线未测 —— reliable_context 786432 是配置声明不是测量；
   需要一次 needle/召回式的实测来填 `context_quality`（S2 附带或独立小实验）。
2. 越界运行（>reliable）的质量记账用什么观测：verification 失败率分桶够不够？
3. 多模型中途切换（大窗口模型探索 → 小窗口模型执行）是否值得：v1 明确不做，
   但 ContextPolicy 的形状应不排除它。
4. briefing 与指针化的边界：decisions 里引用的代码片段算内容还是决策？（倾向：≤10 行
   引述随 briefing 保留，超过则指针化。）

---

## 关键问题直答（§8）

1. **1M 怎么用**：不预加载、不设限读取；预算管折叠不管摄取；显式长任务 max_budget
   直达 window 并记账越界；扩张响应证据（重读压力/影响面/修复升级）。
2. **何时加载更多**：从不"加载"，只"保留更多"——三个扩张信号，任一命中上调一档。
3. **何时有害**：干扰稀释（distractor）、逐轮重复计费、前缀重写毁缓存 —— 三者都有
   对应 eval（C5-E3/E4）与设计约束。
4. **能力与预算分离**：`ModelLimits` 是事实层（已存在且正确），`ContextPolicy` 是
   策略层（落在已有 resolver）——同一个缝，禁止第二个 resolver。
5. **避免省 token 降能力**：预算永不限制读取；C2 suite（Target Recall/Impact）是
   不许跌的常设回归；C2.3B 的证伪作为反面清单写进设计。
6. **评估收益**：现有 ablate 驱动器单旋钮消融 + 真实 usage 计费口径 + 成功率/成本
   双指标 + C5-E1..E4 四形状（含有害面）。
