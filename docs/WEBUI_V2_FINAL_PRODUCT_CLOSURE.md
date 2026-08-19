# WebUI v2 — Final Product Closure

**Date:** 2026-08-19  
**Baseline:** `a9b190c1` (Phase 4A FINAL PASS + protocol 1.6 optional `query_id`)  
**Scope:** Product information architecture: Project → Sessions. No new
Agent Runtime, no Child Session, no Tauri, no Mobile app, no Relay.

This document is the product baseline later Desktop (Tauri) and Mobile
(remote companion) consume. It records what the code does, not what a
future client might do.

---

## Product Model

```
Project          (canonical repository path)
└── Sessions
    └── Session Runtime
        ├── Conversation
        ├── Execution
        ├── Changes
        ├── Agents          (Inspector / Activity — not a workspace tab)
        ├── Findings
        ├── Verification
        └── Completion Truth
```

User vocabulary:

```
Project = which codebase I am in
Session = this thread of work with CodeLeveler
```

There is one Session. Chat / plan / goal and economy / balanced / delivery
are Composer run configuration, not navigation.

---

## Project semantics

Identity remains **canonical repository path**. Existing types are reused:

- `ProjectInfo.path`
- `ProjectManager` (add / remove / rename / restart / historical discovery)
- `RouterService` (session → owning daemon)
- `create_session_for(repo)`

`AppState.selectedProject` is that path. No `ProjectId`, `WorkspaceId`,
`WorkId`, or `RemoteProjectId`.

Selecting Project B:

- Session sidebar lists only B (`sessionsForProject`)
- `+ New Session` targets B (`draftProject` / `POST /api/sessions` with `project`)
- If the open session belonged to A, the view is left: session-scoped
  projections (observation, pending query, attachments, composer seed) are
  cleared, and the WebSocket unsubscribes so A’s `subscribe_session` stream
  cannot land on B

Offline is the daemon `status` string (`offline` / `starting` / `online`).
The UI does not invent Online.

Project menu (real APIs): Rename, copy path, Restart Runtime (non-primary),
Remove Project. There is no OS “Reveal in Finder” API; copy path stands in.
Primary project cannot be removed or restarted from this menu (it is the
in-process runtime).

---

## Session semantics

One Session. Collaboration (`chat` / `plan` / `goal`) and work profile stay
Composer run configuration.

Sidebar row: title, status cue, relative time. No model / tokens / branch /
repo / profile in the row.

Status cues map **persisted `SessionStatus`** only:

| Wire | Cue |
| --- | --- |
| `running` | Running |
| `failed` | Failed |
| `completed` | Completed |
| `blocked` | Blocked |
| `interrupted` | Interrupted |
| `incomplete` | Incomplete |
| `created` / other | (none) |

Live Waiting is overlaid only on the **open** session, from
`pending_interactions` (approval / clarification). It is not inferred from
the list row.

Actions use Runtime commands: `RenameSession`, `ForkSession`,
`ArchiveSession`, `DeleteSession`, `OpenSession`.

`+ New Session` is the only create entry. First composer send still
`POST /api/sessions` (with the selected project) then `SubmitMessage`.

Empty states:

- No project: `Open a project`
- Project selected, no sessions: `No sessions yet` + `+ New Session`
- Draft composer: `你想让 CodeLeveler 做什么？`

---

## Removed / rejected concepts

```
No Work
No Task UI container
No Chat / Coding dual entry
No Child Session
No Agent Session / Conversation Session / Coding Session
No Desktop Session / Mobile Session
No ProjectId migration
```

Clients are views of the same Session. They do not create new runtime truths.

---

## Web architecture

```
React UI
  RuntimeBridge  →  ClientCommand / RuntimeEvent (WS)
                 →  REST only for gateway-owned: projects, fs browse,
                    files, search, git, attachment bytes
Web aggregator
  RouterService  →  per-project daemon
```

The browser does not pick a daemon. Session routing stays on the router
(`session id → owning repository`). Frontend product state is Project +
Session.

Workspace tabs remain Conversation | Execution | Changes. Agents live in
Inspector / Activity. Observatory numbers remain `QueryObservability`, not
`SessionView.tools` / `SessionView.agents`.

Rail:

| Item | Why it stays |
| --- | --- |
| Sessions | Project → Sessions navigator |
| Workspace | Files / Repository / Environment (Symbols: honest empty — no API) |
| Search | Real `/search` API |
| Changes | Git file list **and** the Changes workspace (not a dead icon) |
| Activity | Session observatory |
| Settings | Appearance |

Header: Project name · branch | runtime / waiting / stop | context meter | menu.
Not a kitchen sink of model / profile / permission / repo path / session title.

---

## Desktop reuse boundary

React UI talks only through `RuntimeBridge` + REST. No second controller.

Attachment upload is **file bytes** (`FormData` → gateway →
`AddAttachmentData`), not an ambient filesystem path from the browser. A
future Tauri shell can keep the same shape (bytes or a validated path the
shell itself read).

`window.location` is used only for WS URL + token query. Not a product
state store. DOM does not own Runtime truth.

**Can React WebUI be reused in Tauri?** YES.  
**Requires new Agent Runtime?** NO.  
**Requires new Session model?** NO.

---

## Mobile / Remote readiness

Audit only. Not implemented here. Remote ≠ expose the Unix socket.

Future path remains:

```
Mobile → Authenticated Remote Transport → Runtime / Relay
      → same ClientCommand / RuntimeEvent semantics
```

| Capability | Status | Code fact |
| --- | --- | --- |
| Project list | READY | `GET /api/projects` |
| Session list | READY | `session_list` / summaries with `repository` |
| Open session | READY | `OpenSession` / `subscribe_session` |
| Conversation | READY | snapshot + events |
| Send message | READY | `SubmitMessage` |
| Steer | PARTIAL | `SteerCurrentTurn` on the wire; Web queues follow-ups instead |
| Approval | READY | `ApprovalRequested` → `ApprovalDecision` (Inspector). Remote policy allows once/session/deny; denies `approve_always` |
| Clarification | READY | `ClarificationRequested` → `AnswerClarification` |
| Changes | READY | `UiDiff` / Diff workspace |
| Execution status | PARTIAL | Live HUD from session events is reusable; `QueryObservability` is denied on remote policy (`runtime observatory is local-only`) |
| File upload | PARTIAL | Web: bytes → `AddAttachmentData` (no ambient path). Remote policy denies `AddAttachment` / `AddAttachmentData` in favor of the dedicated upload RPC in `leveler-remote-agent` |
| Authentication | PARTIAL | Web token on the gateway; not a phone pairing UX |
| Remote transport | PARTIAL | `leveler-remote-*` exists; not a public companion channel |
| Push notification | MISSING | none |
| Session mutation (rename/archive/delete/fork) | PARTIAL | Web uses canonical commands; remote policy currently denies session management |

Two clients on the same Session: `subscribe_session` is per connection;
commands route by session ownership. Observatory query ownership is
per-view (`pendingObservationQuery` + `query_id`). This is the right
shape; it is not a multi-device product yet.

---

## Final acceptance (automated)

Covered by tests:

- Project selection filters Sessions
- New Session targets selected Project
- Project switch leaves the other project’s session, observation, attachments
- Project switch unsubscribes the previous `subscribe_session` websocket
- Late previous-session snapshot / event / `ObservabilityLoaded` is dropped
- Offline project status is not rewritten to online
- Removed project falls back to another listed project
- Session actions stay `ClientCommand` on the session id
- Phase 4A: AgentId, query ownership, optional `query_id`, CompletionTruth,
  live-vs-durable agents

Headed browser click-through of Flows A–I was not run in this environment.
Routing and isolation are locked by unit tests; visual layout at 1440 / 1280
/ 1024 is CSS (`Inspector` collapses ≤1279, sidebar ≤899, rail stays).

---

## Known gaps

- No OS Reveal/Open of a project folder
- Symbols index: no API (Workspace subsection is an honest empty)
- Files / Search / Git REST is session-scoped (`/api/sessions/{id}/…`);
  a Project with no open Session cannot browse the tree yet
- Steer is protocol-ready, not a first-class Web control
- `UiSessionSummary.repository` omitted on old runtimes: those rows cannot
  be attributed to a Project and are hidden from the filtered list
- Push / pairing / Relay / Cloud: not this phase
- Remote observatory, remote session mutation, remote `approve_always`:
  policy-denied; Mobile companion will need an explicit product decision
