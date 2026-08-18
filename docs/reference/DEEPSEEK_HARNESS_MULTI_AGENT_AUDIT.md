# DeepSeek Harness 多智能体机制审计(dsh@0.1.0-rc.7, SHA 99f6f02)

冻结参照:deepseek-ai/deepseek-harness @ 99f6f02fecdb7dff40c3fbc9470f5907c29f74ca(detached,本地 ~/Develop/codeleveler-dogfood/reference/deepseek-harness)。语义参照,非代码模板。

结论:dsh 的多代理层是「低摩擦、后台优先、模型自协调」路线——委派调用只需 2 个必填参数,continuable 子代理默认后台并由运行时(而非模型记忆)解析默认值,settlement 通知无条件送达父代理;但它明确不做工作区写隔离、权限格与重叠写保护,把兄弟子代理的工作区协调整体交给模型。无任何"必须委派"措辞:委派是"卸载独立工作以省上下文"的可选项,唯一命令语气是调度层的 background-first。

## A. Main 角色
`code` preset persona 仅一句("You are a coding agent…"),无 coordinator 定位;全部委派指导由工具插件 prompt section 提供(order 116.5,tool-subagent/src/index.ts:466):
> "Use subagent in the background by default. Start independent delegations together in one assistant message and continue useful work while they run. Set `run_in_background: false` only when your next action depends on that subagent's result. When a background run settles, the runtime sends you a notice containing its outcome and any final assistant message."

## B. 委派政策
- 独立工作(tool description :228):"Delegate a self-contained task … to offload focused, independent work — research, a scoped implementation, an analysis — so it does not consume this conversation's context."
- 并行+父继续:见上 prompt section。
- 前台判据::324 "Set `run_in_background: false` only when your next action depends on that subagent's result."
- 避免重复工作:不存在;"Coordinating sibling workspace effects is the model's responsibility"(2026-08-09 note)。
- 定性:是否委派=中性;调度=命令式 background-first。

## C. 调用摩擦
必填仅 `description`(3-5 词展示用)+ `prompt`;可选 `run_in_background`;provider/model/persona/toolFilter/maxDepth 全部部署侧配置,模型不可见("the model receives no provider selector")。

## D. Background-first 落点(四点一致)
1. preset `backgroundMode: continuable`;2. 工具描述 "runs in the background by default, immediately returns a durable subagent id";3. 参数描述 "Defaults to true. Set false to wait…";4. **运行时默认解析**(index.ts:261):`runInBackground: request.run_in_background ?? options.continuable` — 设计笔记:"The model must be able to rely on the advertised default rather than reproduce it perfectly on every tool call."(否决 prompt-only 方案)

## E. 生命周期
fresh child(新 flat scope,不继承父注册)/ fork(父日志 balanced completed-turn prefix 作 seed)/ 前台 one-shot(await+dispose,非 completed stopReason 映射 isError 且附 partial output)/ continuable(durable Session+至多一个 Activation,inbox 是唯一队列,cold resume 走 durable descriptor)/ settlement(见 F)/ followup send_message(FIFO 下一轮)/ interrupt(只停当前轮,inbox 保留,后代继续)。

## F. 父子通信
- **Settlement**:manager 构造的 user-role notice(kind 'subagent-settled',与子自报严格分 kind),**无条件**送达("the cases that most need it — a token ceiling, a model failure, cancellation — are exactly the ones where the child never got to choose")。首行:"Background subagent <id> finished / was stopped before it finished / ran out of room before it finished" + 子最后 assistant 消息或 "It left no closing message."。投递:父 idle→唤醒一轮;父忙→并入下一 step 批;teardown→inject 不唤醒。
- **子报告**:child-scoped `report` 工具 + 义务 prompt:"finishing your work is not itself a result… Report earlier as well whenever a partial finding changes what that agent should do next; reporting never ends your turn."(单自由文本 output,无结构化契约)
- send_message:"returns no answer — only confirmation… A failure means the message was NOT delivered."
- list_agents:"Use it to recall which ones you started, not to poll for completion — you are told when one finishes."(status running/idle/ready)

## G. 并行
`isConcurrencySafe: true` 全形态(子在自己 session,run 不改父 session);前台受 maxParallelToolCalls=10;**后台运行子数无 harness 上限**;"Sibling children can race on shared workspace…; the model owns that coordination."

## H. 安全(明确非目标)
有:子沙箱模式钉死、子审批钉 'never'+子 prompt("permission scope was fixed when you were started… do not retry the denied operation")、toolFilter(preset 未启,自认"可见性非权限")、depth 默认 3(触顶时工具仍可见,运行时拒绝)。
无(architecture note 明示 non-goals):per-child 写作用域、parent→child authority lattice、重叠写保护、重复工作检测、结构化子结果强制、跨子 token 总预算。

## I. Workflow / Ralph
workflow=模型手写 JS 编排脚本引擎,Ralph=fresh-agent 迭代循环("Completion and blockers are worker reports, not independent evaluation" — 无独立验收);两者都被 dsh 自己 prompt 门禁为"仅显式请求"。对本产品 Beta:workflow=later,Ralph=unrelated(被 goal+reviewer 路线取代)。

## 语义处置表(针对本地:独占写作用域+overlap 拒绝+depth1+结构化结果+durable log+harness reviewer)

| # | 语义 | 处置 |
| --- | --- | --- |
| 1 | 省略参数由运行时解析为 background | **ADOPT** |
| 2 | 同一默认四点一致陈述(配置/工具描述/参数/prompt) | **ADOPT** |
| 3 | "Start independent delegations together … continue useful work while they run" | **ADOPT** |
| 4 | 前台仅限"下一步依赖其结果" | **ADOPT** |
| 5 | 2 必填参数低摩擦 | ADAPT(本地需 scope 声明,必填≤3) |
| 6 | manager-owned 无条件 settlement notice(idle→wake/busy→并批) | ADAPT |
| 7 | 子端报告义务+中途早报 | ADAPT(本地 report_finding 已更强,补早报措辞) |
| 8 | continuable + send_message | DEFER |
| 9 | 反轮询措辞("you are told when one finishes") | **ADOPT** |
| 10 | interrupt 只停当前轮 | DEFER |
| 11 | settlement/错误必带 partial output | **ADOPT**(本地 ChildResult 已有,保持) |
| 12 | 兄弟工作区冲突交模型 | **ALREADY_STRONGER_LOCAL**(overlap 拒绝保留并扩展到活跃子) |
| 13 | toolFilter=可见性非权限 | ALREADY_STRONGER_LOCAL |
| 14 | 子审批钉 'never' + 诚实上抛 | ADOPT(与本地拒绝语义契合) |
| 15 | depth 触顶时工具可见、运行时拒 | ALREADY_STRONGER_LOCAL(本地 depth=1) |
| 16 | 后台子数无上限 | ADAPT(本地保留显式并发/总量上限) |
| 17 | context fork | DEFER |
| 18 | workflow 脚本引擎 | REJECT |
| 19 | Ralph 循环 | REJECT |
| 20 | one-shot 后台 Job 双轨 | REJECT |
