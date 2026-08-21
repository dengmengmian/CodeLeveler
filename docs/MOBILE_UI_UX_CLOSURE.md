# CodeLeveler Mobile UI/UX Closure Plan v1.0

**Status:** TARGET product doc — not implemented  
**Scope:** Close UI/UX of existing `apps/leveler-mobile`  
**Rules:** read the code first; do not replace the app; do not flood new features; lean the data model toward the Agent Runtime  

Chinese original: [`MOBILE_UI_UX_CLOSURE.zh-CN.md`](MOBILE_UI_UX_CLOSURE.zh-CN.md)

One-liner: **Manage your AI coding agent from anywhere.** The phone is a remote workspace for the CodeLeveler Runtime, not a chat app and not a mobile IDE.

## Current state (from the tree)

Flutter client. No named routes: `main.dart` `_Root` switches Pairing → Projects → Sessions → Chat from `AppController` flags. Signed WSS to the paired host. `SessionState` is a `messages[]` transcript (`TranscriptEntry {id, role, text}`). Tool calls only update `activity`. `plan_updated`, `diff_updated`, `attachment_added`, `sub_agent_*`, `reasoning_delta` are **deliberately ignored** (comment: they deserve UI later). Approvals and clarifications already interrupt like a workstation. Theme seed `0xFF2F6FEB` already matches desktop. Chat is still iMessage-style bubbles.

Keep: pairing, allowlist, observe-only, resync, `command_id` retry, approval cards, project offline-stays-listed.

Change: the **story** the screens tell.

## Reposition

```text
Runtime (Engine · EventLog · Agent Loop)
        ├── TUI / Desktop  — production
        └── Mobile         — control
```

Task belongs to the Runtime. Closing the phone does not cancel work. Do not build an editor, a terminal, or VS Code Mobile.

## Information architecture (evolve the stack)

Keep the stack. After pairing, default to a new **Home** (cross-project Running / Waiting). Projects and Sessions stay. Relabel Sessions as **Tasks** (CURRENT session is 1:1 with a task — do not split the model). Chat becomes an **Agent Timeline**. Settings stay. Artifacts are timeline rows + a sheet in Phase 3, not a root tab in Phase 1. Chat is never a bottom-tab home.

## UI

- **Home:** Waiting actions, running tasks, recent projects. `StatusChip` + `TaskRow`.
- **Projects:** keep `ProjectsScreen`.
- **Timeline:** full-width assistant Markdown; user as a quote bar; tool steps; turn status lines; keep pinned approval/clarification cards. Drop left/right bubbles.
- **Task detail:** Phase 1 is a header on the timeline, not a new route.
- **Artifacts:** Phase 3; Phase 1 may show an honest placeholder row for `attachment_added`.

## Data model

Dual-write `TimelineItem` next to `transcript`. Do not invent a second EventLog. Project `RuntimeEvent` kinds; stop stuffing tools into a spinner.

## API

Same pairing REST + session WSS. Add `steer_current_turn` to `Commands` when Running (Phase 2). Artifact **download** later. No public HTTP agent API from the phone. No Unix socket from the phone.

## Tech

Stay on Flutter. Keep `ChangeNotifier`. Tighten theme (less bubble radius, more hairline). New widgets: `Timeline`, `ToolStep`, `StatusChip`, `TaskRow`.

## Now vs later

**Now (Phase 1 commits M1–M7):** copy (会话→任务), Home, Timeline skeleton, tool rows, turn status, theme.  
**Next (M8–M11):** thinking, plan header, Steer.  
**Later:** artifacts, Home approval inbox, sub-agent rows, push.  
**Never in this closure:** rewrite, RN, IDE, Always allow, QR (paste already exists).

Each commit must still pair, send, and approve.
