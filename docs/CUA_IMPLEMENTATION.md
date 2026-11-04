# CUA / Native Actions / Scheduled Automation

This document records the evidence-backed audit and the implementation contract
for CodeLeveler's native desktop control, local native actions, and scheduled
automation. It is updated at every phase gate.

## Phase 0 — audit (2026-11-04)

Baseline: local `HEAD` == `origin/main` == `99880bab8a53fea5b28cc5004d43c721b6e2df1e`,
clean worktree, `aarch64-apple-darwin`, macOS 26.5.

### Answers

1. **Computer Use owner.** A capability crate `leveler-cua` owns platform
   backends, observation generations, element references and screenshots.
   `leveler-tools` owns the two thin adapters (`computer_observe`,
   `computer_act`); `leveler-app` composes the handle through `Capabilities`.
   The agent loop, session, goal and approval owners are unchanged.
   Evidence: `crates/leveler-tools/src/registry.rs:571` (`model_surface` is the
   single surface composer), `crates/leveler-app/src/lib.rs:1226`
   (`compose_tool_surface`), `crates/leveler-browser` (same shape).

2. **Native helper process.** None in Phase 1. Calls run on one process-lived
   worker thread inside the runtime host daemon. A separate helper would be a
   second lifecycle and a second permission subject; there is no crash-isolation
   need yet. The crate API can gain a helper later without changing the tools.

3. **Permission / approval.** Reused end to end:
   `Tool::risk` + `Tool::approval_reason` → `ToolHost::admit` →
   `ApprovalPolicy::evaluate` (`crates/leveler-agent/src/executor/host.rs:205`,
   `crates/leveler-execution/src/approval.rs:970`).
   `computer_observe` is `Safe` and observe-class; `computer_act` is
   `Destructive`: Full runs without asking, Auto asks at admission, Restricted
   refuses through `PermissionProfile::permits`
   (`crates/leveler-execution/src/risk.rs:111`,
   `crates/leveler-agent/src/executor/host.rs:594`). No second approval system.

4. **Screenshot → vision.** `ToolOutput.metadata.image {media_type,data}` is
   already consumed by `extract_image` (`crates/leveler-agent/src/executor/dispatch.rs`)
   and turned into `ContentPart::Image`. Screenshots reuse that channel and are
   bounded/EXIF-stripped by `leveler_media::process_image`.

5. **Schedule definitions.** User-global, under the Leveler home, not inside a
   project. A definition can run before any project is chosen and must be
   visible across projects. Run records also live globally and reference the
   project database by id.

6. **User-level scheduler.** A user-level process (launchd / Task Scheduler at
   login) reads the global schedule store, claims due triggers atomically, then
   uses `leveler_runtime_host::ensure_default_runtime` /
   `connect_global_task_runtime` (`crates/leveler-runtime-host/src/client.rs:925`,
   `:1043`) to wake or create the owning project runtime and deliver an existing
   `ClientCommand`. It never runs an agent loop itself.

7. **Daemon idle eviction.** `prepare_daemon` arms 30-minute idle eviction
   (`crates/leveler-runtime-host/src/server.rs:108`); it fires only when the
   runtime is quiescent and `LocalWaiters == 0`
   (`crates/leveler-app/src/interactive.rs:1193`). A scheduler that stays
   subscribed while a run executes keeps the daemon alive; a scheduler that
   disconnects after delivery relies on runtime-owned work being non-quiescent,
   then lets eviction proceed. Phase 3 must observe terminal state, not just
   delivery.

8. **Identity.** Schedule Definition and Schedule Run are new global facts.
   An Agent Run creates or resumes one Session and the engine creates the one
   real Task. The Run stores `session_id` / `task_id`; it does not copy turn or
   task lifecycle into a second truth.

9. **Management API.** One service layer over the global store, consumed by the
   CLI, the TUI and the Desktop JSONL bridge. Session-scoped commands remain
   `ClientCommand`; global management is also exposed there for remote/TUI
   parity, with the bridge allowed to call the service directly exactly as
   `list_tasks` does today (`crates/leveler-cli/src/desktop_bridge.rs:278`).

10. **TCC / Windows session.** macOS checks `AXIsProcessTrusted`,
    `CGPreflightScreenCaptureAccess` and `CGSessionCopyCurrentDictionary`
    (locked / off console) before acting, and returns typed
    `permission_denied` / `no_interactive_session`. Windows compares the process
    session with `WTSGetActiveConsoleSessionId`, opens the input desktop, maps
    UIPI/`E_ACCESSDENIED` to `elevation_required`, and sets per-thread
    per-monitor DPI awareness.

11. **Existing commands as native actions.** `run_command` can already execute
    `open`, `shortcuts`, `osascript`, `powershell` and COM-capable programs, but
    it has no capability discovery and no typed, deterministic action identity.
    Phase 2 adds a discovery/action surface only where that gap is real; it
    does not add a plugin framework.

12. **Necessary additions.** `leveler-cua` (second real platform backend,
    isolated unsafe), `CapabilityPacks::computer_use`,
    `Capabilities::computer_use`, `CapabilityId::ComputerUse`, two tools, and
    the Phase 3 global schedule store/service. No second agent loop, session
    engine, goal engine, approval system, background registry or model pipeline.

### Evidence matrix

| Existing capability | Source | Actual owner | Reuse | Gap | Risk | Planned change |
|---|---|---|---|---|---|---|
| Tool surface composition | `leveler-tools/src/registry.rs:571` | `ToolRegistry` | one new pack | none | low | register two tools |
| Progressive disclosure | `leveler-agent/src/capability.rs:38` | `CapabilityDisclosure` | new `CapabilityId` | none | low | id + mapping |
| Composition root | `leveler-app/src/lib.rs:1226`, `:1361` | `Application` | capability handle | none | low | construct + availability |
| Admission / approval | `leveler-agent/src/executor/host.rs:205` | `ToolHost` | risk + reason | none | medium: Destructive always asks in Auto | none |
| Vision channel | `executor/dispatch.rs` `extract_image` | executor | `metadata.image` | none | low | reuse |
| Media bounds | `leveler-media/src/lib.rs` | media | `process_image` | none | low | reuse |
| Browser ref pattern | `leveler-browser/src/*` | Browser | generation/refs model | desktop-specific OS calls | medium | new crate |
| Global task index | `leveler-app/src/global_task_index.rs:73` | app | read-only projection | schedules are separate facts | medium | Phase 3 store |
| Runtime wake | `runtime-host/src/client.rs:925`, `:1043` | runtime host | `ensure_default_runtime` | none | medium: idle eviction | observe runs |
| Daemon idle | `runtime-host/src/server.rs:108` | runtime | quiescence + waiters | scheduler must be a real client | medium | Phase 3 |
| Persistence | `leveler-storage` migrations 0001–0047 | storage | new global DB | schedule schema | high | Phase 3 |
| Desktop bridge | `leveler-cli/src/desktop_bridge.rs:272` | CLI | new methods | none | low | Phase 4 |
| TUI screens | `leveler-tui/src/screen.rs:8` | TUI | `SecondaryPage` | none | low | Phase 4 |
| OS autostart | none in repo | none | launchd / Task Scheduler | new | high | Phase 3 |

### Verdict

**GO.** No architectural blocker. The runtime, approval, session and vision
owners are reusable as-is. The first implementation is `leveler-cua` plus two
tools; scheduler work waits for Phase 3 evidence.

## Phase 1 — Native Computer Use

### Verdict

**PARTIAL.** The macOS backend is implemented and passed a live acceptance run
against Calculator on this host. The Windows backend is implemented and
type-checks for both `x86_64-pc-windows-msvc` and `x86_64-pc-windows-gnu`, but
no Windows GUI acceptance was executed here, so its runtime behavior is
**NOT_VERIFIED**. Linux remains a compile-time `Unsupported` backend.

Baseline: `99880bab8a53fea5b28cc5004d43c721b6e2df1e`, `aarch64-apple-darwin`,
macOS 26.5, 2026-11-04.

### Ownership

| Concern | Owner | Reuse |
|---|---|---|
| OS observation and input mechanics | `leveler-cua` (new crate) | platform backends behind one `Backend` trait |
| References and observation generations | `leveler-cua::state` | per-session, revalidated on every action |
| Tool schemas and approval text | `leveler-tools::tools::computer` | two thin `Tool` adapters |
| Capability admission | `leveler-agent` `CapabilityPacks` / `Capabilities` | `CapabilityId::ComputerUse` only |
| Approval | existing `ToolHost` / `ApprovalPolicy` | `Safe` observe, `Destructive` act |
| Vision | existing `ToolOutput.metadata.image` channel | no second image path |

No new agent loop, session engine, goal engine, approval system, background
task registry or model-call pipeline was added.

### macOS backend

- Accessibility (`AXUIElement`) for app/window discovery, trees, semantic
  actions, value writes and focus; CoreGraphics for window identity and
  bounds; the system `screencapture` tool for pixels.
- `AXMain` plus application activation is the working focus primitive;
  `AXFocusedWindow` is the authoritative focus signal. A raise that reports
  success but does not move focus is reported as `CalledOnly`, not `Applied`.
- One process-lived worker thread serializes all GUI calls. References are
  `e{generation}-{index}` and belong to one session; a new tree observation
  invalidates that session's previous references.
- An action revalidates role, identifier, title and path against the live UI
  before it acts; a coordinate click also rechecks the observed window bounds.

Live acceptance (`cargo test -p leveler-tools --test computer_macos --
--ignored --nocapture`) drives Calculator end to end and verifies by
re-observation, not by tool wording:

| Observation | Result |
|---|---|
| availability | interactive session, Accessibility granted, Screen Recording granted |
| window discovery | Calculator window found, `focused` reported |
| tree | element references resolve; stale references rejected |
| AXPress clear, then 7 | display re-observed as `"7"` |
| key `8` | display re-observed as `"78"` |
| key `escape` | display re-observed as `"0"` |
| screenshot | 460x816 PNG at scale 2.0 for a 230x408 point window |
| previous frontmost app | restored after the run |

### Windows backend

- UI Automation (`CUIAutomation8`, falling back to `CUIAutomation`) with
  connection/transaction timeouts of 4 s; the control view is the tree. Pattern
  probes produce both the advertised `actions` and the value text, so the tree
  and the dispatcher cannot disagree.
- Actions prefer semantic patterns in a fixed order: Invoke, Toggle,
  SelectionItem, then a coordinate click. Text prefers a writable Value
  pattern and verifies the read-back; keyboard injection is the fallback.
- Win32 owns window enumeration (`EnumWindows`, DWM extended frame bounds,
  cloak and tool-window filtering), focus (`SetForegroundWindow`, verified
  against `GetForegroundWindow`), input (`SendInput`), and GDI capture
  (`PrintWindow` with `PW_RENDERFULLCONTENT`, then a screen blit for on-screen
  windows, and a virtual-screen blit for the desktop).
- The worker thread sets per-monitor DPI awareness V2 and initializes COM as
  MTA. Reported coordinates and pixels are physical; `scale` is 1.0.
- Session safety: a process that is not in the active console session, or a
  locked/secure input desktop, returns `NoInteractiveSession`. A UIPI refusal
  is `ElevationRequired`; it is never bypassed. `Command` maps to Control.

Compile-only evidence:

| Command | Result |
|---|---|
| `cargo check -p leveler-cua --target x86_64-pc-windows-msvc` | PASS |
| `cargo check -p leveler-tools --target x86_64-pc-windows-gnu` | PASS |
| real Windows GUI acceptance | **NOT RUN — no Windows host; not claimed** |

### Tests

- `cargo test -p leveler-cua --lib`: 5 passed (reference generation, stale and
  malformed references, observed-bounds guard, reference/point exclusivity).
- `cargo test -p leveler-tools --lib`: 445 passed, 2 ignored.
- `cargo test -p leveler-tools --test computer_macos -- --ignored`: 1 passed
  (live desktop acceptance above).

### Risks and limits

- Windows runtime behavior (focus arbitration, UIPI, PrintWindow coverage,
  virtual desktops) is unproven until a Windows host runs the equivalent
  acceptance test.
- Screenshots can contain secrets. They travel only through the existing
  vision channel; the image is processed and bounded by `leveler_media`, and
  no screenshot is written to the schedule or session store by this phase.
- The single worker can be blocked by a provider that ignores the UIA timeout;
  the facade then reports `Timeout` with an explicitly unknown effect.
- macOS Screen Recording uses the external `screencapture` path already
  present in the backend; it is not a second input path.

### Next step

Phase 2 (Native Actions) reuses this capability boundary. No scheduler work
starts before its store and authorization owners are implemented in Phase 3.

## Phase 2 — Native Actions (2026-11-04)

### Verdict

**PARTIAL — real send is BLOCKED.** Native capability discovery and the
deterministic action/outcome contract are implemented and exercised on this
macOS host. The Apple Mail provider is installed, but macOS Automation consent
is unresolved on this host and no controlled test recipient was supplied, so no
draft or send was executed; the real-send acceptance (`NA-02`) and the
execute-and-check half of `NA-04` remain **NOT_VERIFIED**. Windows compiles but
has no real execution evidence. Nothing in this phase creates a schedule,
Cron trigger, scheduler host or second approval/execution authority.

Baseline: `99880bab8a53fea5b28cc5004d43c721b6e2df1e`, `aarch64-apple-darwin`,
macOS 26.6.2 (25G83), worktree carries the uncommitted Phase 1 and Phase 2
work.

### Source audit conclusions

- `leveler-cua` owns pixels/input. `leveler-native` (new) owns *existing app
  automation*: discovery, structured request validation, action planning and
  result interpretation. It does not own a model loop, approval, session,
  schedule or process lifecycle.
- `leveler-tools::tools::native` holds three thin adapters:
  `native_capabilities` (Safe), `native_verify` (Safe, read-only Sent check)
  and `native_action` (Destructive). All real side effects go through
  `leveler_execution::CommandRunner`; production code contains no
  `std::process::Command`.
- `leveler-app` injects one `NativeActions` handle through `Capabilities` and
  reports platform support through `CapabilityPacks`; the existing
  `Capability` disclosure, `ToolHost` admission, `ApprovalPolicy` and risk
  split are reused unchanged. Restricted mode hard-denies `native_action`
  while still allowing discovery and verification.
- `leveler-cli native` is a direct human path. It constructs no model provider
  and no runtime host.

### Capability discovery

Every fact is a live probe; the classifier is pure and does not infer from the
OS name.

| Fact | macOS probe |
|---|---|
| application installed | `/System/Applications/Mail.app` exists |
| automation permission | in-process `AEDeterminePermissionToAutomateTarget(ask=false)` |
| app running | `pgrep -x Mail` through `CommandRunner` |
| enabled accounts | static AppleScript count, only when permission is granted and Mail is already running |
| system tools | `osascript`, `pgrep`, `open`, `screencapture` path existence |

The state machine distinguishes `Installed`, `Available`,
`RequiresPermission`, `RequiresConfiguration`, `Unavailable`, `Unsupported` and
`Unknown`. Discovery never launches Mail and never asks for consent. On this
host it first reported `requires_permission` / `not_determined` with Mail
running; after a read-only check made tccd wait on an unresolved consent sheet,
the same probe reported `unknown` with the exact timeout reason. The default
mailto handler is explicitly reported as **not probed** on macOS; `null` is not
an absence claim.

### Action contract

| Action | External side effect | GUI-only | Unattended after preconditions | Sent state | Final delivery |
|---|---|---|---|---|---|
| `mail.draft.create` | yes | no | conditional | not confirmable | not confirmable |
| `mail.send.open_ui` | yes | yes | no | not confirmable | not confirmable |
| `mail.send.submit` | yes | no | conditional | confirmable via Sent scan | **never claimable** |
| `mail.sent.check` | no | no | conditional | — | — |

Structured input is validated before planning: one plain recipient, bounded
subject/body, no control characters in headers, unknown fields rejected. The
recipient, subject and body are written to `0600` files in a private temporary
directory; static AppleScript reads them. No user text enters the script source
or `argv`, so quotes, newlines, Unicode and AppleScript syntax cannot inject.
An exit code is never success by itself: a recognized result marker is
required, a timeout becomes `unknown`, and unknown outcomes are explicitly
non-retryable. `mail.sent.check` can report a Sent-mailbox record; it cannot
prove final delivery.

### Tests and live evidence

| Evidence | Result |
|---|---|
| `cargo test -p leveler-native` | 33 unit + 1 live discovery passed |
| `cargo test -p leveler-tools --lib native::` | 8 passed |
| `cargo test -p leveler-tools --lib registry::` | 31 passed |
| `cargo test -p leveler-agent --lib capability` | 14 passed |
| `cargo test -p leveler-agent --test prompt_surface_budget` | 4 passed |
| `cargo test -p leveler-cli --bins cli::` | 30 passed; native parse included |
| all five static AppleScript sources compiled with `osacompile` | PASS (syntax only; no app automation) |
| `cargo check --target x86_64-pc-windows-gnu -p leveler-native -p leveler-tools -p leveler-app -p leveler-cli` | PASS |
| `leveler native capabilities --json` | exit 0, live report above |
| real agent run (request-approval, network denied) | 5 rounds; called `native_capabilities`; reported the timeout truthfully; `commands_executed=0`, no files modified |

The live agent run is the `NA-04` integration evidence for discovery: the model
found the capability, enabled its pack, read the report and did not overclaim.
It is not evidence that an action executed.

A real read-only `mail.sent.check` attempt reached Mail without a model call and
returned AppleEvent timeout `-1712` after 120 s (exit 3, `status=failed`,
`sent_state=unknown`). No email was sent. That attempt also exposed a real
defect: with a consent sheet unresolved, `AEDeterminePermissionToAutomateTarget`
blocked indefinitely and discovery hung. The probe now runs on a detached
thread behind a 2 s bound with a single-flight guard; discovery returns
`unknown` with the exact reason instead of hanging, and a second probe does not
queue another thread. A regression test reproduces the stall with an injected
slow probe.

### Windows

The classic Outlook provider is detected from the `Outlook.Application` ProgID,
an Office profile and presence of Windows PowerShell; the new Outlook AppX is a
separate `windows.outlook.new` provider and is never assumed to support COM.
Actions use static PowerShell scripts plus a JSON payload, with the same
outcome model and Sent scan without a Restrict query. Cross-compilation passes.
No Windows host executed it; real mail automation is **NOT_VERIFIED** and is
not claimed. Real classic-Outlook COM capability, UIPI behavior and the Phase 1
UIA worker hang isolation still require a Windows host.

### Linux

`leveler-native` has a compile-time `Unsupported` backend. A latent missing
`unsupported.rs` declaration would have broken the Linux build and was removed;
the inline unsupported module remains. A Linux target was installed locally but
`cargo check` could not build a C dependency without a Linux cross-compiler, so
the Linux compilation itself is **not locally verified**. No Linux product
behavior was changed.

### Risks and limits

- Real draft/send/verify acceptance remains blocked by the unresolved macOS
  Automation consent and the absence of an explicitly authorized controlled
  test recipient. Clicking a TCC prompt must remain a human decision.
- A pending consent sheet leaves at most one detached probe thread in a
  long-lived host until tccd answers; discovery itself stays bounded.
- `mail.sent.check` scans the newest 200 matching records. Absence is reported
  as `not_found_in_scan`, never as proof that nothing was sent.
- Windows runtime behavior is compile-only evidence in this phase.
- Final delivery is not observable through either provider and is never
  claimed.

### Next step

Phase 3 can build schedule definitions on top of the action contract, but must
keep its own durable authorization: a one-off action approval must not become
unattended authorization. An unattended run that hits `NotDetermined` or
`Unknown` permission must record `WaitingApproval` (or fail closed), never pop
a prompt when nobody is present. Real send acceptance should run on a
controlled account/recipient after consent is resolved.

## Phase 3 — Scheduled Automation (2026-11-04)

### Verdict

**PARTIAL — the macOS schedule core is verified end to end; Windows, OS
autostart and native-action scheduling are NOT_VERIFIED.** A deterministic
shell action was scheduled, persisted, claimed exactly once, executed through
the existing runtime, observed to a terminal record, deduplicated on a second
dispatch, and recovered after a real dispatcher kill as `unknown` without a
retry. The provider server saw zero requests in every run. Both candidate
drivers were measured on this host; neither creates a second runtime.

Baseline: `99880bab8a53fea5b28cc5004d43c721b6e2df1e` plus the uncommitted
Phase 1–3 worktree, `aarch64-apple-darwin`, macOS 26.5, debug build. Windows and
real mail sending remain blocked by the same conditions recorded in Phase 2.

### Domain freeze

| Fact | Identity and dedup | Store | Relation |
|---|---|---|---|
| `ScheduleDefinition` | `id`; canonical workspace; trigger; command; permission profile; timeout; optional model; one `session_id` after first use | global `<home>/state/schedules.db` | configuration only; it executes nothing by itself |
| `ScheduleOccurrence` | `UNIQUE(definition_id, scheduled_for)` | same DB | one generated slot; one future slot at a time per definition |
| `ScheduleRun` | `UNIQUE(occurrence_id)` | same DB | one attempt; `dispatching → running → succeeded / failed / unknown`; stores `session_id` and the user-shell `execution_id`, never a task lifecycle |
| Runtime `Session` | existing `SessionId` | existing session DB | created through `CreateSessionRequest`; one per definition; reused by every run |
| Engine `Task` | none | — | no turn, no agent loop, no model call, no second task truth |

A schedule is therefore a global configuration row beside the project database,
not a second Engine. A run is projected from the runtime's own user-shell
record; the schedule DB stores only the identity and the observed outcome.

Slot generation is grid-based: a definition keeps exactly one future occurrence;
after a finished run the next slot is the first grid point strictly after now.
Missed slots are not backfilled. A `Once` definition creates exactly one slot
and never a successor.

Claiming is one SQLite transaction: `UPDATE schedule_occurrences SET
state='claimed' WHERE id=? AND state='pending'` plus the `schedule_runs` row.
The unique `(definition_id, scheduled_for)` key is the deduplication boundary.
The claim carries a 120 s lease renewed every 40 s; an expired lease changes the
occurrence to `interrupted` and the run to `unknown` with an explicit error and
does not retry. Delivery is at-most-once: a missed run is visible, a silently
duplicated one is not.

The first run resolves the definition's session with a stable
`CommandId("schedule-session-{id}")` and adopts the winner if two dispatchers
race. A definition is bound once; a different stored session is a hard error,
never a silent rebind. The action itself is the existing
`ClientCommand::RunUserShell` with `CommandId("schedule-run-{run_id}")`; the
dispatcher observes the terminal `UserShellExited` record by polling the session
snapshot, because a socket event subscription attaches asynchronously while a
fast action can finish before it is live. The snapshot merges live records with
the durable user-shell history, so a completed fast action is still observed.
The client is dropped after every claim, leaving no attached waiter, and the
daemon's existing idle eviction remains in force. The dispatcher owns no
runtime, session DB, engine, approval system or process lifecycle.

### Driver comparison

Method: debug `leveler` binary on this macOS host; short `/private/tmp` home so
the runtime socket fits `SUN_LEN`; a local probe provider pointed at a loopback
HTTP sink, whose request log stayed empty. The short-lived arm was invoked at
the due second by an external `sleep` loop (an OS timer was not installed). The
persistent arm ran `leveler schedule serve --poll-secs 30` across two 15 s
slots, with the daemon's idle timeout at 60 s to keep it warm. Absolute values
are debug-build figures; only the two arms are comparable.

| Metric | Short-lived `dispatch`, first run (cold daemon) | Short-lived `dispatch`, second run (warm daemon, reused session) | Persistent `serve`, run 1 / run 2 |
|---|---|---|---|
| driver wall time | 0.51 s | 0.34 s | resident |
| due → claim | 22 ms | 63 ms | 3 ms / 3 ms |
| due → finished | 517 ms | 390 ms | 437 ms / 336 ms |
| driver max RSS | 36.1 MB | 35.7 MB | 26.3 MB idle → 38.8 MB after runs |
| driver CPU | 0.01 s user + 0.01 s sys | 0.01 s + 0.01 s | 0.03 s cumulative over ~30 s |
| model requests | 0 | 0 | 0 |
| runtime daemons | 1 (spawned, then idle-evicted) | 1 | 1 |

With a warm daemon both drivers finish a scheduled action in well under a
second. The persistent host removes the per-slot process start (about
0.2–0.3 s in this debug build) and otherwise does not reduce execution latency;
its claim is a few milliseconds earlier because the process is already resident.
It costs one resident process at ~26 MB idle, with no measurable idle CPU. The
short-lived path leaves no process behind: with
`LEVELER_DAEMON_IDLE_TIMEOUT_SECS=5`, the daemon exited 5.1 s after the dispatch
released its client. Both arms use exactly one runtime daemon per workspace.
The persistent host is therefore the better default only when slot granularity
is fine; for ordinary intervals the short-lived driver is simpler and already
sub-second.

### Real evidence

- `cargo test -p leveler-schedule` — 6 store tests + 2 socket E2E tests passed.
  The store tests lock atomic claim-once, one-slot recurrence, no `Once`
  successor, expired-lease recovery, non-rebinding session identity and cascade
  removal. The E2E tests run a real `Application`, `LocalSocketServer`,
  `LocalSocketRuntimeClient` and `Dispatcher`, assert one succeeded run with a
  bound session and execution id, assert the second dispatch claims nothing,
  assert one future slot for a recurring definition, assert
  `provider.request_count() == 0`, and assert `LocalWaiters == 0` after the
  dispatcher drops its client.
- `cargo test -p leveler-app --test deterministic_action_run` — 1 passed: the
  real socket runtime executed `printf 'leveler-action-ok'` for a session with
  zero provider requests, and the workspace gained no file.
- `cargo test -p leveler-native` — 33 unit tests + 1 live discovery test passed,
  including the two automation-probe regressions below.
- `cargo test -p leveler-cli --bin leveler schedule_subcommands_parse` — passed;
  `cargo check -p leveler-cli` passed.
- Real macOS CLI run: `schedule add` → `dispatch` produced one `succeeded` run
  (exit 0, `session_id` and `execution_id` recorded), the action's file was
  written, a second dispatch claimed 0, and the loopback sink logged 0 requests.
- Real crash window: the action was `sleep 12; printf 'crash-ok' > …`. The
  dispatcher was `SIGKILL`ed 2 s after the claim while the run was `running`;
  the shell still completed and its file existed. At the 120 s lease expiry the
  next dispatch reported `recovered=1`, the run became `unknown` with
  `"the dispatcher's lease expired before the run reported a terminal state; the
  action's effect is unknown and it was not retried"`, the occurrence became
  `interrupted`, and the following dispatch claimed 0. The recorded outcome is
  deliberately unknown even though the side effect happened.
- Idle eviction: with the 5 s override, the runtime daemon exited 5.1 s after
  the dispatch client was dropped; no process remained between slots.
- Windows compile only: `cargo check --target x86_64-pc-windows-gnu -p
  leveler-schedule -p leveler-cli` passed. No Windows host ran the schedule.

### Authorization and permission

The definition stores the wire permission profile; session creation receives
it, and `user_shell_execution` maps it to the existing execution write scope
(`RequestApproval` and `Assisted` confine to the workspace, `FullAccess` is
unrestricted). The dispatcher never opens an approval prompt: there is no
unattended ask in this path. No TCC state is created, cached or upgraded by a
schedule; the Phase 2 permission probe remains fail-closed `Unknown` and its
bounded detached probe, generation registry, single-replacement allowance and
post-consent re-probe behavior are unchanged.

Honest limit: a live Assisted/FullAccess comparison wrote to a system temp path
and both succeeded, because macOS permits that directory. The code path from
the stored profile to `write_scope` is verified; an OS-level denial of a write
outside the workspace was not re-proven in this phase and rests on the existing
execution sandbox, not on schedule code.

Schedules carry shell commands only. Native actions are not schedulable, so a
one-off native approval cannot become unattended authorization, and the Phase 2
authorization gate is untouched. `NA-02` remains **BLOCKED**; no email was sent
and `mail.sent.check` remains read-only.

### Limits and risks

- At-most-once is intentional. A crash after delivery leaves `unknown`; there
  is no retry, and recurring missed slots are not backfilled.
- The dispatcher identifies its run by a newly appeared `UiUserShell` with the
  exact command text in the definition's session. Another client running the
  same command concurrently in that session could be mistaken for the run. The
  session is dedicated by its goal, but exclusivity is not enforced.
- The runtime refuses to start without at least one configured model, even
  though a shell schedule never calls one; observed live as a startup failure
  (`no models configured`). Schedules cannot run on a model-less install.
- A long `LEVELER_HOME` path exceeds the macOS `SUN_LEN` socket limit and
  produces a typed failed run (`path must be shorter than SUN_LEN`). The failure
  was recorded honestly, but no run is possible in such a home.
- `serve` is foreground-only. No launchd or Task Scheduler integration was
  installed or tested, so real OS wakeup after sleep/login and its timing remain
  **NOT_VERIFIED**; the short-lived arm was simulated by an external timer.
- There is no TUI or Desktop schedule surface yet; only the CLI exists.
- Windows runtime behavior, shell execution and daemon supervision remain
  **NOT_VERIFIED** beyond the compile check.

### Next step

Windows host acceptance; an OS autostart integration only with explicit
authorization and sleep/login wake tests; native-action and real-mail schedules
only after their separate authorization and consent gates. Phase 1 stays
**PARTIAL** and Phase 2 stays **PARTIAL / NA-02 BLOCKED**; this phase does not
promote any of their items.
