# CodeLeveler Web Desktop UI

Current product shell for the Web client, and the IA that a future
Desktop Client should inherit. Presentation only. Runtime, protocol,
EventLog, Observatory semantics, and local-transport EOF are out of
scope.

Supersedes the rail + four-tab Inspector described in
`WEB_PRODUCT_UX_CLOSURE.md` and `CURRENT_WEBUI_ANALYSIS.md`.

## Desktop Shell

```
Single Sidebar (240px)
  + Conversation / Changes / Execution
  + Optional Contextual Inspector (320px)
  + Unified Composer
```

There is no 48px AppRail. Navigation, brand, sessions, and settings live
in one sidebar. Workspace tabs are not in the header.

| Breakpoint | Layout |
| --- | --- |
| ≥ 1440px | Sidebar + Workspace + Inspector |
| 1100–1439px | Sidebar + Workspace; Inspector is a drawer |
| 900–1099px | Workspace; Sidebar and Inspector are drawers |
| < 900px | Workspace; both drawers overlay |

## Sidebar

| Entry | Role |
| --- | --- |
| New Task | Draft a session in the selected project |
| Conversations | Workspaces → projects → sessions |
| Files | File tree + git footer for the current session |
| Search | File-content search |
| Settings | Appearance (bottom of the sidebar) |

Changes and Execution are **not** sidebar destinations. They are
workspace tabs. Activity is not a surface; durable runtime inspection is
`Workspace → Execution`.

Project rows show the name. The disk path is a tooltip / context-menu
item. Online is not a permanent green badge. Session rows are title +
relative time. Running / waiting / failed are the only list status cues.

## Workspace

| Tab | Owns |
| --- | --- |
| Conversation | User ↔ Agent product interaction |
| Changes | Code review (full diff) |
| Execution | Durable Runtime Observatory |

Conversation is a turn stream, not a stack of independent bubbles:

```
TURN
├── User Prompt
├── Agent Execution
│   ├── Thinking
│   └── Tool activity
├── Assistant Result
└── Turn Footer
```

`.conv-turn` is the only spacing owner: 12px inside a turn, 32px between
turns. User prompts use `--user-accent`, not the brand green. Assistant
results stay document-style (no avatar, no card).

### Conversation Message Presentation

| Kind | Rendering |
| --- | --- |
| Normal user | plain text (never Markdown) |
| Assistant | `MessageBody` / shared `.md-body` |
| BTW | side-question card + `.md-body`. Live only: `ChatMessage.btw` from `btw_*` events. Not in the transcript, so reload cannot restore it. |
| Compaction summary | collapsed context disclosure + `.md-body`. Runtime stamps `COMPACTION_SUMMARY_PREFIX` (`对话摘要（已压缩历史）`) on the replacement user message — the same constant the TUI uses. Not a user prompt. |

Do not copy Markdown CSS per container. `.message-assistant`, `.btw-card`,
and `.compaction-summary` own chrome; `.md-body` owns typography.

### Turn Footer Contract

Wall-clock timestamps are not shown in the primary Conversation UI
(`title` on the user prompt still holds the time).

Turn Footer owns:

- Turn Truth
- duration
- terminal actions (retry / re-run)
- copy of the **assistant result only**

Live / process disclosure keeps a left rail. The footer does not.
Copy is omitted when there is no assistant result. Footer appears only
after terminal Turn Truth exists (not while streaming).

Thinking is current-turn presentation of `SessionView.reasoning` /
`reasoning_delta`. Empty → hidden. Default collapsed. Body capped at 24
lines. No protocol change, no EventLog write.

Conversation execution disclosure (`AgentRunBlock`) is the concise
product view. Execution is the durable observatory. They do not swap
payloads.

## Contextual Inspector

Priority: `waiting > running > terminal > idle` (`inspectorMode`).

Sections render only when they have content:

```
ACTION REQUIRED → TASK / RESULT → PLAN → VERIFICATION → CHANGES → AGENTS → RUNTIME → More
```

Verification is one section, not a tab plus a copy inside Task.
Checkpoints and Memory live under **More**. Runtime is a short summary
with `Open Execution`. Changes is a summary with `View changes →`.

Waiting auto-opens the Inspector. The header waiting cue also opens it.

### Action Required Contract

Approval is permission / risk (warning). Clarification is a missing-information
request (informational). They do not share the same chrome.

Clarification options are choice rows (`A` / `B` / `C` in a fixed column),
not boxed form buttons. Freeform input is a mini composer (Enter submits).
Approval keeps the existing decide-approval decisions; Deny is a text
action, not a third identical box.

## Composer

One input. Attachment, compact Run Configuration (model · work profile),
send. Enter sends, Shift+Enter newline, `/` slash commands, queue while
the turn is running. Focus uses a neutral border plus a low-alpha ring —
not a solid brand-green box. There is no fake `@` context control.

TUI may express commands as CLI strings. Web maps the same runtime
semantics to GUI interaction:

| Kind | Commands | Web interaction |
| --- | --- | --- |
| Action | `/compact` `/clear` `/cancel` | existing ClientCommand |
| Selector | `/model` `/work-mode` `/collab` `/perm` | dedicated picker |
| Entity picker | `/checkpoint` | checkpoint list, typed id |
| Input mode | `/btw` | `/btw` chip + argument (not `/btw ` in the textarea) |
| Navigation | `/diff` `/memory` | Changes tab / Inspector More |

The slash palette shows `/command` + a Chinese description. Internal
ClientCommand names (`SelectModel`, `SetProductAxes`, …) stay in code.

## Icon system

`lucide-react`, size 18 / stroke 1.75 for navigation and actions.
Turn-truth glyphs (`✓ ✗ ◇ ●`) stay as semantic status, not nav.

## Visual system

- UI 13–14px sans, conversation 15px, metadata 11–12px, code 13px mono
- Spacing scale 4 / 8 / 12 / 16 / 20 / 24 / 32 / 40
- Radius 6 / 8 / 12
- Paper: workspace white, sidebar/inspector soft gray, warm-neutral text
- Graphite: charcoal dark. Midnight: blue-black. Not two navies.
- Brand green: brand mark, primary send, selected indicator, running
- Success color is not the brand accent
- User prompt is a neutral task marker, not a blue chat bubble
- Composer focus is a neutral ring, not a brand-green glow
