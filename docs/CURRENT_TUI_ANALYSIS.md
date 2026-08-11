# CURRENT_TUI_ANALYSIS

基线 `b215dc7`。TUI Optimization Phase 0 审计（改动前）。

## Strong Existing Foundations

- **Typed transcript + 单向数据流**：RuntimeEvent → reducer → AppState/TranscriptState →
  renderer；`ToolGroupBlock { calls, open, expanded }` 已是 per-group 纯 UI 态（transcript
  为 TUI 侧投影，expanded 不入 canonical log/resume）。
- **Conversation Workbench**：alternate screen、独立滚动（`conversation_scroll` +
  `auto_scroll` + unread + ▼ badge）、行级缓存（ConvKey: version/width/theme/locale/
  tools_expanded/reasoning）、鼠标选择/剪贴板/边缘滚动/URL 点击、bottom-align pad 修正。
- **Tool 呈现已有分层**：Silent/Normal/Important 分类、批量折叠（`collapsed_group_line`）、
  失败合并 ×N、Edit 合并 + compact diff、并行 header、preview/duration；Tools 全屏兜底。
- **Plan active-only sticky、/btw 浮层、Reasoning ephemeral、DiffView/CodeBlock 去黑框 +
  display-only folding、i18n ZH/EN 双表、TestBackend/E2E/visual_dump/soak 测试面齐全。**
- Busy submit = `SteerCurrentTurn`（无队列，本轮保持）。

## P0 UX Problems

1. **无 CC 式可点击 disclosure**：折叠仅当 `visible>=2 && finished && !edits`
   （activity_stream.rs:53）——单个 shell 命令完成后**永久占多行**；折叠行文案是通用
   "N 个工具 · Ctrl+O"，无语义标签、无 ▸/▾、无时长。
2. **展开只有 Ctrl+O 且只作用于 latest**（`toggle_last_tool_group`，reducer:869）；
   鼠标 hit-testing（reducer:203-300）只有 conversation/input/jump/URL/selection，
   **没有任何 disclosure 点击区**；历史组无法独立展开。

## P1 UX Problems

3. Header 3 行（空行 + identity + animated hairline）与 Status busy 双信号竞争。
4. 折叠行不带聚合时长；失败折叠行未按 §19 形态（✗ 标签 + 首条错误）呈现。
5. status 的 `↓~` streaming 估算为 heuristic，与 provider usage 有混淆风险。

## P2 UX Problems

6. 语块间距在长任务下的空行密度未系统审计；footer 零值占位待核。

## Proposed Change Surface（本轮）

- `transcript.rs`：+`toggle_tool_group_at(index)`（ToolGroup/SubAgent）。
- `activity_stream.rs`：disclosure 契约——finished 非 edit 组（visible≥**1**）collapsed 一行
  `▸ <语义标签> · dur`；expanded 前置 `▾ <语义标签>` header + 既有明细；失败折叠行
  `▸ ✗ …` + 首条错误；语义标签走 i18n（shell/read/search/parallel/mixed，单复数）。
- `workbench.rs`/`state.rs`：装配时在 `before_item` 记录 disclosure 首行 → (abs_line, item_idx)
  hit 表，与行缓存同键缓存；对 reducer 暴露查询。
- `reducer/mod.rs`：鼠标 Down 命中 disclosure 行 → toggle 该组并返回（优先于 selection/URL）。
- i18n 双表新增字段；测试（单/复数、独立 toggle、selection/URL 不冲突、resize 键失效）。

## Explicit Non-Changes

Runtime/Agent/Tool 语义、canonical transcript、resume/replay、EventLog、Steer 语义、
alternate screen、Plan 规则、Reasoning ephemeral、conversation 缓存策略、
现有 Silent 分类与 Edit 特例（compact diff 常显）、Ctrl+O（保持 latest 语义）。
