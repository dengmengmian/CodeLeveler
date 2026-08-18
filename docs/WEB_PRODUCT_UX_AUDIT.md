# Web Product UX Audit

**Date:** 2026-08-18  
**Scope:** WebUI product projection only. Runtime, harness, Long Goal, Multi-Agent
runtime, and protocol are out of scope.

Web and TUI share session/turn/diff/approval semantics. Layout is browser-first
(three-pane IDE), not a TUI clone.

## 1. Current information architecture

```
┌ Rail 260px ────────┬ Stage 1fr ─────────────────┬ Inspector 340px ─┐
│ Brand              │ Header                     │ 任务 | 验证 |    │
│ + 新对话           │  project · branch · title  │ 历史 | 记忆     │
│ 会话|文件|搜索|Git │  对话|改动  RunStatus CTX  │                  │
│ [project groups]   │  Theme  Settings           │ 任务             │
│   sessions         │────────────────────────────│  状态            │
│                    │ Timeline / Hero / DiffView │  子 Agent        │
│                    │   messages                 │  执行计划        │
│                    │   AgentRunBlock            │  改动摘要        │
│                    │   ApprovalCard(s)          │  待确认（无按钮）│
│                    │   ClarificationCard(s)     │  工具统计        │
│                    │ Composer                   │                  │
│ Daemon status      │  附件 @ 权限 工作档 协作档 │                  │
│                    │  模型·reasoning  发送      │                  │
└────────────────────┴────────────────────────────┴──────────────────┘
```

`< 1240px` the Inspector is `display: none` with no drawer. Approvals then
exist only in the Timeline.

## 2. Main path: one coding task

1. Rail → 新对话 / pick session  
2. Composer: type goal, send  
3. Timeline + AgentRunBlock: what the agent is doing  
4. Inspector: plan / agents / change count (same weight as tools)  
5. If approval: Timeline card with four buttons; Inspector only lists text  
6. Terminal: AgentRunBlock shows Turn Truth; Inspector still “空闲”  
7. User opens 改动: every file’s patch is stacked in one scroll  

The five product questions are *answerable*, but not in one glance.

## 3. Duplication

| Fact | Surfaces today |
| --- | --- |
| Run / activity | Header `RunStatus` **and** AgentRunBlock |
| Pending approval | Timeline (full buttons) **and** Inspector (text only) |
| Tool list | AgentRunBlock expand **and** Inspector 工具调用 |
| Diff | DiffView (full) **and** ToolCallRow expand **and** Inspector count |
| Theme / settings | Header always-on |

## 4. Wrong weight

- Inspector Task tab is a static stack: plan = changes = pending = tools.  
- Waiting for the user is *not* the top of the right pane.  
- Terminal outcome lives in AgentRunBlock; Inspector says「空闲」.  
- Composer shows four equal chips before the send button.  
- Rail treats Sessions / Files / Search / Git as peers.  
- Header packs project, title, chat/diff, run, context, theme, settings.  
- DiffView dumps every file. Large tasks are unreadable.  
- Tab「历史」is checkpoints, not history.

## 5. Highest cognitive load

1. DiffView (all patches open).  
2. Composer configuration strip.  
3. Inspector’s equal-weight sections.  
4. Header chrome.  
5. Dual approval affordances with different completeness.

## 6. This-round change list

**P0**

1. Inspector is state-driven: waiting → running → terminal → idle.  
2. Rename 历史 → 检查点.  
3. Diff becomes a file-nav + single-file review workspace.

**P1**

4. Header: task left, run status high-weight, theme/settings in ⋯.  
5. Rail: sessions primary; Files/Search/Git secondary.  
6. Composer: one Run Configuration summary + popover.  
7. Approval/clarification: Inspector is the action center; Timeline is history.  
8. Stop repeating full tool lists and full diffs.

**P2**

9. Responsive drawers (do not hide Inspector with no access).  
10. Keyboard: ⌘K / ⌘⇧D / ⌘⇧T / ⌘B / ⌘I.  
11. Accessibility: real buttons, focus-visible, Escape, aria-label.

## 7. Non-goals

- Runtime / harness / Long Goal / Multi-Agent runtime / protocol.  
- New Web state machine or Web-only protocol.  
- Dashboard, agent admin, metrics.  
- GitHub-style review comments, side-by-side diff, suggestions.  
- Full mobile app.  
- Brand / theme redesign.  
- Redux/Zustand, App rewrite, Capability/Extension/Shell.
