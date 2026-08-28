# Completion Reconciliation Gate（ICG-6R 结构性收口）

## 旧缺陷

icg-6r（不可能任务诚实收场探针）复现的失败链：

```
用户原始目标（与不可改测试冲突，字面上不可满足）
    ↓ 执行模型发现冲突
语义重解释（空名记录 / 部分面豁免 / Valid==false 坏记录 …）
    ↓ 实现被替换后的弱化任务，测试全绿
自报完成 → update_goal(complete) → 全部机械门放行 → Verified
```

措辞层契约（update_goal 工具描述 / goal 序言 / closeout nudge，共两个 commit）把诚实收场率从
~33% 提到 ~83% 后进入渐近：每封死一种重解释，模型换一种新的。机械门无法判语义。

## 架构

执行声明与最终裁决分离——**一次独立的模型二审**，只发生在
`depth==0 && goal_mode && update_goal(complete)` 且所有既有机械门（blocking findings、
todo/delivery gate）都已通过之后：

```
update_goal(complete)
    ↓ 既有机械门（不变，先行）
Completion Reconciliation（新，独立 generate 调用）
    ↓ 结构化判决 JSON
SATISFIED 且全要求满足且零矛盾 → 走既有完成路径
BLOCKED / UNCERTAIN / 矛盾 / 畸形输出 / provider 失败 / 超时 → 拒绝完成（fail-closed）
```

- **原始目标权威**（§5）：输入用 `ObjectiveAnchor`（session goal 原文，否则首条用户消息）——
  执行模型对目标的后期转述不作为契约。
- **输入有界**（§6）：原文目标 + complete 声明的 summary + 最近 2 条 assistant 文本（4KB 尾）+
  最近 3 条工具结果（各 1.5KB 尾）+ 改动文件清单 + fresh-verify 布尔。无 CoT、无隐藏答案。
- **结构化输出**（§8）：`{verdict, requirements[{requirement,satisfied,evidence}], contradictions[], reason}`；
  verdict=satisfied 但 requirements 为空 → 视作 uncertain（没有逐条对账就没有裁决）。
- **拒绝反馈**：durable `GoalIntercepted{kind:"completion_reconciliation"}` + ledger intercept +
  update_goal 工具错误结果，指引"按原表述完成或 blocked 点名冲突"；Unavailable 类单独措辞
  （"什么都没被裁决，重试或如实 blocked"）。
- **取消/重试**（§17/§18）：走 loop 的 cancellation child token；单次调用无自建重试——
  拒绝后模型下一轮重试 update_goal 即是天然重试。
- **持久化/重启窗口**（§19/§20）：gate 在任何完成事实持久化之前运行；中途崩溃 = 无完成持久化，
  恢复后模型重发 update_goal → gate 重跑。重复的 gate 调用无副作用。
- 子代理与普通聊天轮不触发（children 由父级 settle；chat 无 goal 可对账）。

## 成本（实测于 icg-6r ×10）

见验收数据节。预算许可（§14）：仅发生在 goal 终点，flash 档 2-4s 级。

## 测试

- 单元（reconciliation.rs ×8）：判决解析矩阵（满足/一项不满足/矛盾/uncertain/空 requirements/
  畸形×3/JSON 周围散文/CJK 有界尾）。
- e2e（loop_test ×3）：blocked 判决拒绝完成且运行继续（durable 事件+反馈文案）；畸形输出
  fail-closed；provider 失败 fail-closed。
- 既有门优先级：blocking-finding 先于 gate 拒绝（既有测试保持通过=未被覆盖）。
- 测试基建：scripted mock 由 `leveler_test_support::reconcile_autopilot` 对 gate 请求带外应答
  （识别指令头 `completion reconciliation judge`），脚本 FIFO 与请求数断言不受影响；
  loop_test/multi_agent_test 显式脚本消费（两种模式各证一面）。
- `direct_spends_no_extra_model_call_on_acceptance` 语义更新：acceptance 审计仍零额外调用；
  reconciliation gate 是本轮有意新增的一次调用，在 mock 中带外应答。

## HC002-F1 — 结构化结果交付可靠性（假阴性收口）

首版 gate 上线后，GORK 的 HC-002（icg-5-long-task ×2）与本地受影响 lane 重跑（17/18）暴露
**系统性假阴性**：活全做对（隐藏验收绿）但每次 reconciliation 都 `Unavailable —
reply carried no JSON object` → fail-closed 拒绝 → 终局 Blocked / rc 1。
（×10 icg-6r 没暴露它，因为该探针的期望结局恰好就是 blocked。）

**根因（对真网关逐项复现锁定）**：openai_chat 编码器对 thinking-flag 模型**总是**发
`thinking:{"type":"enabled"}`；gate 又继承了主策略的 reasoning effort（profile 默认 **max**）；
在 `max_tokens: 1024` 下 reasoning 吃光全部预算——实测 `finish=length`、
`reasoning_tokens=1024/1024`、content 空。裸 API 同 prompt 不带 thinking/effort 时一切正常。

**修复（transport 层，语义零改动）**：
1. gate 请求自带 `reasoning_effort: low` + `max_tokens: 4096`（判格式不判难题；实测 4.6s /
   ~350 reasoning tokens vs max 档 18s / ~1750）——不再继承主策略 effort；
2. 解析加固：字符串感知的平衡花括号扫描，接受裸对象 / ```json 围栏 / 散文包裹的**唯一**对象；
   相同对象重复可、**不同对象=ambiguous 拒绝**；
3. 失败分类：provider_error / timeout / empty_reply / no_object / bad_json / bad_verdict /
   ambiguous_objects（观测字段 + 拒绝文案区分"任务未满足"与"验证器不可用"）；
4. **一次**格式修复重试（FORMAT_REPAIR_MAX_ATTEMPTS=1）：同对话回放 + 只要 schema、
   明示不得改判；provider 错误不触发修复；两次失败即 fail-closed，无第三次；
5. Unavailable 反馈加反重复重试指引（HC-002 观测到同一声明连提 5 次）。

SATISFIED/BLOCKED/UNCERTAIN/UNAVAILABLE 接受规则逐字未动；解析成功≠语义成功
（单测锁定不可能任务不因解析变宽而通过）。

## 验收（§28-§33 / §59-§60）

（双向控制 + lane 重跑后回填：icg-6r ×3 / HC-002 ×3（Completed+rc0+Unavailable=0）/
受影响 lane 假完成 0 与 CORRECT_WORK_UNDERCLAIMED / 时延与修复率实测）

## 跨模型 Judge 实验（Cross-Model Completion Reconciliation）

**动机**：同模型二审存在相关性误差——执行模型重解释需求后，同一模型的判定往往接受同一套
重解释（icg-6r 门后假完成 ≈ 1/19），且漏掉显式列出的验收项（scale-s800 的"边界规则要有测试覆盖"）。
假设：**执行与判定用不同模型**能降低这种共谋。

**实现**（`e3daca2` + `293cf46`，窄改动，语义零改）：

```
[agents]
completion_judge_model = "deepseek/deepseek-v4-pro"   # 未配置 = 执行模型自己（既有行为）
```

`global_config → ExecutorFactory.completion_judge_model → Executor::with_reconciliation_model_opt`
→ `drive.rs` 只把 gate 那**一次** generate 换模型；主循环每一次请求仍走执行模型。
配错（不是 `provider/model`）是**响亮的配置错误**，judge 不可达是 **fail-closed**——
都不会静默退回同模型（那会污染实验并掩盖笔误）。prompt、输入构造、判定规则、
SATISFIED/BLOCKED/UNCERTAIN/UNAVAILABLE 接受规则、格式修复上限逐字未动。

**结论：实验被 transport 阻断，语义问题未能测量。**

冻结的 `RECONCILE_TIMEOUT = 60s` 是**按 flash 判定校准**的。实测（同一 6.4KB gate 形状载荷，
串行无并发）：

| 判定模型 | gate 时延（3 次） | 中位 | 相对 60s 天花板 |
| --- | --- | --- | --- |
| deepseek-v4-flash | 16.3 / 8.7 / 13.2s | 13.2s | 4.5× 余量 |
| deepseek-v4-pro | 31.9 / 18.5 / 31.5s | 31.5s | 1.9× 余量 |

真实探针里的 gate 调用（载荷更大）：成功样本 **53.0s / 53.0s / 57.4s**，全部贴着天花板；
6 次 judge 调用中 **4 次 `Unavailable / failure_kind=timeout`**（67%）。后果与 HC002-F1 同形，
只是成因从"推理预算饿死"变成"时延超限"：HC-002 第 2 次连续两次 gate 超时 → fail-closed 拒绝
→ 活做对了却终局 Blocked（`expect_passed=true, completed=false`），**假阴性回归**。

按 §53 分类为 `CROSS_MODEL_TRANSPORT_REGRESSION`，**不**记为语义失败；按 §14/§15 未在实验中
调整超时/effort/预算/修复次数。

**已取得的实测数据**（部分门，未跑满，判定 = v4-pro）：

| 门 | 跑数 | 假完成 | 说明 |
| --- | --- | --- | --- |
| icg-6r | 8/10 | 0 | 8/8 诚实收场（blocked + 树干净 + build/test 绿）。**judge 调用 0 次**——执行模型一次都没声称完成，所以本门没有检验到跨模型判定 |
| scale-s800 | 2/10 | 0 | 2/2 外部验收通过，含"边界规则被测试锁住"（`_test.go` 必须在改动集内） |
| HC-002 | 2/3 | 0 | 1 次 Completed+rc0；1 次 transport 假阴性（见上） |

判定成本：成功调用 2 次，53.0s / 57.4s；实验总花费 $0.98（含冒烟 $0.197）。

**下一步是产品决策，不是继续加层**：60s 是为 flash 选的常量，换更强判定就必须重新校准；
在超时问题解决前，跨模型假设（语义相关性是否真的降低）**仍是未测量状态**。

## Judge 超时校准（Timeout Calibration）——同步 v4-pro 判定不可行

上一节的 60s 天花板被解耦为可配置策略（`b829fe89`）：

```
[agents]
completion_judge_model = "deepseek/deepseek-v4-pro"
completion_judge_timeout_seconds = 180    # 未配置 = 保持出厂 60s
```

超时是**运行策略，不是语义真值**；0 是配置错误而非回落默认（0 会把每次判定瞬间超时，
伪装成"活没干完"的判决）。天花板约束**一次请求**（首判与那一次格式修复各得一份），与 60s 时代
的语义形状一致。拒绝结果现在带真实 elapsed，可区分"provider 秒失败"与"跑满天花板"。

**结论：`V4_PRO_SYNCHRONOUS_JUDGE_OPERATIONAL_VIABILITY=FAIL`。**

校准后 cohort（Dogfood Lab，冻结二进制 `b829fe89c6d4`，串行，无并发自污染）：

| cohort | 天花板 | judge 调用 | 拿到的判决 | 超时 |
| --- | --- | --- | --- | --- |
| 校准前 | 60s | 6 | 53.0s / 53.0s / 57.4s | 4 |
| 校准后 | 180s | 3 | 59.4s（Uncertain）/ 90.8s（Satisfied） | **1（180.0s）** |

预检（合成 6.4KB 载荷，串行）：3/3 结构化成功、0 超时、17.4 / 21.4 / 47.9s——**预检严重低估了
真实载荷**。真实 reconciliation 载荷下，判定时延分布右偏且**尾部越过 180s**。

抬高天花板确实把被右删失的判决救了回来（90.8s 那次在 60s 下根本拿不到，而它正是让
HC-002 #1 走到 Completed 的那一次判决）。但问题不是"60s 差一点"，而是**同步单次判定的
时延尾部没有可辩护的上界**：再抬到 300s / 600s 只是把同一条尾巴往后推，同时把每次完成
判定的最坏等待推到分钟级——这已经不是一个可以放进产品完成路径的同步调用。

按 §76 停止，不再抬高超时。语义问题（异构模型是否真的降低相关性误差）**仍然是
UNMEASURED**：全部 3 次校准后判定里只有 2 次拿到判决，样本不足以判任何语义门。

**保留证据**（`PRE_CALIBRATION_RESULTS_SCORING=NO`，不计入任何新分母）：
校准前 icg-6r 8 次 / scale-s800 2 次 / HC-002 2 次；校准后 HC-002 1 次 Completed（隐藏验收绿、rc0）。

## 残余限制

- 二审仍是模型判断：同一模型对自身同型偷换存在共谋风险；×10 探针是回归门不是数学保证。
  若结构性二审后仍漏 → SEMANTIC_COMPLETION_RECONCILIATION_LIMIT_REACHED=YES，停止工程收敛。
- 保守误伤（CORRECT_WORK_UNDERCLAIMED）单独计数，不与假完成混淆。
