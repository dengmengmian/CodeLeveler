# WEB PRODUCT UX CLOSURE FINAL REPORT

**Date:** 2026-08-18  
**Verdict:** **READY**

Product projection only. No Runtime / Harness / Long Goal / Multi-Agent
runtime / protocol change.

## 1. Before

Web was a complete runtime client: four equal Rail tabs, a header packed
with chrome, four Composer chips, Inspector sections of equal weight,
approvals with buttons in the Timeline, and Diff as a stack of every
patch. Users could finish a task; they could not see *what matters now*
in three seconds.

## 2. Information architecture

```
Sessions (primary)     Conversation / Changes      Task / Verify / Checkpoints / Memory
Workspace tools        AgentRunBlock (now)         state-driven priority
Files · Search · Git   Approval = history record   Action center when waiting
```

Five questions map to:

| Question | Surface |
| --- | --- |
| What is the task? | Header identity + Inspector title |
| What is the agent doing? | Header run status + AgentRunBlock |
| Do I need to act? | Inspector action block (waiting) |
| What changed? | Changes workspace |
| How was it verified? | Inspector Verification / 验证 tab |

## 3. Inspector

Modes: `waiting` > `running` > `terminal` > `idle` (`lib/inspectorModel.ts`).

- Waiting: approval / clarification **actions** at the top.  
- Running: activity, elapsed, current step, agents, change jump. Tools in
  a collapsed「更多」.  
- Terminal: Turn Truth glyph/tone (unverified is never green success).  
- Tab「历史」→「检查点」.

## 4. Diff workspace

File list + one focused file. Previous / Next / Refresh. Old/new line
numbers, sticky header, hunk/meta kinds, large-patch collapse. Tool-row
file names jump to Changes (`focus_diff`). No second router.

## 5. Header

Left: rail toggle + repo / branch / task.  
Center: conversation/changes + **one** high-weight run status.  
Right: context meter + ⋯ (Appearance / Settings) + inspector toggle.

## 6. Rail

Sessions stay the main body. Files / Search / Git sit in a quieter
Workspace row. `span role=button` inside `proj-head` left as-is (cannot
nest `<button>` without a larger restructure).

## 7. Composer

Default: attachment + one Run Configuration summary + send.  
Click opens a single popover: Model, Reasoning (read-only), Work
Profile, Collaboration, Permission. Slash commands still work.

## 8. Approval / Clarification

Timeline = record (“在任务面板处理”).  
Inspector = only full action buttons.  
Waiting auto-opens Inspector.

## 9. Responsive

- ≥1280: three columns.  
- 900–1279: Inspector drawer.  
- <900: Rail + Inspector drawers + scrim.

## 10. Accessibility

`focus-visible` on controls. Escape closes ⋯ / run popover / settings.
ARIA on inspector tabs, action regions, more menu. Terminal tone is not
color-only (glyph + label). Keyboard: ⌘K composer, ⌘⇧D changes, ⌘⇧T
inspector, ⌘B rail, ⌘I inspector. Existing y/s/a/n + Esc kept.

## 11. Tests

`npm test` — 54 pass (was 35-ish + 19 new).  
Coverage: inspector modes/tones/plan progress; diff next/prev/line
numbers/collapse; run-config summary; store `focus_diff` / toggle
inspector.  
`npm run typecheck` · `npm run build` pass.

## 12. Browser smoke

See `docs/WEB_PRODUCT_UX_SMOKE.md`. Mock contract smoke **SMOKE OK**.
Headed daemon click-through not executed (localhost browser tool
unavailable).

## 13. Remaining debt

- Keyboard Shortcuts menu item is a stub.  
- Nested `span role=button` in project/session ⋯ menus.  
- No side-by-side diff / review comments (explicit non-goal).  
- Headed daemon walkthrough still due on a real display.  
- Inspector default-open on desktop can still feel busy on first load.

## Verdict

```
READY
```

The WebUI now projects the existing runtime as a coding-agent client:
task state first, user action first, changes and verification above
tool logs. It is not a new product surface and not a Beta release.
