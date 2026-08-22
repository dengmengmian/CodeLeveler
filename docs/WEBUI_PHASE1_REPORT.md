# WebUI Phase 1 — Desktop Shell Migration

**Date:** 2026-08-19  
**Scope:** Application Shell only. No Observability, no protocol, no EventLog,
no QueryObservability, no new backend API.

---

## 1. Files changed

| File | Change |
| --- | --- |
| `web/src/App.tsx` | Compose AppRail + Sidebar; Header without task/tabs; Workspace tabs |
| `web/src/components/AppRail.tsx` | **new** 48px first-level nav |
| `web/src/components/ExecutionView.tsx` | **new** Execution slot placeholder |
| `web/src/components/Rail.tsx` | Context sidebar (was mixed 260px rail) |
| `web/src/components/Appearance.tsx` | Export `AppearancePanel` for Settings sidebar |
| `web/src/state/store.tsx` | `StageView.execution`, `railNav`, `workspaceSection` |
| `web/src/state/store.test.ts` | Shell nav / placeholder stage |
| `web/src/lib/sessionDay.ts` | Today / Yesterday / Earlier buckets |
| `web/src/lib/sessionDay.test.ts` | Bucket tests |
| `web/src/app.css` | 48px + 220px grid, rail/sidebar/workspace tabs |
| `docs/WEBUI_PHASE1_REPORT.md` | this report |

Untouched: `controller.ts`, `ws.ts`, protocol, `query_observability`, Timeline
message rendering, DiffView internals, Composer send path.

---

## 2. Layout before / after

**Before**

```
[ 260px Rail: brand + lists + Sessions/Files/Search/Git tabs ]
[ Header: ☰  project  branch  TASK TITLE  |对话|改动|  status  ⋯ ]
[ Timeline | Diff ]
[ Composer ]
[ Inspector ]
```

**After**

```
┌ 48px AppRail ┬ Context Sidebar ┬ Workspace ──────────────┬ Inspector ┐
│ Sessions     │ + New Task      │ Header: project branch  │  existing │
│ Workspace    │ Today / …       │         status ⋯        │           │
│ Search       │                 │ Conversation|Changes|   │           │
│ Changes      │ Files / Git / … │ Execution               │           │
│ Activity*    │                 │ (surface)               │           │
│ Settings     │                 │ Composer                │           │
└──────────────┴─────────────────┴─────────────────────────┴───────────┘
```

`*` Activity / Execution are **UI placeholders**. They do not call
QueryObservability.

---

## 3. Kept capabilities

- Conversation, streaming, tool rows (`Timeline` / `AgentRunBlock`)
- Diff workspace (`DiffView`, `focus_diff` still opens Changes)
- Composer, Run Configuration, slash commands
- Inspector waiting / running / terminal (including Header waiting cue)
- Files tree, content search, git status (same REST)
- Multi-project session groups, rename / archive / fork
- Theme / Appearance (sidebar Settings + Header ⋯)
- Keyboard: ⌘B sidebar, ⌘I inspector, ⌘⇧D Changes, y/s/a/n approvals

---

## 4. Known issues

- Execution / Activity are empty slots until Phase 2.
- Workspace → Symbols has no API (intentionally; no new backend).
- Session list still grouped by **project** (multi-daemon product), then
  Today / Yesterday / Earlier inside each project.
- Nested `span role=button` in project/session ⋯ menus is unchanged.
- Icon rail uses compact glyphs, not a separate icon font.
- Headed browser click-through was not run in this environment
  (`127.0.0.1` attach previously blocked). Automated: `npm test` 67,
  `npm run typecheck`, `npm run build`.

---

## 5. Phase 2 suggestion

Wire **Activity / Execution** to existing `ClientCommand::QueryObservability`
→ `RuntimeEvent::ObservabilityLoaded`. Keep live `SessionView.tools` as the
current-turn HUD. Do not group_by live tools as a session total.

Do not start Phase 2 as a standalone dashboard page.
