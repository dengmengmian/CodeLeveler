# TUI Architecture

**CURRENT** TUI ownership 契约（geometry 单一 owner 已在 `eceb271` 之后落地，
基线见 `main@04a015b`）。改 TUI 前先读本文的 "Where should a future change go?"。

改前审计（历史，描述 hardening **之前** 的 ownership）见
`TUI_ARCHITECTURE_AUDIT.md`。全栈架构与 CURRENT/TARGET/DEBT 分界见
`ARCHITECTURE.md` / `ARCHITECTURE.zh-CN.md`。

## 数据流（不变的骨架）

```
Terminal Event / Runtime Event
        → Action
        → Reducer（意图 → 状态迁移 / Effect；不算几何）
        → AppState（唯一事实源）
        → conversation / presentation / components
        → Ratatui
```

## Ownership

| Owner | 拥有 |
| --- | --- |
| **AppState**（state.rs） | domain/application 状态：transcript、plan、runtime status、composer、overlay、tokens。Conversation 视图态收敛在 `state.conv` |
| **conversation::view** | `ConversationView`：scroll/auto-follow/unread/rect/badge rect/selection 几何字段/plain 缓存/行缓存（ConvKey + hits）。纯呈现态，永不入 EventLog/persistence/模型上下文 |
| **conversation::geometry** | **所有**布局事实的唯一实现：content width、viewport height（rect 权威 + 首帧 fallback）、max_scroll、live edge、bottom-align padding、screen→content 映射。renderer 与 reducer 都只调用这里 |
| **conversation::build** | transcript(+reasoning) → 折行 Lines + disclosure hit 行，同一 ConvKey 缓存条目（hit 永不相对画面过期——不许拆缓存） |
| **conversation::viewport** | 可见窗口绘制、rect 发布、▼ badge。只画，不算 |
| **conversation::interaction** | `Hit{Disclosure/Url/Text/Outside}` hit_test（disclosure>URL 优先级在此编码）、滚动操作（scroll_by/pinned/jump）、pin_at_current_viewport（先固化再关 auto-follow 的顺序约束）、selection edge 几何、plain 缓存重建 |
| **presentation::disclosure** | `▸/▾` 行的通用渲染，输入是 `DisclosurePresentation`（label/failed/suffix/duration/first_error）。**不认识任何 tool name** |
| **activity_stream** | Agent ToolGroup 的 presentation **adapter**：DisclosureClass（由 ToolKind 派生）、资格谓词、语义标签、失败自述、时长权威性判断 + 组内 unit/detail/edit-merge 渲染 |
| **transcript** | typed blocks、tool 生命周期投影、per-group `expanded`。无几何/主题/终端宽度 |
| **workbench** | 顶层布局槽位 + 组件组合（header/plan/toast/btw/input/footer glue）。不再拥有 Conversation 内部算法 |
| **reducer** | 意图→迁移/Effect。鼠标路径 = pin → ensure_plain → `hit_test` → 按语义施策 |
| **run** | 事件环/订阅/timers/effects/绘制调度；滚动同步调 `conversation::sync_scroll(state)`（不自己拼尺寸） |
| **selection.rs** | 纯文本选区（TextPos/提取/高亮），无屏幕坐标知识 |

## 硬性规则

1. **Geometry single-owner**：任何模块需要布局事实（宽/高/scroll/pad/映射）
   一律调 `conversation::geometry`。再写一份 `size - N` 公式 = 架构回归
   （历史教训："点 A 展开 B" 就是 renderer 与 reducer 各算一套 scroll）。
2. **hits 与 lines 同缓存条目**：不建立第二套鼠标缓存。
3. **呈现态不入 canonical**：expanded/scroll/selection/hit 不进 EventLog、
   snapshot persistence、模型上下文。
4. TUI 只经 `leveler_client_protocol` 接 runtime。

## 已知的有意保留

- `AppState.tools_expanded`：与 per-group `expanded` 语义重叠但非纯镜像——
  Ctrl+O 镜像最新组的同时控制 edit diff 折叠深度（tool_cell cap 48/8），且进
  ConvKey。移除=行为变更，留待未来单独评估（届时先写行为等价测试）。
- `geometry` 的 size 估算 fallback：仅首帧 rect 未发布时生效，此刻无可点内容。

## Where should a future change go?

| 变化 | 去处 |
| --- | --- |
| 改 disclosure 外观（glyph/颜色/排布） | `presentation/disclosure.rs` |
| 改某类工具的语义标签 | `activity_stream`（adapter）+ i18n 表 |
| 新增 Tool / MCP Tool 的呈现 | 通常零改动（taxonomy 派生 + generic 兜底）；要专属标签才动 adapter |
| 改滚动/auto-follow/jump 行为 | `conversation/interaction.rs` |
| 改 hit-testing / 点击优先级 | `conversation/interaction.rs`（hit_test） |
| 改 bottom alignment / 映射公式 | `conversation/geometry.rs`（一处） |
| 新增 Conversation item 类型 | `transcript`（block）+ `render::item_render` 分发 + 所属 presentation adapter；不动 geometry/hit/workbench |
| **加入 `!command`（User Shell）** | input 路由（submit）+ user-shell 执行态（新 transcript block 或复用）+ **新 presentation adapter → 复用 `presentation::disclosure`** + 测试。不动 ToolGroup 分类、tool taxonomy、geometry、workbench、鼠标 reducer |
| 改 Plan chrome | `workbench`（plan 面板函数）+ `plan_cell` |
| 新增全屏 Screen | `screen.rs` + `render/`，不动 conversation |
| 改顶层布局行数 | `workbench::render_workbench` 槽位；geometry 自动跟随 rect |
