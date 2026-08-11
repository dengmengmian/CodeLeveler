# TUI Architecture Audit

基线 `eceb271`（TUI Correctness 已 PASS）。本文档是 Architecture Hardening 的
改前审计：当前 ownership 实况、变更半径、重复知识。改造原则见结尾。

## Current Ownership（实况，非注释宣称）

| Subsystem | 文件（行数） | 实际负责 |
| --- | --- | --- |
| AppState | state.rs (431) | 全 UI 单一事实源；Conversation 视口的 13 个字段**平铺**（scroll/auto_scroll/unread/last_len/rect/scroll_bottom_rect/plain/plain_width/cache/selection×4）；cache 类型直接引用 `workbench::ConvKey` |
| Reducer | reducer/mod.rs (1502) | intent→transition **加上**几何引擎：`conversation_content_width` / `conversation_viewport_height` / `mouse_to_text_pos_clamped` / `pin_conversation_at_current_viewport` / `scroll_conversation(_pinned)` / `request_jump_to_bottom` / selection edge 几何 / `ensure_conversation_plain` |
| Workbench | workbench.rs (1215) | 顶层布局 + header×3 + **Conversation viewport 渲染**（窗口切片、bottom-align pad、selection 高亮、▼ badge、发布 rect）+ **行装配/缓存**（ConvKey、conversation_lines_and_hits、build_conversation_lines_with_hits、disclosure hit 表）+ plan 面板 + toast + btw + composer/footer glue + `sync_conversation_scroll` |
| ActivityStream | activity_stream.rs (1775) | ToolGroup 语义分类（DisclosureClass）+ disclosure 资格/标签/失败摘要/时长 + **disclosure 渲染** + unit/detail 渲染 + edit/fail merge——domain adapter 与通用渲染器**未分层** |
| Transcript | transcript.rs (768) | typed blocks + tool 生命周期投影 + per-group `expanded`。无几何、无主题——边界干净 ✅ |
| Selection | selection.rs (348) | 纯文本选区（TextPos/提取/高亮）——边界干净 ✅；但 screen→content 映射与 edge 几何在 reducer |
| Render | render/mod.rs (1857) | 全屏视图（Tools/Agents/Diff）+ item_render 分发 |
| Run loop | run.rs (890) | 事件环/订阅/timers/effects/paint 调度；曾自行拼 sync 尺寸（Correctness 轮已改为调 reducer 的 authoritative helpers——但 run 仍需知道"该拿哪两个数"） |

## Duplicated Knowledge（一个事实多处各算一次）

| 事实 | 出现点 |
| --- | --- |
| `max_scroll = total − height` / live edge | workbench::render_conversation、workbench::sync_conversation_scroll、reducer::mouse_to_text_pos_clamped、reducer::pin_…、reducer::scroll_…×2、reducer::request_jump_to_bottom（7 处同一公式） |
| bottom-align pad | render_conversation（渲染）与 mouse_to_text_pos_clamped（映射）各写一遍 |
| viewport 宽/高（rect + fallback） | reducer 两个 helper；render 用 area；run 调 reducer helper |
| plain 文本缓存生命周期 | render_conversation（选区活跃时重建/否则清空）与 reducer::ensure_conversation_plain（按需重建）双向管理 |
| "展开"语义 | per-group `ToolGroupBlock.expanded`（disclosure）与全局 `AppState.tools_expanded`（Ctrl+O 镜像，另管 diff 折叠深度 48 vs 8）并存 |

正确性轮修的 "点 A 展开 B" 正是第一行重复知识的直接产物。

## Change-Radius Analysis（当前 HEAD 实测）

| 变化 | 现在要动 | 期望 owner |
| --- | --- | --- |
| A. 新增一种 finished execution block | transcript.rs + workbench 装配 + activity_stream 全链 + render/item_render | presentation adapter 一处 |
| B. 改 disclosure 标签 | activity_stream（label+i18n） | 同左（已可接受）|
| C. 改 disclosure 点击行为 | reducer::handle_mouse + workbench hit 表 | conversation interaction 一处 |
| D. 改 Conversation scroll | reducer 3 个 fn + workbench sync + run 调用点 | conversation viewport 一处 |
| E. 改 bottom alignment | workbench render + reducer 映射（两处同步改，漏一处=坐标错位） | geometry 一处 |
| F. 改 screen→content 映射 | reducer（映射+pin）+ 必须核对 workbench 渲染公式 | geometry 一处 |
| G. 新增顶层 chrome 行 | workbench 布局 + **reducer fallback 高度常数**（size−12） | workbench 布局一处 |
| H. 新增 Conversation item | transcript + workbench 装配 + item_render | transcript + 一个 presentation adapter |
| I. 加入 `!command` | 预计 submit 路由 + transcript + activity_stream 分类 + workbench + reducer——**过宽，FAIL 状态** | input 路由 + shell adapter + presentation adapter + tests |

## Legacy Mirrors

- `tools_expanded`：与 per-group expanded 语义重叠但**并非纯镜像**——它同时控制
  edit diff 折叠深度（tool_cell cap 48/8）并进 ConvKey。移除=行为变更，本轮记录
  ownership、不动（见 TUI_ARCHITECTURE.md 的处置说明）。
- `build_conversation_lines`（无 hits 版包装）已是 `#[cfg(test)]`-only。

## 改造原则（本轮执行约束）

1. 单向数据流不变：Event→Action→Reducer→AppState→Render。
2. 行为零变更：视觉/键盘/鼠标/Runtime/Transcript 语义全部不动。
3. Geometry 单一 owner 是硬性要求（防"点 A 展开 B"类 bug 的架构层解法）。
4. 不引入组件框架/EventBus/第二套 reducer；Header/Footer 保持函数模块。
5. 切片提交：A2 geometry+state → A3 interaction → A4 build/viewport → A5 disclosure
   presentation → A6 workbench/run 收尾 + TUI_ARCHITECTURE.md。每片测试先绿。
