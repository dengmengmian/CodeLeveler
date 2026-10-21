# CodeLeveler 架构与使用说明

[English](ARCHITECTURE.md) · [自定义 Agent](AGENT_EXTENSIBILITY.zh-CN.md)

CodeLeveler 让 AI 模型在真实工作环境里完成任务。你可以提出目标，让它读取和搜索文件、执行命令、修改代码，并通过界面了解过程和结果。模型决定怎样工作；程序负责把调用真正执行、保存记录，并处理权限、取消和恢复。

编程是当前第一个产品方向。底层运行基础也面向评审、研究等其他智能体。增加这些领域时，共用已有的任务、权限和持久化机制，各自定义工作规则。

本文前半部分说明用户会遇到的行为，后半部分保留开发和排障所需的技术约束。它与英文架构文档表达同一套契约，中文按使用过程组织。

| 想了解什么 | 阅读位置 |
| --- | --- |
| 从哪里使用、如何开始工作 | [入口](#1-可以从哪些地方使用)与[任务过程](#2-一次工作怎样进行) |
| 工具组、错误、diff 和快捷键 | [终端展示](#3-在终端里怎样看过程和结果) |
| 是否需要批准、授权如何复用 | [权限](#4-权限决定哪些动作能执行) |
| 怎样看完成、断线和取消 | [任务结果](#5-怎样判断回答和任务是否结束)与[恢复](#6-断线继续和取消会发生什么) |
| 后台进程、Agent 和技能 | [后台与协作](#7-后台命令与多智能体) |
| 模型、上下文、预算和隐私 | [模型与数据](#8-模型上下文和数据) |
| 协议、平台限制与实现责任 | [技术说明](#技术说明) |

## 1. 可以从哪些地方使用

当前仓库已有以下入口，其中目录名表示仓库内的客户端实现，不等于已发布的安装包。Windows 的执行隔离仍未完成实机确认，托管后台进程的安全后端也未实现，具体限制见[权限执行契约](#90-permission-mode-顶层产品契约)。

| 使用方式 | 入口与当前范围 |
| --- | --- |
| 命令行 | `leveler` |
| 终端界面（TUI） | `leveler tui` |
| Web | `leveler web` |
| 移动端 | `apps/leveler-mobile` |
| 远程宿主桥 | `leveler remote` 与 `services/leveler-relay` |

独立桌面应用和云端 Worker 尚未发布。`apps/leveler-desktop` 已有 Electron 骨架，它与发布产品的状态分开记录。

这些客户端使用同一套运行时和业务协议。客户端可以位于其他机器上；“同一套运行时”也不要求所有项目共用一个后台进程。

### 选择项目或无工作区会话

项目会话有一个主要工作区，可以使用该项目的配置、规则、技能和相关能力。无工作区（No Workspace）会话没有关联目录，也不会偷偷使用 HOME、启动目录、上次项目或内部临时目录代替。

无工作区会话使用同一套运行时，但不加载项目配置和规则、自动执行钩子、技能、项目外部工具连接、Git 与代码分析、文件恢复点或命令环境。默认需要工作区的工具会拒绝调用，即使使用 Full 权限或传入 HOME 的绝对路径也如此。这属于缺少工具所需资源。独立的网页、搜索和浏览器能力仍遵守各自配置与权限规则。

全局历史汇总已有项目与无工作区的持久记录。项目目录丢失时，历史保留并标记工作区不可用；某个来源读取失败时，返回其余结果并说明缺项。扫描不会创建或迁移数据库，不启动后台进程，也不建立另一份任务记录。全局扫描只开放给可信本地客户端，某项目的远程或 Web 授权不自动开放其他项目历史。

## 2. 一次工作怎样进行

以用户要求“修复这个缺陷并验证”为例，实际过程如下。

1. 客户端接收目标。编程领域规则提供仓库约束、工具说明与完成要求。
2. 运行时建立任务与回合，持久保存输入，管理预算、事件、取消和恢复。
3. 模型理解目标，调查代码，决定下一步，通过工具发起调用。
4. 读取、搜索、命令等能力执行工作。涉及文件、进程或网络时，都经过同一个宿主执行边界。
5. 运行时保存实际结果，例如退出码、文件变化和产物。模型结合目标和证据判断是否继续。
6. 客户端展示过程与结果，用户决定是否接受交付。

模型看到的工具会按需要加载。每个新目标从基础编程工具、产品默认能力面和很小的能力控制接口开始；项目启用 memory 时，记忆工具与指导属于默认能力面，第一轮请求即可见，不需要先调用 `capability(enable)`。其他可选能力在加载后才把工具和指导带入后续请求。同一目标内已加载能力只增不减；恢复目标时恢复其已加载能力并保留默认能力面，新目标回到基础工具集合加默认能力面。

### 能力存在、权限允许和当前可见各有条件

| 条件 | 对用户的含义 |
| --- | --- |
| 可用（Available） | 运行环境实际具备能力，例如已配置搜索服务、PATH 中有 Git、模型支持图片、所选浏览器可驱动 |
| 允许（Permitted） | 配置、权限和操作系统隔离规则允许调用，包括记忆开关与委派权限 |
| 已暴露（Exposed） | 当前模型请求实际携带的工具 |

当前可见工具必须同时可用并被允许。加载工具不会创造能力或授予权限。模型决定需要加载什么，编程规则所在的领域适配层负责校验和加载。程序不按任务关键词、模型强弱、模型服务商或轮次擅自挑选工具。旧会话中的工作配置不再决定工具集合；聊天、计划、目标等协作方式也不会改变执行权限。

### 工作过程中怎样说明进展

编程领域的沟通规则由模型执行。开始工作时说明目标和首步；有关键发现、阻碍或方向变化时说明影响和下一步；长时间等待时说明当前状态与等待事项。说明应传递信息，避免逐条复述工具。界面只显示收到的模型正文，不生成发现、不编造下一步，也不把工具活动自动解释为目标进展。

## 3. 在终端里怎样看过程和结果

对话区按事件原顺序显示用户输入、模型的进展说明、工具回执、文件修改和最终回答。模型正文按自然段完整显示，不给每段额外添加 `●` 或 `◆`。

### 工具组默认收起，按可见阶段累计

连续同段的读取、搜索、列目录、执行命令和外部工具调用等普通工具合为一组。单次工具也使用同一个入口。标题累计真实种类与次数，失败仍计入种类，并另外显示失败标记与数量。

以下只示意布局，文件、次数和状态以真实记录为准。

```text
我先读取相关文件，再检查已有测试。

▸ 读取 3 次 · 搜索 2 次 · 执行了 2 个命令

发现一个边界情况，接下来核对失败条件。

▸ 读取 2 次 · 执行了 3 个命令 · ✗ 1 个失败
```

用户消息、进展正文、最终回答，以及独立展示的编辑、权限与用户决策、计划和目标管理条目，会划开工具组。模型轮次和思考记录不划组，界面也不会猜测阶段。

关闭时隐藏成功输出、失败原因和实时输出。点击组标题后，按真实时间顺序显示成员与各自详情入口。关闭父组保留成员的展开选择，重开后恢复，新成员默认收起。用户手动选择不应被后续事件随意覆盖。成功的静默内部工具不计入标题，也不划分阶段，其原记录和可用输出仍可通过详情查看。

正在运行或停止的工具必须在标题表达真实状态和数量。后台启动调用返回成功，只说明启动调用已返回，不能将仍运行的进程显示为完成。后台任务结束后按它的唯一身份更新原启动命令，已有启动记录时不重复追加通知；没有原记录时保留终态通知。临时通知与后台详情仍可访问。

### 失败收在入口里，diff 默认完整显示

普通工具失败默认在组内收起，标题显示失败数量。展开后才显示失败目标与脱敏错误。请求失败等独立终态条目也默认收起，点击自身标题看详情；不能显示没有实际交互入口的“查看详情”。收起只改变显示，错误来源、错误码、能否恢复和原始事实保持不变。

已确认的文件编辑独立展示，保留文件名和修改统计。长到会淹没这一轮的 patch 在会话里做摘要：保留结构、每个变更簇的首尾和隐藏行数，因为几百行的 diff 会把这一轮其余内容挤出屏幕；打开该行（`Ctrl+O` 或点击）显示全部行，任何改动都不会够不到，短到能读完的 patch 直接完整显示。全屏 Diff 页始终保留完整内容。工作区 diff 读取失败必须明确显示失败，不能当成没有修改。移动文件时，diff 两侧路径对应真实源路径和已提交目标路径。

### 思考与快捷键

思考进行中在对话里单独显示一行：标题是 `◆ 思考中…`，正文在左侧竖条里滚动。这一行不进入工具聚合回执的成员。思考结束后该行离开对话，展开回执或 Ctrl+O 也不会把它恢复进去。状态栏在实际收到思考时仍显示思考，结束后清除。隐藏已结束的思考不会删除原始记录，也不改变模型协议要求的历史回传。

| 操作 | 结果 |
| --- | --- |
| 点击工具组标题 | 展开或收起成员 |
| 点击成员或失败条目入口 | 查看其具体详情 |
| Ctrl+O | 展开最新执行（工具组或用户命令 `!command`）的详情，在原处展开该块；范围不扩大到全历史，无执行时不会改为展开思考 |
| Ctrl+G | 跳到最终回答；没有最终回答时仅提示，不移动视口 |

普通命令在对话原处展开，显示原始参数中的完整命令，并按终端宽度换行。底部运行状态不重复长命令。Markdown 的代码围栏无论闭合与否都保留全文，不按源行数或换行后的显示行数裁切。

### 本地命令（Shell Mode）

空 Normal 输入框里输入 `!` 进入 Shell Mode。`!` 只是模式标记，从不进入命令文本；输入框的提示符、描边和占位文本随之变化，但位置与尺寸不变。空输入框上的 Esc、Backspace、Ctrl+C 退出该模式；全局键盘契约不变：Shift+Tab 仍是权限模式切换，Ctrl+J / Alt+Enter 仍插入换行，Tab 只在 Shell Mode 内做补全。

补全完全离线且确定：文件、目录（补 `/` 并可继续进入）和 PATH 可执行文件；多个候选的公共前缀只填充公共部分；候选过多或扫描被截断时打开菜单而不是自动接受；接受候选从不执行命令。候选菜单打开期间拥有 Tab/Enter/方向键/PageUp/PageDown/Esc，并支持鼠标悬停、点击和滚轮。

每次执行在对话中只有一条内联记录，与输入框上方的固定实时活动行来自同一个运行时事件。左侧状态轨区分 Running（紫色、按时间呼吸）、Ran（绿色）、Failed（红色，带真实退出码）、Cancelled（琥珀色，明确取消）；stdout 与 stderr 保留来源底色；完整命令独占一行 `$`；终端输出经过安全投影，折叠时明确写出标记，展开后每一行仍可达。超过内联预算的结果不以提高内存上限解决：记录保持有界预览，Enter 或点击标题在 TUI 内打开完整输出页。该页只持有逻辑行字节范围与按当前宽度换行的行数索引，按视口从 `combined.log` 读取并渲染可见行；`g`/`G`、PageUp/PageDown、滚轮、拖选复制和 `a` 复制全部均可用。窗口宽度变化时后台重建索引并保持顶部逻辑行。捕获目录权限为 `0700`/`0600`；会话存续期间保留其全部完整捕获，不按数量提前回收（记录仍引用这些文件），删除会话时整目录回收；恢复时持久化路径必须先校验位于本地 `shell-output` 根下且 `combined.log` 是普通文件。内联记录标题最右侧的停止区与实时活动行都可点击；Ctrl+C 精确取消该 execution，不会取消 Agent、旁问或应用。同一时间只允许一条前台用户命令。恢复会话时从运行时事件还原记录，不重新执行。运行中的前台命令可用 Ctrl+B，或点击内联标题/实时活动行的“↗ 转后台”，把同一个 execution、同一个进程和进程组交给现有后台任务注册表：不重新 spawn、不重跑命令、不追加 `&`。执行 ID 不变，输出、退出码、时长与取消结果连续；实时活动行消失，记录保留并标记 Background，现有后台列表按 task id 找到它。Ctrl+C 只取消前台 execution，后台任务只由后台停止取消。恢复会话不重新执行后台记录；运行时重启后没有终止事实的记录明确显示状态未知，不伪造 Running 或 Completed。

### 滚动、任务详情与状态栏

有内容的对话从视口顶部开始，不满一屏的空行留在下方；只有空会话的欢迎画面居中。超过一屏后正常滚动并沿用跟随末尾行为。折叠、自动折叠或工具组重组要保持当前阅读位置；正在读的正文被折叠时停在对应入口附近，跟随末尾时继续贴底。

后台任务页保持上方列表和下方详情，任务身份与停止操作固定，仅详情内容滚动。完整命令和日志按宽度换行；鼠标点击选任务，滚轮按所在区域选任务或滚动详情。每个任务保留自己的滚动与跟随状态，停止结果由运行时确认。审批、提问和选择弹层在可绘高度内保持焦点及决策块可见，窗口大小变化后回到焦点。这些交互不是独立面板：它们在对话上方的一条普通分割线处开始，沿用对话自身的背景与分隔线颜色，问题只在弹层里出现一次——正在询问的调用行和状态栏在该弹层打开期间都不重复它。

状态栏的上下文占用按运行时预计输入量除以完整模型窗口显示。只有没有运行时快照时才使用模型服务报告的用量；窗口或用量未知时不伪造占用。详情 `/context` 分别说明输入容量、模型窗口和输出预留；压缩阈值仍按输入容量计算。8 格条随用量变化，无动画，低于 70% 使用普通提示色，70-89% 使用警告色，90% 起使用错误色。窄窗口依次隐藏占用条、缓存和容量，时钟与权限、协作控件优先保留。

复制文本也区分“已发送”与“已验证”。仅本机非 tmux 的原生剪贴板写入且读回一致才标记已验证。SSH 与 tmux 跳过原生路径；tmux buffer 和 OSC 52 只表示已发送，送达未经验证。程序不会为复制静默修改 tmux 配置，终端诊断也不读写剪贴板。

## 4. 权限决定哪些动作能执行

权限模式（Permission Mode）描述 CodeLeveler 产品运行时的权限。它不替代用户对具体开发、部署或数据操作的授权。精确执行与平台支持契约保留在后文 §9.0。

| 模式 | 用户会遇到的行为 |
| --- | --- |
| Full（`FullAccess`） | 已实现能力完全放开，CodeLeveler 不因权限或操作系统隔离询问、拒绝或限制，审批次数为 0 |
| Auto（`Assisted`） | 普通开发默认允许，包括全部 Git 操作，以及工作区内字面路径的 `rm` / `rmdir`；只在工作区外或看不清目标的删除、提权，以及凭据、工作区外写入、会执行本地程序的 Git 配置上要求批准 |
| Restricted（`RequestApproval`） | 按目的地和资源边界执行，无法可靠约束的范围明确询问、拒绝或标记不支持（Unsupported） |

Full 下能力缺失、参数错误、操作系统或外部服务失败仍会如实报错；取消、超时和资源回收照常有效。Auto 的批准只覆盖当前明确动作。执行时沿用同一批准，不再次拒绝，也不扩大之后未批准的调用。操作系统隔离不能比已允许的权限更严。

### 批准一次、会话或项目的区别

Once 只批准当前调用，不保存为可复用记录。Session 绑定真实持久会话，Project 绑定真实项目对象。复用必须重新解析和校验资源，文件、代码仓库、远端目标或凭据改变后，旧记录不能继续代替新批准。文件批准绑定精确路径与实际对象，新文件绑定现有父目录，不能靠目录前缀放大范围。

读写、删除、远端读写、后台观察与控制、凭据使用与读取明文都是不同能力。批准使用凭据不自动允许读取凭据文件或访问网络。授权界面只展示宿主解析的请求，模型描述和界面状态不能生成执行权限。无法证明真实资源与执行效果时，仍使用精确本次调用的审批，不提供可复用授权。

### 浏览器和搜索分别启用

浏览器控制与网页搜索独立配置，互不替代。缺少搜索服务的密钥不会改用浏览器搜索，浏览器失败也不会转用搜索。网页搜索有一份与后端无关的工具契约，更换后端不改变参数范围或结果外壳。

浏览器控制分别提供导航与标签页、像用户一样操作、观察这三个工具，每个入口表达一种动作。自动化浏览器会话不等于接管用户已有窗口。Safari 自动化使用与普通浏览数据隔离的独立窗口，不能读或控制已有标签页。将 URL 打开给用户看由操作系统默认浏览器处理，不让自动化工具获得这个标签页、网页内容引用或控制权。

浏览器在会话开始前按调用指定、配置默认、本机自动化默认的顺序选择。本机默认只从 Chrome、Edge、Chromium 中依次选首个可驱动且提供控制台、页面错误与网络观察通道的浏览器，Safari 必须显式指定或配置。指定产品无法驱动时明确报错并说明选择来源；会话启动或操作失败后不换浏览器。

## 5. 怎样判断回答和任务是否结束

命令成功、测试通过、文件变化和模型说“完成了”，分别提供不同证据。测试通过不能自动证明用户需求已全部实现。运行时记录可确定的执行事实，模型结合领域规则解释证据，用户拥有最终验收权。模型不能通过缩小自己声明的验收范围来宣布用户委托的整体任务完成：用户明确枚举的交付项必须逐项记账，缺失、未完成或被机械事实反驳的要求都会拒绝完成。

最后一段正文结束后，任务进入收尾（`Finalizing`），还可能需要依赖结算、必需评审和结果裁决。只有运行时持久保存的任务结束记录（`TaskFinished`）才能确认已结束。没有提交终态时，界面只能显示可恢复错误，不能自行合成失败或完成。

| 结果或状态 | 能证明什么 |
| --- | --- |
| 已回答（`Answered`） | 已回答，修改文件不会自动将其升级为完成 |
| 声明完成（`Completed`） | 模型声明目标已完成，仍需实际验证；不能独立证明格式、构建或测试通过 |
| 未能继续推进（`Stalled`） | 保留原因，作为可恢复中断处理，未完成目标不结清 |
| 工具成功或最终正文 | 本次调用或输出发生过，不能替代任务终态 |

测试、编译和代码检查是普通工具。运行时不会在任务结束后另起验证计划，也不会自动为会话生成“验证通过、失败、未运行或不可用”的状态。模型按用户要求通过命令执行验证，说明只传播实际观测，缺少观测时标记未知。

普通建议性评审是模型意见，不阻止任务结束。配置为必须执行的评审仍可产生完成警告；`/develop` 工作流明确要求评审通过，未通过时停在可恢复边界。不同工作流要求不能互相套用。

权威终态提交后，产品立即公布结果。已绑定具体任务与运行记录的后台收尾可以继续，但不能延迟结果或让客户端重新显示运行中。

## 6. 断线、继续和取消会发生什么

输入持久保存后才返回接收确认（ACK）。确认只证明收到输入，不证明任务完成或工具副作用只发生一次。无响应和断线也不能证明调用没有执行，未知结果不允许自动重跑。

运行时保存会话、任务、回合、事件、所有权、使用量和产物等事实，先持久化再传播给客户端。断线后的快照恢复正文、计划、终态及仍存活的审批或提问入口；已答复、超时、取消或重启后失效的旧入口不能重新操作。

重启不会自动续跑旧模型流。旧回合根据原执行者的身份和存活情况标记中断或未知，恢复从持久记录重建。有结果未知的变更工具需要人工核对，不能自动重放。断流、取消或连接异常结束前已接收的正文保留为未完成消息，未完成的工具参数和未经协议签名确认的思考不冒充完整回答。

无附件的“继续”由统一命令入口识别并进入原任务的恢复操作，沿原目标和计划继续，补充要求追加到原目标。有附件时仍走内容与图像校验。是否可恢复由运行时判定，显式取消的任务拒绝继续；无可恢复任务时保留普通消息语义。

### 停止当前回合和取消任务

| 动作 | 后续行为 |
| --- | --- |
| 停止当前回合（`CancelCurrentTurn`） | 中断当前回合，保留可恢复任务 |
| 取消整个任务（`CancelTask`） | 保存已取消结果，结清目标，并拒绝之后继续 |

取消经历请求、送达、执行终止和终态持久提交。接收确认只表示请求已送达拥有该任务的运行时；回合还活着时显示“正在停止”，提交终态后才能显示“已停止”。持久取消先于完成提交时，结果必须为已取消；已提交的完成不被迟到取消覆盖。

用于继续工作的恢复点必须在任务结束记录之前保存。写失败保留具体错误、原始转录与活跃目标，进入可恢复中断，不假称保存成功。取消与重启恢复的事务和身份约束见技术说明。

### 多客户端同时改设置

标题、模型、默认模型、权限、思考选项和协作方式等设置，按客户端实际看到的日志版本更新。别人已修改时，本次冲突会明确报错并刷新，不能自动拿新版本覆盖。相同命令身份的重复请求按已有结果返回。会话默认模型已变但全局配置写失败时，明确报告部分结果，不能盲目重放。

## 7. 后台命令与多智能体

后台进程从启动时明确寿命，不根据界面是否打开猜测。

| 生命周期 | 何时停止 |
| --- | --- |
| 随目标（`goal`，默认） | 创建它的目标结束时回收 |
| 随运行时（`runtime`） | 不随目标结束清理，当前运行时退出时回收 |
| 随会话（`session`） | 启动起由独立执行宿主托管，显式删除创建会话时停止 |
| 持续运行（`persistent`） | 用于用户明确要求跨运行时重启运行的服务或监听程序，自行结束或显式停止时结束 |

运行时更新不会停止已托管服务，也不将旧本地任务自动转换为托管任务。独立执行宿主异常退出属于独立故障，不能保证服务随之退出。无法确认进程身份时显示未知或需要人工恢复，保留其未结算的写入归属，不向可能已被其他程序复用的进程号发送信号。

读取日志、等待与停止使用统一后台接口。Auto 与 Restricted 校验创建会话，允许同会话跨回合操作，跨会话要批准；Full 或已批准调用由宿主按明确的无限制执行权执行，不伪造创建者。

### 日志与退出状态

日志有容量上限，客户端读取慢时仍持续读取进程输出，避免进程卡住；丢弃内容会明确标记截断，不把结果说成完整。等待进程结束不会因新日志而提前返回或结算。命令及其所有子进程都结束后，才统计和结算文件变化；仅启动它们的命令解释器退出还不够。

等待接口在子任务状态变化、调用超时或取消后，一次交付期间累积的新日志，长任务不必每个等待区间请求一次模型。任务详情接口读取完整保留日志，不同观察者可以独立记录读取位置；历史被截断时明确报告缺口。查看日志不删除任务结算事实，不同读取者看到的失败一致。

### 子智能体共用哪些约束

产品已有探索、执行和评审等子智能体角色，支持父子会话，也支持项目或用户定义的 Agent。角色不同仍共用任务生命周期、所有权、写入范围、能力、取消、结算和持久化。子智能体不能建立第二套写入、权限或任务记录。自动委派由模型选择，不保证一定更快或便宜。

声明式 Agent 使用 `.leveler/agents/<name>/agent.yaml` 与 `instructions.md`，由编程领域的定义目录解析和校验。定义只能收窄现有能力类，不能新增运行时角色；创建子智能体时保存定义快照，重启仍按该快照运行。模型决定何时使用哪个 Agent，引擎不按文件类型或任务内容挑选。完整用法见[自定义 Agent](AGENT_EXTENSIBILITY.zh-CN.md)。

技能（Skill）是由 `SKILL.md` 和可选脚本、参考资料组成的知识包。目录名是唯一名称，文件头声明不一致会标无效。项目定义优先于用户定义，用户优先于内置定义，同一范围的来源顺序固定；被覆盖定义仍可诊断，高优先级坏定义不会悄悄退回低优先级。

技能目录兼容项目和用户范围中的 CodeLeveler 原生、Codex、Agent Skills 和 Claude 来源，按固定顺序处理名称、优先级、覆盖和状态。兼容 Skill 直接读取原目录，包括 `~/.codex/skills`、`~/.agents/skills`、`~/.claude/skills`，外部更新后，下次读取定义就能看到。索引仅有名称、范围和描述，`load_skill` 或 `$name` 按需读完整正文。定义目录的读取与修改分开，CodeLeveler 管理的技能先校验、再用户确认、最后原子替换；外部和内置技能可以加载，不能在这里改写或删除。Agent 和 Skill 都属于产品与领域层，不下沉进引擎。

## 8. 模型、上下文和数据

不同模型在同一套领域规则与运行时上工作。差异用明确能力表达，包括上下文窗口、思考支持、并行工具、图片、结构化输出、工具调用和最大输出，不靠模型名称猜行为。模型真实缺能力时明确报告，不用隐藏行为假装支持。

模型负责理解、规划、工具选择、调试和证据判断。运行时修复并发、取消、恢复、权限和一致性等工程问题，不替较弱模型规划。领域层可以根据重复验证、长期无有效进展或重复失败等可观察行为提供反馈；所有自适应策略必须可观察、解释、关闭、度量、A/B 验证且与模型身份无关。模型更强时，非必要干预应减少。

### 模型读到的上下文

当前契约、项目规则、选中技能、记忆与执行状态每次请求重新组装。真实用户输入、模型输出、工具调用和结果构成对话转录。两者来源和用途分开，工具或附件中的命令式文字仍是待分析数据，不能自动取得项目或用户指令权限。

长历史超过实际请求阈值时，系统保留目标、规则与最近工作集，将中段替换为模型写的简报。简报可能有遗漏，属于建议性上下文；原始转录保留。摘要不可用时明确说明细节不可用，不能暗示仍有完整历史。模型协议要求回传哪些历史思考内容，由协议决定，隐藏思考的界面设置不改变回放。

消耗包括每次模型尝试、失败、重试、压缩摘要和子任务。没有用量报告或价格信息不等于免费。子任务和 `/develop` 阶段共享父任务剩余预算，并发已发请求按实际返回用量结算；预算只能阻止后续调用，不能撤销已发生消耗。耗时包含模型、工具与批准等待，不能都说成模型思考时间。

### 数据会发到哪里

CodeLeveler 默认不向项目方或中心服务收集、上报产品遥测和用户工作数据，包括源码、文件内容、会话、提示词、模型输出、命令输出、环境、用量和崩溃上下文。

用户启用的模型服务、MCP 外部工具连接、网页搜索、远程执行或更新检查仍可能联网。每次出站由明确功能触发，目的地与用途可解释，只发送必要数据，不通过隐藏替代路径扩大范围，也不把秘密当遥测。未来诊断、崩溃上报或分析须由用户明确选择启用，提供关闭选项，并与核心执行分开。

## 技术说明

以下用于开发和排障，保留字段、协议与安全机制的精确名称。普通使用可以按遇到的问题查阅。

### 责任划分与依赖方向

| 组件 | 唯一负责的事情 | 不能接管的事情 |
| --- | --- | --- |
| 模型 | 理解、推理、规划、选择工具与判断证据 | 文件系统、进程或任务生命周期 |
| 领域适配层（Harness） | 领域词汇、约束、工具面、上下文、协作及完成语义 | 通用生命周期、持久化、权限或隐藏推理 |
| 内核（Agent Kernel） | 模型与工具循环、流式输出、预算、usage、重试、超时和取消 | 编程、评审、仓库工作流、产品提示词和用户验收 |
| 引擎（Engine） | 会话、任务、回合、事件、恢复、后台父子关系、所有权和结果 | 领域规划与语义完成判断 |
| 能力（Capability） | 可复用执行逻辑、必要状态、生命周期与一致性 | 某个工具或产品的展示逻辑 |
| 工具（Tool） | 模型意图入口、参数解码、调用能力和表达结果或错误 | 全局状态、服务发现、权限裁决或进程生命周期 |
| 宿主执行权威（Host Authority） | 唯一执行真实文件、进程、网络副作用及权限边界 | 领域完成判断或根据 UI 改事实 |
| 持久化（Persistence） | 任务、回合、所有权、证据、用量、产物和后台工作的唯一事实源 | 竞争性重复记录 |
| 模型接入层（Provider / Protocol） | 请求响应、流、工具协议、reasoning、结构化输出、视觉和能力限制 | 改领域语义或为特定模型注入隐藏行为 |
| 产品与客户端 | 交互、导航、能力组合及本地显示状态 | 创造终态或绕过运行时管理长期任务 |

运行时由内核与持久化引擎组成。引擎管理生命周期，模型负责智能，领域层定义语义。编程、评审和研究领域是平级关系，新增领域复用通用能力，无需继承另一领域的语义，也不应要求重写内核。

依赖从产品经领域层朝运行时、能力与基础层发展。运行时不依赖具体领域，领域层不依赖具体界面，基础层不知道编程和评审，能力不反向依赖调用它的工具。改变职责不能削弱机械正确性、安全、持久化、取消、恢复或一致性。

能力是一种职责，不要求拆成独立包或进程。新包、trait、manager、registry 或框架需要两个真实实现或消费者、依赖倒置、独立安全/协议/持久化/生命周期/远程部署边界，或已观察到的所有权与耦合问题。仅有未来设想或图形对称不构成理由。本地、远程和云端后端可以更换执行位置，但不能改领域语义或绕过统一权威。

### 执行反馈、请求账本与上下文控制

模型每轮获得的执行状态是权威状态的只读投影，而不是新的任务账本。内核通过 `AgentHarness::request_context` 接收领域层的请求级上下文；编程 Harness 从现有 `LoopContext`、进展账本和 Plan 生成有界反馈，包括累计耗时、资源消耗与上限、已执行命令数、观察到的修改数和模型声明的计划状态。

投影只附加到当次模型请求。不追加到持久聊天历史，不进入压缩摘要，继续或恢复时从原权威状态重新生成。上下文统计与缺失 usage 时的估算必须覆盖实际发送的投影。耗时包含模型、工具和授权等待，不能解释成纯模型推理时间；估算 token 与报告值可区分，未配置的预算保持未知／无上限，不伪造为零。

模型请求将领域层的控制上下文（`ControlContext`）与对话转录分开。控制上下文复用有名称和稳定性标记的 `PromptSegment`，包含当前契约、项目规则、选中技能、记忆召回和请求级执行状态。新回合与恢复使用同一组装入口；协议适配器把控制上下文编码到系统通道，不能改成用户意图。生命周期为单次请求的块挂在转录之后，其余块构成稳定的请求前缀，这样每轮变化的观察不会使历史的 prefix cache 失效。上下文统计和压缩压力计算包含控制上下文，但历史摘要只处理转录。

转录保存真实用户输入、模型输出、工具调用与结果，以及必要且有界的协议修补和产品定义的运行时通知。控制正文不作为历史消息落盘。持久层的 `role=user` 只是传输角色，不表示用户意图。新写入的用户输入和 Harness 消息在 payload 的 `origin` 字段记录来源；没有该字段的旧行仍可读取，也不会被改写。已发现的子目录规则来源路径保存在既有进展账本中，恢复时重读当前文件；兼容旧 session 时，只从旧 System 行恢复来源路径，忽略其过期正文，不修改历史数据库。来源不明的历史 System 文本不会获得契约或项目指令权威。来源不明的历史 User 文本也不会获得用户意图。记忆检索和 reasoning 回放保持不变。每一段如何被解释见[指令与数据的来源和权威](#指令与数据的来源和权威)。

每次模型尝试都先进入现有请求记录与资源账本，再决定是否重试，包括失败、仅产生 reasoning 的断流、压缩摘要和子任务调用。缺失 usage 或价格不能记为免费。子任务、评审和 `/develop` 各阶段消费父任务的剩余预算；重试次数与请求尝试记录不得重复累计。等待模型流时，取消与 deadline 必须能在没有任何新事件时唤醒；仅产生 reasoning、尚未收到正文或工具调用数据的瞬态断流，可以由有界的逻辑重试生命周期自动恢复。reasoning 仍是已生成且可能计费的内容，不能冒充未生成而安全重放；失败尝试先结算，再重新准入，新的尝试重置实时思考，不将放弃的 reasoning 合并到成功回答。已收到正文或工具调用数据的流不自动重放。

共享预算在每次模型请求及重试前从同 scope 的持久请求事实检查，后台子任务不能一直使用启动时的余额快照。已经发出的并发请求仍按返回的真实 usage 结算；预算阻止后续调用，不能撤销已发生的消耗。

请求记录保存所属 goal 或根 turn 的 `budget_scope`，以及与 provider 实测值分开的 token 估算。恢复从该 scope 的请求事实重建模型消耗，覆盖“请求已落库、进度事件尚未落库”的崩溃窗口；同 session 中不属于该 scope 的任务和辅助调用不能混入。收费的回合准备摘要也必须发生在 Engine 持久化 initiating turn 之后。旧记录无法证明预算归属时，不得通过清零旧消耗继续受限任务。

压缩压力以实际下一次 `RequestProjection` 为准，保留原始 transcript。回合内、下一用户回合和恢复使用同一 `ResolvedContextPolicy`；24K 仅是未声明窗口时的后备值。手动压缩只有在摘要非空、正常停止且不含工具调用时才能切换 context epoch，拒绝或失败保留旧 transcript 与 checkpoint。

Goal 的文字和附件通过同一 Goal 回合入口持久化，附件不能把回合降为 Chat。自然停止的 Answered 只表示已回答；会话列表从最新 `TaskFinished.stop` 区分“已回答”和“声明完成”，不改变会话生命周期状态，也不把声明当成验证。

断流、取消或异常 EOF 前已观察到的正文通过既有 TranscriptSink 保存为未完成的 assistant 消息；原始行的 `incomplete` 元数据用于重开历史投影。未完成工具参数和未签名思考不作为完整回答保存。继续任务的 checkpoint 写失败保留具体错误、原始转录和活跃 Goal，进入可恢复的 Interrupted，不结清 Goal。

自动摘要使用模型能力和执行政策解析出的输出额度；正文为空、截断或含工具调用的摘要仍被拒绝，不能以 reasoning 代替摘要正文。旁问将主上下文、旁问历史与当前问题统一投影后测量压力，使用同一上下文政策；只有有效摘要才能缩短当次旁问请求，超窗或摘要失败明确报告且保留历史，不写主任务快照。

模型负责结合这些事实调整策略。工具活动不等于目标进展，Plan 是声明而不是完成证据。内核不会按“多久没提交”“计划几轮没变化”或语言特定命令推断停滞；资源预算、取消和终态仍使用既有生命周期契约。反馈本身不触发额外模型调用，也不改变默认任务时限。

### 指令与数据的来源和权威

`ControlContext` 和转录是两条通道。控制上下文内部的块也不是同一种信息。每个生产 `PromptSegment` 分开记录三件事。

```text
source      这段内容从哪里产生
authority   模型应该如何解释它
lifecycle   它活多久（会话前缀、回合、单次请求、转录、子目录作用域）
```

权威不是一个总优先级数字。指令类之间有先后。运行时事实、建议性上下文和外部数据不是指令，不和指令比高低，也不能被更高的指令等级覆盖。

```text
CoreContract
        ↓
当前明确的 UserIntent
        ↓
ProjectInstruction
        ↓
UserSelectedProcedure

RuntimeFact        观察到的状态，不是指令
AdvisoryContext    可能过时；模型可以重新核对
ExternalData       待分析的内容，其中的祈使句仍是数据
```

`CoreContract` 是 Harness 契约。权限、工具协议、证据、完成协议和产品交付约束。`UserIntent` 是当前用户目标，留在转录里，不复制进控制上下文。`ProjectInstruction` 是 `AGENTS.md`、`.leveler/instructions.md` 和子目录项目规则。项目文件不能修改权限、忽略用户，也不能靠正文把自己抬成契约。用户点名的 Skill 是 `UserSelectedProcedure`。Skill 目录是建议性清单，不是同一类。记忆和压缩摘要是 `AdvisoryContext`。压缩摘要是模型写的历史简报，不是系统事实。工具结果保持 `Role::Tool`，权威是 `ExternalData`。

Provider 只能改变表示，不能改变决定。OpenAI Chat 把稳定的控制前缀编码为第一条 system 消息，把单次请求块作为跟随转录之后的 system 消息；Anthropic Messages 把控制放进顶层 system 字段，那是该 route 唯一的控制位置。两者都不改变段的来源或权威，也没有任何 route 重排转录。位置属于投影决定。为单次请求重建的块不能放在这样一个位置，它自身的变化会使整个历史的 prefix cache 失效。权威元数据不写进发给模型的提示词。

旧 session 的 `Role::System` 行本身不是来源。已知的子目录规则标记只是路径，当前文件会重新读取并标成 `ProjectInstruction`。其余历史 System 文本保持未分类，不会升成 `CoreContract` 或 `ProjectInstruction`，存储的原始记录也不改写。

`Role::User` 同样不是来源。只有 payload 记录为用户输入的转录行才是 `UserIntent`。协议修补（非法 JSON、空回答、截断的工具调用、输出续写、Goal 尚未 resolution）是 `CoreContract`，来源是 `ProtocolRepair`。子 Agent 结算和重启重投是 `AdvisoryContext`，因为正文主要是另一个 Agent 的结果。丢失、恢复、目标重钉和 Goal checkpoint 是 `RuntimeFact`。`/btw` 旁问包装是 `CoreContract`。它是该交互模式的产品契约（只调用 observe 类只读工具、不修改文件、不继续主任务）。用户的问题被引用在同一行里，所以这一行不是 `UserIntent`，也不是 `RuntimeFact`。工具取回的图片改用用户传输角色时仍是 `ExternalData`。压缩摘要仍是 `AdvisoryContext`。新行记录 `CompactionSummary`，旧行靠既有 breadcrumb 标记识别。没有 `origin` 的历史 User 行是 `LegacyUser` / `Unclassified`，不会因为角色名叫 User 而升成 `UserIntent`。权威元数据不写进发给模型的提示词，Provider 编码也不改变上述权威。

### 上下文的保留与释放

一个请求的实际内容由两个 owner 依次决定。生命周期决定哪些语义块还在活动表面上，`RequestProjection` 决定这些块在该 route 上能否、以什么表示进入请求。压力也按同一个结果衡量。折叠的触发依据是**真正要发出的那次请求**的投影，包含控制上下文、工具 schema 和 reasoning 通道。任何只按转录字符数或“不含工具”的请求形状估算的路径都是错的，会让 provider 校验的字段从 wire 上消失，并低估压力与账本。

各类内容的生命周期。

| 内容 | 活动表面 | 持久事实 |
| --- | --- | --- |
| 控制上下文（契约、项目规则、技能、记忆召回、执行状态） | 每个请求重新组装 | 转录行（若落盘）不因此改写 |
| 用户目标 | 转录头，折叠时重钉 `<objective>` | 用户输入行 |
| 工具结果 | 逐结果上限截断后保留，直到折叠释放 | 原始行 |
| assistant 可见正文 | 直到折叠释放 | 原始行 |
| historical reasoning | 由 route 的 replay 契约和保留策略共同决定 | 原始行 |
| 压缩摘要 | 折叠产生的简报行 | `CompactionSummary` 行 |
| 作用域项目规则（中途 System 行） | 跨折叠**原样携带**，不进摘要 | 原始行 |

reasoning 是一个 route 事实，不是 harness 偏好。当某条 route 声明对携带工具的请求回传 reasoning 时，工具交换的 reasoning 属于 provider 校验的协议内容。请求的保留策略不能把它删掉，差异必须记成 `protocol_protected_turns`，不能假装策略已生效。因此历史 reasoning 的增长只能由生命周期释放，折叠把它连同承载它的回合一起移出活动表面，而不能靠“永久少发一点 reasoning”来省。反过来，当 route 没有 reasoning 通道时，一个都不发，并报告捕获数与携带数；这不改变转录，也不改变后续 route 看到的语义。

折叠（`compact_messages`）的语义固定。

- 只在投影后的请求超过阈值时触发；
- head 是系统行与第一条用户消息；中途的作用域规则作为持续约束原样携带；
- 尾部工作集同时受消息条数与 token 预算约束，且从不切入一个工具交换；
- 被省略的中段由模型写的交接简报替代；简报是 `AdvisoryContext`，不是系统事实；
- 折叠路径若摘要不可用，明说细节不可用，而不是暗示历史仍在；
- 二次折叠把已有简报合并更新，而不是从零重述；
- 原始转录始终是运行时加载的那些行，折叠只影响活动表面，恢复从持久快照加精确尾部重建，已释放的 reasoning 不会复活。

压力公式。`threshold = min(quality_boundary, window - output_reservation - headroom)`；`quality_boundary` 未声明时退化为容量。固定成本（控制上下文 + 工具 schema）已经计入投影，所以可用的动态预算就是阈值减去固定表面。headroom 默认为 0。没有测得的多余余量就不臆造数字。

多 Agent 沿用同一规则。父 Agent 拿到的是子 Agent 的结算结果（有界），不是它的完整转录，子 Agent 的 reasoning 生命周期属于它自己的席位。

可观测性。每次模型调用的投影大小与其中的 reasoning 大小都落库（`projected_input_tokens`、`projected_reasoning_tokens`），活动请求的分项统计按类给出控制、工具 schema、用户/assistant 正文、reasoning、工具调用、工具结果和压缩简报，因此“这一段为什么还在”可以由投影本身回答。

### 模型档案与协议投影

模型档案声明的是**事实**。上下文窗口、模型能产出的 completion、长上下文召回预期开始退化的质量边界、它说的协议，以及该协议的兼容性怪癖。

它不声明 harness 要花掉多少这份容量。completion 预留、安全 headroom、历史折叠的压力阈值、以及最近历史的保留预算，都属于 **harness 策略**，按执行席位从这些事实解析出来。把它们留在档案之外，才能让两个席位以不同方式花掉同一个模型的窗口，而不必臆造第二个模型。

reasoning replay 遵循同样的切分。一条 route 的 replay 契约，什么时候回传已捕获的 reasoning、以及某个没有捕获到 reasoning 的 assistant 轮次携带什么，由协议及其兼容性事实解析，永远不由模型名或 provider 名决定。

Anthropic thinking 由显式 `reasoning.style` 决定。`adaptive_thinking`，或带 `budget_tokens` 的 `budgeted_thinking`。适配器校验预算与 output cap 的关系，并按原顺序保存、回放 signed / redacted thinking blocks；不伪造签名，不把无签名文本冒充 signed thinking。缓存读取与写入都计入 input usage；价格档案无法表达缓存写入费率时，费用保持未知。

provider 可见的请求随后由**唯一一个 projection owner** 产出。语义对话输入，wire 编码器与上下文统计读取同一个结果。用于替代缺失 provider usage 的估算，估算的就是这份 projection，因此“实际发了什么”和“统计了什么”不可能分叉。

### 记忆的身份与生命周期

持久记忆是建议性、用户所有的上下文。项目来源（`AGENTS.md`、项目指令、当前代码）高于记忆；召回的记忆只记录写入时为真的事实，不会因此变成系统事实。

一个事实由规范化后的 subject 标识，而不是由标题标识。规范化是确定性的：ASCII 大小写、空白和标点折叠；中文 subject 使用稳定哈希；一小组显式别名把 `default_model`、`model_default`、`默认模型` 等已知拼写归到同一身份。未知拼写保持独立，不做模糊相似合并。标题只是人读的标签，可以变化而不改变身份。

所有写入路径执行同一套决策：用户直接命令、CLI、接受候选、后台提取、模型 `remember` 工具以及恢复。结果是 Create、Merge（严格兼容的超集）、Supersede（同一身份的新值）、Skip（重复或已被覆盖）、NeedsApproval（冲突、身份不确定或权威不足）。待确认是持久状态：候选被停放，只有人接受后才能成为当前真相。拒绝某个值只压制该值，不压制整个身份，因此以后合法的值仍可被提出。

事实权威与执行权限相互独立。用户直接陈述高于项目来源、运行时观察、工具观察和模型推断。`Full` 只允许 `remember` 工具执行，不提高模型的事实权威。模型只能用新证据替换自己此前记录的模型推断事实，不能静默替换用户事实；能替换用户事实的是用户后续自己的陈述，或被明确接受的候选。引用、粘贴、代码围栏或块引用中的文字不是用户在陈述，不能成为显式用户事实。

写入由项目级 lifecycle lock 串行化。Supersede 采用 new-first：新真相先落盘，旧值再进入历史，因此崩溃最多留下两个 active 文件而不是零个。读取从 supersedes 链推导当前真相，残留副本不会被当成当前值。历史保留且可审计，被替换或归档的值不会再作为当前真相被召回。恢复是明确的用户命令：创建新的当前版本并替换该身份当前值；已脱敏的敏感历史拒绝恢复。凭据形状的记录在 forget、过期、supersede 每个保留边界都会脱敏，因此历史与恢复路径不会复活秘密。

`leveler memory audit-identity` 只读地报告历史记录能建立什么身份：ready、needs_confirmation、possible_duplicate、possibly_stale、sensitive。它是 dry run，不重写用户记忆。

召回、catalog、常驻 preference、模型记忆工具和状态面读取同一份 effective-active 视图：生命周期状态、过期时间，以及每个身份 supersedes 链的终端条目。因此 Chat、Goal、Resume 和 Headless 看到同一份当前值。记忆正文不进入稳定系统前缀，catalog 只放标题用于发现。禁用记忆的项目不暴露记忆面，也不接收提取。

---

### 9.0 Permission Mode 顶层产品契约

`FullAccess`（Full）由宿主在 admission 之前选择 unrestricted 执行路径，绕过 CodeLeveler 权限规则、审批、sandbox、资源范围、凭据过滤和任务所有权限制。Full 下的文件、HOME、凭据、任意 Git remote、loopback / LAN / Internet、进程控制及已实现的未来能力均不产生权限 ASK / DENY。实际能力缺失、无效工具参数、OS / 程序或外部服务失败仍是失败；进程取消、超时、日志持久化和资源回收仍是运行时职责。

`Assisted`（Auto）默认允许普通读写、构建、测试、localhost、owned task，以及全部 Git 操作（`commit`、`fetch`、`pull`、`push`、`rebase`、`reset`、`clean`、`checkout --`、`restore`）。Auto 只在三类边界上请求批准：删除（工作区外、绝对路径、`..`、`~`、变量或通配的 `rm` / `rmdir`，以及 `dd` / `mkfs` / 关机；工作区内的字面相对路径直接执行。不是 `git clean` 或 `git reset`）、提权（`sudo` / `su` / `doas`、打开宿主应用、MCP、跨会话任务、agent / skill / memory 定义），以及安全（凭据明文、目的地已有已存凭据的 Git remote、工作区外写入、会执行本地程序的 Git 配置或参数，例如 `core.hooksPath`、`core.fsmonitor`、`alias.*`、`--upload-pack`、`--exec-path`、`GIT_SSH_COMMAND`）。叫不出远端名字的传输（`git fetch $REMOTE`、`git fetch --all`）无法绑定凭据，仍要批准。普通系统诊断（`ps`、`top`、`sysctl` 读取）和普通临时文件读写（工作区、`$TMPDIR`、系统 `/tmp`）都是普通开发动作。已存凭据仍在准入时询问，公共远端没有 credential binding，也不弹窗。Full 和明确批准动作的执行 authority 冻结在本次 `ResolvedExecutionPolicy`，`ProcessRequest`、文件工具、HTTP、MCP、任务控制统一消费；批准后的同一动作不再经第二套权限拒绝。单次批准不修改 session profile，也不扩大随后未批准的调用。

**Sandbox 不得比 permission policy 更严格。** 若 permission / risk 层判定 ALLOW，执行阶段的 OS sandbox 不得对同一 capability 静默返回 `EPERM`。因此 macOS seatbelt profile 必须显式携带 Auto 已允许的 capability。`sysctl` 名称到 OID 的翻译读取（`sysctlnametomib`，缺少它时连白名单中的 `kern.argmax` 都会被拒绝）、工作区之外的共享临时目录写入根、以及 `ps` / `top` 这两个 setuid 进程观察程序的 `process-exec (with no-sandbox)` 例外。最后一项是 OS 约束而非实现选择。内核禁止已沙箱化的进程 exec setuid/setgid 镜像，任何 SBPL 规则都无法放宽（`(allow default)` 同样失败），因此这两个只读观察者只能以沙箱外执行放行，且该放行按字面路径只对这两个镜像生效，`sudo`、`crontab`、`su`、`login`、`traceroute` 等其余 setuid 程序仍然 fail closed。若某 capability 确实无法安全进入 sandbox，必须在 permission 层 ASK 或给出明确结构化拒绝，而不是让上层看到裸的 `Operation not permitted`。

`RequestApproval`（Restricted）使用受限资源契约。`NetworkScope` 和 `NetworkResource` 描述目的地与已解析地址；HTTP broker 禁用环境代理、pin 地址，并逐跳校验 redirect。Full / 明确批准调用直接走 unrestricted HTTP 路径。范围分类本身不构成执行证据。

当前子进程后端支持矩阵。macOS 的 None 断网；Loopback **PARTIAL**，只保证 `127.0.0.1` / `::1` outbound，拒绝所有 listen（包括 loopback），不支持完整 `127/8`。实测 seatbelt 的 localhost inbound 条件仍允许 wildcard listener 接收 LAN 流量，因此不能使用。Linux None 使用 bwrap network namespace，宿主 Loopback / LAN / ConfiguredRemote 显式 Unsupported；Windows 受限网络范围显式 Unsupported。macOS LAN / ConfiguredRemote 也显式 Unsupported。Internet 与 Full 保留网络开放语义。ConfiguredRemote 的身份记录尚未接入真实 Git remote resolver / socket broker，不能声称已完成该范围。

Browser / MCP 无可靠目的地级进程隔离，拒绝部分网络范围，只在 Internet 或 unrestricted authority 下执行操作；旧浏览器 / MCP 进程自主出站仍存在尚未完成约束的生命周期问题。MCP 继承凭据的 launch authority 由宿主决定，不由服务器配置提升。模型 Provider、固定本地 RuntimeHost / ExecutionHost transport、用户配置 remote relay 与更新服务是独立基础设施用途，不因工具 scope 取得任意出站权。

Phase 3 / Phase 4 必须继续遵守本节。资源 grant 和 credential broker 只能约束 Auto / Restricted，不能给 Full 或同一已批准动作增加权限拦截。

权限架构主线验收状态为 CLOSED。 Permission Mode 契约、NetworkScope、Resource Grant、Credential Broker、`ApprovedGitTarget`、文件系统对象绑定与 Full bypass 不变量均已实现，并有真实执行验收。仍然 remaining、但不阻塞 mainline 的只有。Restricted 网络加固（loopback-only listen、`ConfiguredRemote` 的地址 pin / redirect、Linux host-loopback）、Windows / Linux 真实宿主 qualification、可选的更深 SSH brokering，以及非阻断的 release advisory。

#### Phase 3 可复用资源授权

本阶段验收状态为 CLOSED。 存储与资源解析已经建立。文件写入与删除使用对象绑定事务。提交本身是一次 `renameat2` / `renameatx_np` 的 `RENAME_EXCHANGE` / `RENAME_SWAP`（删除则为一次性 rename 移出），随后的判定基于真正被换出的那个对象，而不是提交前重新读取的名字。因此替换无法被静默提交。一旦发现条目已不是获批对象就恢复原对象并 fail closed，且永远不会 unlink 未经创建与验证的对象。对抗 race 测试（替换、删除、symlink、父目录替换）已验证该契约。可 grant 的 Git remote 命令（`fetch` / `ls-remote` / `push`）执行的是冻结目标。

admission 解析出的 `ConfiguredRemote` 绑定（repository identity、remote name、canonical URL、transport、capability）在执行层被还原为 `ApprovedGitTarget`；执行时对已验证的 git 目录只读取一次 config 字节并验证（无 effect-critical key、解析出的 remote URL 与获批一致），中和仓库自带的 credential helper，随后对私有 mirror git 目录运行 Git。冻结 config、空白 global/system config、禁用 hooks、显式 pin remote URL、清空 `GIT_*` 环境覆盖。

此后 Git 重读 `.git/config`、global/system config 或环境都不能改变获批目标；无法从冻结字节复现目标时 fail closed。真实 HTTP Git probe 证明在批准后修改 remote URL、pushurl、`insteadOf`、credential helper、SSH override 或替换 repository 时，服务端只会看到获批 URL，或根本没有请求。生产 admission 现在接受执行层可绑定的文件／仓库／remote 绑定（Unix），拒绝无法绑定的组合。Windows 的文件与 Git 复用仍为 UNSUPPORTED / fail-closed。以下类型与持久语义描述框架。


`ToolHost` 是资源授权的执行 owner。从真实工具输入和宿主资源解析器生成 `GrantRequest`，冻结 capability 与 resource 的精确绑定，并在真实副作用发生处约束该身份（存在已证明执行绑定的消费者），使获批资源与实际作用资源一致。客户端只展示这份请求并返回选择；描述文本、模型参数中的身份声明和 UI 状态都不能生成 authority。`ResourceGrantStore` 负责授权记录的校验、匹配与持久化，生产实现使用现有数据库。记录落库必须晚于批准事件的 durable barrier；存储或解析失败要求重新批准，不默认放行。

Once 仅用于当前调用，不落库；Session 绑定持久会话的真实 `SessionId`，不是 turn id、agent id 或展示名称，子执行器沿用其所属会话；Project 绑定 canonical checkout 和 Git common directory 的对象 incarnation，非 Git 项目绑定 canonical 目录对象。文件授权绑定精确 canonical path 与现存对象（新目标绑定现存父目录），不是目录前缀。后台任务绑定 runtime incarnation、任务编号、不可变 owner 和进程身份；重启后的同号任务不能重放旧授权。对象、配置或 remote 身份变化后，旧记录不能替代新批准。

capability 与 resource 分开匹配。远端读写、文件读写删除、后台任务观察和控制、凭据使用和原文读写彼此不隐含授权。资源授权不产生 unrestricted authority，也不扩大 `NetworkScope`。Full 在解析与存储查询前直接进入 unrestricted 分支；Auto 的普通已允许动作不查询授权存储，仅需要批准的动作尝试精确复用。旧 session/always 规则保留原有语义，但不迁移或冒充资源授权。

无法证明真实资源或执行效果时，不提供可复用资源授权，回到现有 exact-call 审批。包括 SSH remote、未经归因的 credential helper、叫不出名字的远端、替代 PATH，以及 Git 环境或命令配置 override。可解析的 `sh -c "git …"` 与直接 Git 命令一样在 Auto 下自动运行，并解析出同一份 remote grant；该目的地已有已存凭据时仍在准入时询问。Shell 本身不是冻结的 Git 目标，批准后的脚本按宿主 helper 执行。External PID 尚无可信进程出生证明，因此不生成持久授权。Phase 3 的 `CredentialUse` / raw capability 类型由下述 Phase 4 broker 实现。`ConfiguredRemote` 的网络层地址 pin 仍属 Phase 2。冻结 Git 执行目标证明的是实际网络目的地为获批 URL，不声称 redirect 已被网络层完全隔离。

#### Phase 4 凭据隔离（Credential Broker）

本阶段验收状态为 CLOSED。 “使用凭据”与“读取凭据明文”在同一套 Phase 3 grant store、resource identity 与 scope 上是两个 capability。宿主侧 broker 在 scratch 目录中运行用户自己的 global Git credential helper 来解析目的地凭据，并过滤掉调用方提供的所有 `GIT_*` / askpass override，因此仓库自带的 helper 与调用环境都不能成为来源；broker 从不经过 agent shell。凭据值不会成为模型输入、工具结果、日志行或命令行。它被写入冻结 Git 目标私有目录内 owner-only（0600）文件，由生成的 helper 只输出给已批准的 Git 进程。grant 绑定凭据的单向 incarnation commitment（对 username 与 value 的 `sha256`）；token 轮换会改变 commitment，复用的 Session/Project grant 因此 fail closed，而不会用新凭据认证。

`credential.use` 与 `NetworkScope`、文件系统读取相互正交。获批凭据既不授予出站网络，也不授予 `~/.git-credentials` / `~/.ssh` 访问；原文凭据路径（`read_file`、`cat .git-credentials`、`security find-generic-password -w`、`secret-tool lookup`、`pass show`、`git credential fill`）需要各自的决定。Auto 与 Restricted 下，目的地存在已存凭据的 Git remote 即使 `git fetch` 本身是只读也会升级为真实决定；没有已存凭据的目的地（公共仓库）不产生 credential binding，也不弹窗。Full 完全绕过 broker。无 ASK、无 DENY、无注入。

凭据授权之所以可复用，只因为冻结 Git 目标在执行时重新解析凭据并在 incarnation 变化时拒绝，与 remote URL 相同的 execution-bound 规则。SSH agent 认证（`SSH_AUTH_SOCK`）仍为 **PARTIAL**。只使用 agent，不经 broker，私钥文件保持拒绝。SSH identity 本身的 brokering，以及 Windows/Linux 真实宿主的凭据验证，仍为 remaining。detached（background）已认证 Git 命令为 fail-closed UNSUPPORTED。脱离进程无法在任务结算时回收私有 isolation 目录，运行时因此直接拒绝，而不是把凭据文件留在磁盘上等待 OS 临时区清理。需要该效果的调用必须在前台运行；前台执行在进程结束时回收目录与凭据文件。

### 任务收尾与后台结算的精确边界

模型输出最后一段正文，只能证明本轮不再等待模型生成；它不能证明任务已经完成。

任务收口必须经过统一的生命周期边界。

```text
Running
   ↓ 最终正文结束
Finalizing
   ↓ 依赖结算、必需评审、结果裁决
Terminal（TaskFinished）
```

`Finalizing` 是领域中立的运行时状态。引擎负责记录阶段和耗时。持久阶段起点、下一阶段起点和 `TaskFinished` 时间戳足以机械计算各段耗时；纯 timing 完成记录不得增加权威终态的持久化关键路径，更不得改写结果。

`TaskFinished` 是唯一权威任务终态。客户端只能从已经持久化的 `TaskFinished` 投影完成、失败或阻塞结果；最终正文、回合结束和后台清理都不能替代它。终态事务没有提交时，客户端只能显示可恢复错误，不能合成 `Failed` 或任何其他终态。

声明、验证证据和生命周期分别表达不同事实。`Answered` 不因产生文件修改而升级为 `Completed`；`Completed` 不证明 format/build/test 通过，提交说明只能传播实际验证观测，缺少观测时标为 unknown。`Stalled` 保留其停止原因，并映射为可恢复的 `Interrupted`，不结清仍欠工作的 goal。`/develop` 自身要求 Review PASS，未通过时停在可恢复边界；这个工作流要求不改变普通 advisory review 的非阻塞性质。

权威结果一旦提交，产品必须立即发布用户可见终态。终态后只有已经绑定到不可变 task/run 身份的工作可以继续；它不得延迟终态可见性，也不得把客户端重新切回运行中。会话级清理必须在发布前固定精确资源 ID。继续执行检查点属于权威 work-window 边界，必须在 `TaskFinished` 前提交；否则终态后重新读取“当前会话”会错误吸收下一回合；检查点创建失败时，保留具体错误并将该 work window 暂停为可恢复的 `Interrupted`，保留尚欠工作的 Goal；不能谎称检查点写入成功，后续继续从原始权威事实恢复。配置为 Required 的评审仍可产生完成警告；评审发现是 advisory 的模型结论，不是机械终态 verdict，纯 advisory 评审不得阻塞。

后台进程的清理边界必须在启动时显式确定。默认的 `goal` 生命周期随创建它的目标终态回收；`runtime` 跳过目标终态清理，但仍随当前 Runtime 退出回收，保留原有契约。`session` 和 `persistent` 进程从启动时起由独立 Execution Host 托管，保留创建会话作为访问归属；`session` 在显式删除会话时停止，`persistent` 用于用户明确要求跨 Runtime 重启继续运行的服务或 watcher，在进程自行结束或用户显式停止时结束。Execution Host 异常退出的结果按独立故障域处理，不保证服务随宿主退出。Runtime 更新不停止托管服务，也不将已有本地任务自动转换为托管任务。所有生命周期继续使用统一后台任务接口观测和停止。

写入归属在执行前转换为操作系统执行边界，包含允许路径及其他执行者独占的排除路径。无法执行该限制的平台必须拒绝，不能退回全 workspace 写入。mutation 只从已执行限制的范围内结算，不使用全树 diff、过滤其他执行者变更或全树 rollback 补偿权限。后台 workload 必须整体结束后再结算 mutation，最后发布进程终态；父 shell 退出不满足这个边界。Auto / Restricted 读日志、等待与停止校验创建会话，允许同会话跨 turn 使用；跨会话操作需要批准，Full / 已批准调用通过宿主显式 unrestricted authority 执行，不伪造创建者身份。

stdout/stderr 的实时通道和行缓冲都有容量上限。消费者变慢时仍持续读取 pipe，避免子进程被堵死；丢弃字节必须显式标记 truncated，最终输出不得伪装完整。工具结果保留退出码、错误和沙箱等执行事实，不把 connection refused 解释成非代码问题，也不指示下一步必须提权或执行某个工具。

后台任务的增量观察属于进程能力层。`observe` 在同一任务锁下校验读取位置、注册变化通知并交付有界日志；新输出、非运行状态或等待区间结束都可返回。显式 cursor 彼此独立，不推进默认读取者；被截断的历史显式报告缺口，未交付内容保留下一次读取。`wait_task` 只负责参数与模型输出适配，`get_task` 读取完整保留日志。其面向模型的契约是等待终态。阻塞到子任务状态变化、调用方超时或被取消，再在一次结果中交付累积的日志增量，因此观察一个长任务只消耗一次 model round，而不是每个区间一次。日志到达不会满足仅等待进程结束的 `wait`，也不会触发提前结算。

终态结算事实在任务记录存续期间不可被读取操作消费；权限违规等失败对每个读取者保持一致。变更路径和快照的上报可以只交付一次，但交付标记不改变结算事实。

### 会话设置的版本保护

替换会话标题、模型、默认模型、权限、Thinking 或 collaboration 的命令必须携带客户端实际观察的 snapshot `last_sequence`（空日志为 0）。Rust `SNAPSHOT_VERSIONED_COMMANDS` 是唯一命令分类，schema 导出同一 wire-tag 名单，Web/Desktop 消费生成版本，TUI 直接使用该分类。存储在同一 SQLite writer transaction 中拒绝负数、超前的 observed 日志位置及该位置之后的 `SessionMetadataChanged`，只替换目标字段并追加 `SessionMetadataChanged`；回答进度等不改变设置的事件推进日志不会造成冲突。snapshot 的设置字段和版本来自同一 read transaction。冲突不修改字段或日志，客户端刷新并显示错误，不能自动改用新版本重试覆盖。已完成的相同 command identity 仍按 receipt 幂等返回。 LocalSocket/TCP 使用独立 `DeliverVersionedSettings`（wire tag `deliver_versioned_settings`），WebWS 使用 `deliver_versioned_setting`；旧 peer 不认识该操作时必须拒绝，客户端不退回 legacy deliver。新 peer 也拒绝通过 legacy deliver 修改受版本保护的设置，且新操作只接受名单中的命令与显式 observed version。

Submit/Steer/Cancel 和 request-id waiter 回复保留各自的队列、取消或 identity 契约。RunGoal/ConfirmPlanToGoal 的内部轴切换只改 collaboration 并推进日志，不能把旧配置整行写回。SetDefaultModel 的 CAS 保护当前会话模型；全局默认模型文件仍由 GlobalConfig 负责，不提供跨会话 CAS。会话已切换但全局文件保存失败必须明确报告部分结果，并保留未知 dispatch receipt，禁止盲重放。

### 回合的持久接收边界

一个新的用户回合或对话回合，只有在同一条 turn 记录已经携带版本化、可重放的 initiating user message 后，才可以持久地进入 `running`。这份预写载荷是崩溃恢复时的权威输入；`session_messages` 是供客户端展示和后续模型请求使用的有序 transcript 投影。

```text
持久的 running turn
        ⇒
持久且可重放的 initiating input
```

daemon 只有在这个边界提交后才发送 wire ACK。如果进程在正常 transcript 追加之前退出，重启恢复会在当前 ownership token 下，以 turn id 而不是消息内容作为身份键，恰好一次地补齐 initiating message，然后才结算孤儿 turn。`UserMessageAdded` 是乐观的客户端通知；它不是权威输入、持久化命令或 durability witness。

对每个交互客户端来说，ACK 契约是同一个。启动或继续 Turn 的命令，只有在对应的持久化接收边界提交后才返回成功。进程内 TUI、`leveler serve` 暴露的 daemon、该进程服务的 `/web` UI，以及连接的 WebUI 都遵守这一条；Embedded 与 Daemon 之间不再存在不同的 Turn ACK 契约。成功 ACK 只表示执行输入已被持久接收（`EngineEvent::TurnStarted` 在 `TurnStore::start_owned` 提交后发出），不表示 Turn 已完成，也不表示工具副作用 exactly-once。被拒绝的 Turn 必须返回明确错误，而不是成功 ACK。无响应或断线不能推导命令未执行。凭证仍按原语义去重，未知结果不授权自动重跑。

无附件的 `SubmitMessage` 若被既有 `parse_continuation` 识别为“继续”，由 App 命令入口统一进入已有 `ResumeTask` handler。沿原任务、Goal 和计划继续，并把补充要求追加到原目标。是否可恢复、显式取消拒绝和无可恢复任务时的普通消息语义，仍由原 Runtime owner 判定；带附件的提交保留内容与图像校验路径。客户端不各自新增“继续”解析器。

Chat 显式委托是 Turn 级的 Goal 身份，不是第四条协作轴。顶层可写的 Chat 回合可以调用注入工具 `commit_task(objective, acceptance)`；Runtime 校验声明后通过既有 `GoalBinder` 把该回合绑定到持久 Goal（goals 行、`turn.payload.goal_id`、预算 scope 迁移）。持久 Goal 的 objective 是 turn 目标锚点里的用户原话，不是模型的复述，因此模型不能通过声明一个更小目标来缩窄任务。从下一轮起，同一回合暴露 `update_goal`，使用 Goal closeout 与验证门。会话 collaboration 仍是 `chat`，之后普通消息仍是 Chat。声明是结构化的：Runtime 不按用户文本分类执行意图，也不增加分类模型调用。问答、讨论、只读和 Plan 面不暴露该工具，子 Agent 不能声明任务。恢复以 `turn.payload.goal_id` 作为充分的目标身份，因此被中断的委托 Chat 回合按 Goal profile 恢复，不改变会话轴。暂停的任务保持可见，直到完成或显式取消；显式取消逻辑任务会结清其 Goal 并拒绝之后的继续。

用户明确枚举的交付项是范围权威。持久 Goal 记录模型候选的 acceptance 和之后每条用户范围指令；`update_goal(complete)` 必须逐项记账，缺少、声明未完成或被已核对的产物/命令反驳时拒绝完成。提取范围刻意收窄：文件路径、行内代码标记、短分隔列表、编号项、具名可执行命令，以及明确的测试要求。开放式语义目标不产生机械要求，只能以诚实的未验证边界完成，不能伪造证明。只有用户自己写下的豁免才能把要求移出有效范围。

Web 重开会话时，持久保存的最后选择与本次收到的 `SessionList` 决定恢复对象；不能读取尚未由 React 提交的旧列表。恢复继续遵守 current / pending selection guard，避免覆盖用户已开始的会话选择。

#### 取消请求、送达与终止各有证据

命令 ACK 回答的是“送达”，不是“执行完成”。取消让这条区分变得关键，其语义阶段可以拆开表达。

```text
CancelRequested → CancelDelivered → ExecutionTerminated → TerminalPersisted
```

这些是语义阶段，不是新的 wire 类型。`CancelCurrentTurn` 返回成功只表示请求已经到达拥有该 Turn 的 Runtime；Turn 在终态提交前始终是 `running`。客户端从持久终态（`TaskFinished` 及对应 `RuntimeEvent`）得知真正的结束，而不是从 ACK 推断，并且在 Turn 仍存活时显示“正在停止”，而不是“已停止”。

`CancelTask` 的差别在意图，不在 ACK 强度。它要求提交终态 `cancelled`、结清 Goal 并拒绝之后的继续；而 `CancelCurrentTurn` 留下可恢复的 `interrupted`。运行中的取消请求在 ACK 前写入现有事件日志，绑定精确 task、turn、boot 和 owner epoch，并与终态提交共用持久写入事务边界。取消先提交则终态必须为 `cancelled`；已提交的完成结果不会被迟到取消覆盖。boot 在 ACK 后死亡时，运行时只回收已证明死亡的 turn，Harness 依据该 turn 的持久取消意图供给 Goal 结算，终态与会话投影和 Ownership 释放原子提交。

恢复不重放结果未知的工具调用，ACK 始终不能被表述成“执行已经终止”。 当 boot 崩溃后只有 fenced recovery 写入的最新 Interrupted turn、尚无该工作窗口的 TaskFinished 时，App 使用同一持久 SessionFacts 的 turn ordinal 与终态边界判断可恢复性，并要求有效的 user/chat initiating payload 和已释放的 Task owner。迟到的同 turn finish 不能让 Cancelled/Completed 重新可继续。Harness 继续拥有原 objective、Goal、计划与 ledger 的重建；未知结果的变更工具要求显式人工 reconciliation，不能自动重放或伪造成功。


---

### 客户端连接与两个宿主进程

`leveler-client-protocol` 定义客户端与运行时之间唯一的**业务语义协议**。
`ClientCommand`、`RuntimeEvent`、`UiSessionSnapshot` 及其 envelope。CLI、TUI、
Web 与未来桌面端在连接运行时时都使用这套语义，不为某个 Shell 另建会话、回合或工具事件协议。
协议不规定进程如何启动，也不规定使用哪种本地连接。

Runtime Host 是另一条边界。它负责让客户端找到、启动、接管或复活可连接的运行时，
并编排版本/配置兼容性检查、旧进程退场与新进程交接、daemon 认证 token 与 ready contract、
transport 绑定和空闲回收策略。它不处理 Agent loop、模型、工具、权限决策、
会话持久化或恢复。这里的 **Runtime Host** 是进程和连接的生命周期编排者，
它不拥有 §9.0 的真实副作用执行权威。

后台长期进程使用独立的 **Execution Host**，它属于 `leveler-execution` 的执行
边界，不与 Runtime Host 合并。Execution Host 从启动时起唯一拥有托管进程的
句柄、进程树、沙箱资源、输出读取器和退出监控；CodeLeveler Runtime 是它的
客户端。Runtime 更新、正常退出或崩溃不会转移这些进程的 ownership。
Execution Host 复用现有 `CommandRunner` 和后台 registry 的执行实现，不建立
第二套 spawn、日志或停止路径。模型、Agent loop、会话生命周期、Goal、
verification 和文件变更结算仍属于 CodeLeveler Runtime。

OS workload 全部退出与语义结算完成是两个事实。Runtime 在 spawn 前持久化
创建会话、writer、写入边界和 mutation baseline；Execution Host 不解释这些
结算输入，只保存进程事实。进程组完全结束后，宿主持久记录退出事实，Runtime
在线时结算，离线时由重连后的 Runtime 恢复结算。结算结果提交之前不得发布
原有后台任务语义终态，也不得提前释放对应的写入归属。并发客户端不能各自
结算一份结果；Runtime 的持久结算记录是该事实的唯一权威。

Execution Host 的进程协议以 major/minor 和能力集合判断兼容性，不要求宿主与
Runtime 的 build fingerprint 相同。不兼容时保留服务、显式报告错误。工作中的
宿主不因 Runtime 更新而被替换；宿主自身的升级不承诺迁移现有进程。
已经由旧 Runtime 持有的 legacy 任务保留旧清理和更新阻塞行为，不伪造迁移。
旧 Runtime 返回 `RetireDecision::Unsupported` 时，客户端不得发送退出、强制退休
或进程信号。它报告有运行回合或后台任务时，升级显式延期；即使健康快照显示
空闲，也必须阻断自动迁移，因为快照不能原子关闭新任务 admission。用户先让
旧 owner 正常结束工作并退出，再由现有 Runtime Host 连接入口启动新版本。
这不影响支持原子退休的现代 Runtime，也不改变 Full 的工具权限语义。
旧版本缺少原子退休能力是兼容性错误，不是权限 ASK / DENY。

Execution Host 崩溃是独立故障域。正常析构时的进程树清理不能证明 SIGKILL
后的清理。恢复必须以持久元数据和可验证的进程身份为依据；无法安全证明身份
时显示 unknown / recovery-required，保留未结算写入归属，不向可能复用的
PID 或进程组发送信号，也不将无法观测的进程标记为已退出。

第一阶段没有可证明进程出生身份的自动 adoption。持久记录仍为 Running/Killing
或存在未决 spawn intent 时，拒绝自动替换丢失的宿主，并明确要求人工恢复。
进程自行退出也不能让这一不确定状态凭猜测消失。已经持久确认的退出事实和日志
可由新宿主恢复，不会当作正在运行的进程重新启动。

当前 Execution Host 的 owner-only 状态目录安全后端支持 Unix，Windows 的安全
后端尚未实现，托管生命周期明确拒绝执行；不回退到无保护的 TCP 或宽权限 token
文件。现有 Runtime Host 的 Windows 支持及本地 `goal` / `runtime` 执行不因此改变。

这些职责有各自的最终 owner。

| 边界 | 负责什么 |
| --- | --- |
| Shell / 产品 | 参数与交互、项目选择，以及客户端持有的进程生命周期策略 |
| `leveler-runtime-host` | runtime discovery / spawn / adopt / compatibility / handoff / revive；协调 token、readiness、bind 和 idle 策略 |
| `leveler-local-transport` | 实际建立连接与监听、transport trust 和 endpoint ownership；Unix socket/flock 与 Windows pipe/instance guard 由平台适配层持有 |
| `leveler-client-protocol` | 客户端命令、运行时事件及快照的语义契约 |
| `leveler-app` / `leveler-engine` | 运行时内部状态、会话与回合生命周期、持久化和 crash recovery |

Host 只编排 transport 的绑定与所有权取得；它不能再实现一份 socket、`flock`
或运行时内部 ownership/recovery。客户端建立连接后，命令和事件经协议与 transport
流动；业务命令不需要先经过 Host。也因此，“单 Runtime”指共享一套运行时实现，
不表示所有项目或 embedded 模式必须共享一个操作系统 daemon 进程。

Phase 1 前有两条各自实现的启动路径。`leveler-cli` 为默认 TUI 编排可脱离终端的
项目 daemon，Web 的 `ProjectManager` 则自行 probe/spawn 并持有自己启动的子进程。
现在两条路径都调用 `leveler-runtime-host`。TUI 使用 detached 的发现、版本交接与
revive 入口；Web 使用 owned-child 的接管或启动入口。CLI 保留终端输入与文案，
Web 保留项目 registry、路由、状态和它启动的子进程的监控与终止策略。
两种进程所有权行为保持各自原有语义。
Web 页面访问用的 token 仍是 Web 产品边界，不是 daemon 认证 token。
embedded 模式仍使用同一套 `leveler-app` 运行时实现，不要求另起 daemon。

交互客户端创建会话使用 `create_session_identified`（Web 为 `/api/sessions/identified`，
Desktop 为显式 identified IPC），并在未知响应重试中复用同一个 `request_id`。
该 ID 复用 CommandId 的全局命名空间；不同原始请求或已有普通命令占用同一 ID
都会显式冲突。TaskCreationStore 在同一事务内写 session、task、原始请求 fingerprint
及已有 command receipt，初始 model、workspace、permission、sandbox、collaboration
由该 session row 持久化。重试返回原 session 的当前快照，不重写后续用户设置；默认
模型变化不会改变原请求身份。receipt 随 session 显式删除而删除，其去重有效期与
session 的持久生命周期一致。ApprovalPolicy 保持既有运行时 / transport 信任边界，
不被此 creation receipt 包装为新增的持久权限事实。
旧 wire 省略 request_id 仍是非幂等创建，不允许响应未知时自动重试。identified
操作在旧 peer 上必须明确失败，客户端不退回 legacy 创建；不通过忽略字段推测兼容。

客户端协议之下的 transport 使用同一套 framing、命令处理与重连逻辑。

```text
Client Protocol
    └── Transport
        ├── Unix socket：trusted-local
        ├── Windows named pipe：trusted-local
        └── TCP + bearer token：authenticated-remote
```

信任由服务端 transport 决定，客户端声明的 ClientKind 只能降低权能。TCP 即使是
loopback、token 正确，也不能授予本地 AutoApprove 或计为本地交互等待者。
PermissionProfile 保持原契约；transport gate 限制客户端授予 AutoApprove，
不重新定义执行权限。daemon owner 的全局 AutoApprove 配置仍按既有语义生效。

Unix 保留 owner-only socket 和 server lifetime flock。Windows adapter 使用拒绝
远端连接的 named pipe、protected current-user SID DACL，并双向校验 pipe peer
process 的 TokenUser SID。客户端还校验当前连接 pipe handle 的 owner SID，
把身份检查直接绑定到安全对象；Win32 安全操作由 `leveler-win-local-ipc` 封装。
这与 Unix 0600 一样是操作系统用户边界，不是应用签名认证；同用户进程和管理员
不被视为边界外的未授权用户。不同服务账户不能自动获得本地信任。

Windows pipe 名由当前 SID 和 Layout 的逻辑 socket path 哈希确定；路径使用
UTF-16 编码，不直接把 Unicode repo path 放入 pipe 名。ready contract 继续保留
`socket` 字段作为逻辑 endpoint，build/config identity 仍从 runtime_info 获取。
Unix flock 与 Windows retained first-instance handle 分别是平台唯一的 lifetime
ownership authority，均在 recovery 和 ready 发布前取得；UI 断开不释放 ownership，
runtime crash 由 OS 释放。Windows pipe 不需要文件系统 stale socket cleanup。

RuntimeHost 的 start/adopt/revive/handoff 共用生命周期，仅 transport、安全原语与
detached process 配置具有平台差异。安全失败必须显式失败，不回退 trusted TCP。
当前 Windows 实现与交叉编译证据不等于 Windows 实机验收。
Phase 3C 的 Electron 骨架位于 `apps/leveler-desktop`。Renderer 通过受限 preload IPC
连接 Main；Main 只启动 `leveler desktop-bridge` 薄 Rust 适配入口。bridge 复用上述
Runtime Host 的 detached 接入、跨来源连接和 revive，以及现有客户端协议；不复制
进程编排或业务状态。关窗只断开 bridge，不取消回合或关闭 Runtime；Runtime 保留
既有空闲回收策略。Desktop Main 与 Rust bridge 接受现有 `CancelTask` / `ResumeTask` 命令并绑定当前会话；续写内容有界，取消与恢复的生命周期判定仍由 App / Harness owner 负责。

桌面交互式终端复用同一个 Execution Host、BackgroundTaskRegistry 和
CommandRunner。PTY 的进程、输入、尺寸、原始输出和退出事实由宿主执行权威拥有；
Application 校验会话归属并冻结创建时的实际执行策略，桌面只传递有界输入、尺寸
和输出游标，不在 Electron 中启动另一套 Shell。终端与 Agent 回合独立，输入不进入
模型对话；Full 保持 unrestricted，其他模式保留实际可执行的宿主边界，不能把
打开终端扩张成无限制授权。缺少 PTY 能力、工作区或平台支持时明确拒绝。

终端创建使用稳定标识避免重连重复启动；输入结果未知时禁止自动重发。切换会话
或隐藏面板只断开展示，恢复时观察同一终端。输出使用宿主原始字节游标，留存不足
明确标记截断。Ctrl+C 写入 PTY，由终端驱动通知前台作业；关闭标签才停止所属
进程树，确认退出后释放标签。正常退出桌面关闭本客户端创建的终端，断线本身
不能伪造退出事实。旧宿主缺少所需能力时不得悄悄切换为本地进程。

### 无工作区与全局历史的精确契约

Session、TaskEngine 和 Runtime 只有一套实现。Session 的 `repository` 是持久的 optional primary workspace。已有项目为 `Some(path)`，No Workspace 为 `None`。Application state 始终存在；执行工作目录由执行能力和授权决定。三者不能通过 HOME、启动 cwd、上次项目或内部 scratch 路径互相替代。

`Layout::no_workspace` 使用独立的 `state/no-workspace` source、socket identity 和 app-owned 配置。项目 runtime 拒绝显式 None；No Workspace runtime 拒绝其他项目关联。创建请求区分省略的 `RuntimeDefault`、显式 `None` 与 `Workspace`。原 CLI/TUI/Web 项目请求仍默认绑定原项目。No Workspace 复用同一 runtime，但不加载项目配置、rules/hooks、skills、MCP、Git/LSP、文件检查点或 shell context。工具 dispatch 在执行前检查真实 workspace 资源，默认要求 workspace 的工具即使被直接调用、权限为 FullAccess 或传入 HOME 绝对路径也必须拒绝。Web/search/browser 的独立能力遵守 §9.0 的 Permission Mode 契约。

`leveler-app` 的 Global Task Index 通过 SQLite read-only 连接扫描已有 project stores 和 No Workspace store。它聚合持久事实，不创建数据库、不运行 migration、不启动 daemon、不维护第二份任务账本。source 错误随 partial result 返回；checkout 缺失保留历史并标记 workspace unavailable。打开跨来源 Session 时先解析 index 中的真实 owner，再由 Runtime Host 连接该 source；Web 的在线多项目 Router 保留原职责。全局扫描只向可信本地客户端开放，项目 Remote/Web 授权不能自动获得其他来源历史。

`session_projection` 从持久 turn/event 顺序、boot 存活证据和实际 live waiter 派生 `task_status` 与 `task_terminal`，供本地列表、snapshot 和全局索引共享。原 `status` 保留 Session lifecycle 语义。唯一权威 terminal 是已提交 `TaskFinished`；Answered 表示回答，stop=Completed 是领域完成声明，均不代表独立验收。旧 TaskFinished 的 outcome=Completed、stop=None 保留原终态兼容，不补造领域声明。已提交 terminal 优先于尚未清理的旧 live lease；其后新受理 turn 的 durable identity/ordinal 和事件顺序阻止旧 terminal 覆盖新活动。没有 terminal 的最终正文或工具成功不能显示 completed。只读全局索引能够证明 owner 正在运行，却不能凭持久审批记录证明 waiter 存活；可操作 WaitingUser 从连接后的 owner snapshot 获得。

断线后 snapshot 恢复持久正文、plan、终态和仍存活的 approval/clarification waiter。答复、超时、取消或 runtime restart 后的旧 interaction 不能重新操作；取消期间 future 被丢弃也必须清除 waiter。runtime 重启读取持久事实，旧 provider 流不自动续跑，旧 turn 按 owner epoch 表达 interrupted/unknown。Desktop 与其他交互客户端一样使用同一个 durable ACK 路径。in-process embedded 与 daemon 不再有不同 Turn ACK 契约，in-process ACK 也是 durable admission。ACK 丢失查询相同 command identity，未知结果不授权自动重跑。

`optional_workspace` capability 在本地连接入口协商。旧客户端连接项目 source 时 `repository` 仍为 JSON string；支持该 capability 的新客户端才能进入 No Workspace source 并接收 null。JSON schema 与 TS DTO 通过既有生成流程同步；Mobile Dart 使用手写 JSON map 消费者，没有单独生成器。nullable repository migration 仅由 source owner 写连接执行，保留旧关联、外键、索引、trigger 和原任务事实；全局只读查询不迁移来源。

### TUI 绘制与性能约束

用户展示规则见第 3 节。其实现还须保持以下可验证性质。

- streaming delta 只累积语义增量，投影按 painted frame 节奏重建，不能每个 delta 重建一次。
- syntect 高亮使用有界 LRU（512 项 / 4 MiB），key 为 `(lang, 代码内容)`，与主题无关；主题渲染时映射颜色，O(1) 淘汰。
- 不可变 finalized transcript item 复用 wrap 结果，内容变化才重算；读取 live 状态的 ToolGroup、SubAgent、UserShell 不进入 memo。仅高度变化的 resize 也使投影失效。
- 完整 Shell 输出页的每帧成本由可见窗口、行索引与有界预取决定，不随捕获字节数线性增长；Conversation 永不物化完整结果。
- Workspace Diff 的 Git 进程数为 O(1)，tracked 用一次 `git diff --numstat HEAD` 和一次多文件 `git diff HEAD`，untracked 在 Rust 内生成新文件 patch。
- 编辑成功提交后生成的完整 diff 不按字节预算丢弃。Markdown 长行按终端宽度换行，缩放与历史重放后开头、中间、末尾及邻接消息仍可达。
- 绘制、鼠标和选择映射共用 Conversation 几何规则。Ctrl+G 复用同一 memoized 投影。
- 工具组与失败条目展开状态仅属于 TUI，不进入 Runtime、事件或持久化；不能将 running、cancelling、cancelled、unknown 显示成完成。
- 审批和澄清浮层不遮住它们要判断的会话：浮层拥有 Enter、Esc 和方向键；翻页键，以及落在会话视口上的滚轮，继续滚动上方会话。滚轮落在浮层上则不滚动会话。澄清的单选问题即使模型没有要求也提供自由文本答案（这是对"客户端只呈现运行时事实"的显式例外：选项列表不是封闭集合，回传的仍是用户自己输入的字符串）。Agent roster 显示每个子 Agent 的角色（探索 / 执行 / 审查），不显示它收到的 task brief。
- 错误来源使用结构化事实，`LocalConfiguration`、`Auth` 的 `error.code` 和 `ConnectionRefused` 的 `io::ErrorKind` 不用 `message.contains(...)` 特判。
- Footer 百分比、分母、条长与颜色使用同一 projected input / 完整窗口口径，不重复“上下文”标签。详情网格和分项剩余量按当前展示口径派生，快照 `free_tokens` 仍表示模型窗口剩余量，缓存使用 provider usage 口径。
- narration 去重不删除 reasoning，已流式发布的 reasoning 不重复发布。分组是 view-time fold，不改原始工具和 reasoning 记录，也不改生命周期、权限或副作用 owner。
- tmux 即使无 SSH 环境变量也不走原生剪贴板，因为服务器环境不能证明 attached client 在本机。DCS passthrough 支持未知，终端诊断仅报告观察和未知能力。
- `leveler-tui` 的 `perf_bench` 是 deterministic 性能回归基线，`ux_experiment` 是 80×24 UX fixture，两者使用 `--ignored` 手动运行。

### 后续演进与当前范围

父子智能体、角色协作和声明式 Agent 已存在。未来增加评审、研究等独立领域，应复用同一运行基础。工作区、执行、浏览器、搜索、记忆、版本控制、代码理解、媒体和外部服务可以继续成为多领域共享能力。

长期方向包括远程主机、容器、隔离虚拟机与云端 Worker，以及长目标、暂停继续、后台执行、跨设备续接、持久子会话和长期上下文。这里记录演进方向，不能据此宣称未交付产品或平台已可用。变化发生在相应职责层，能力协商仍由明确的模型、宿主、运行时与领域能力共同决定，不使用隐藏 fallback。
