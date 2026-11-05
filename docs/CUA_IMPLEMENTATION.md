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

**PARTIAL — macOS OS wakeup is verified end to end; Windows, real Linux,
real sleep/logout/login and native-action scheduling are NOT_VERIFIED.** A
per-user LaunchAgent wakes the ordinary short-lived `schedule dispatch` every
60 s with no Desktop or TUI and no resident scheduler. Real runs were observed
from the OS start through claim, delivery, runtime start and terminal exit,
plus a duplicate trigger, a dispatcher crash, missed slots under both misfire
policies, uninstall, and the independence of definitions from the OS record.
The store remains the only schedule authority: the OS record is an
opportunity to be started, never an execution fact.

Baseline: `bf19f8b` (the committed schedule foundation) plus `224712a`
(deterministic runtime and execution identity) and this commit,
`aarch64-apple-darwin`, macOS 26.5, debug build. Windows,
real Linux execution, real sleep/logout/login and real mail sending remain
blocked by the same conditions recorded in Phase 2.

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
`ClientCommand::RunUserShell`, delivered through a `CommandEnvelope` whose
`CommandId` is `schedule-run-{run_id}`. The runtime persists that id on the
started fact (`UserShellStarted.origin_command_id`) and projects it into
`UiUserShell.origin_command_id`; the dispatcher selects exactly the execution
carrying its own id and never matches command text. Identity is what keeps two
clients running identical text in one session distinguishable, and the runtime's
command receipts deduplicate a retransmitted envelope onto the same execution.
The dispatcher observes the terminal `UserShellFinished` record by polling the
session snapshot, because a socket event subscription attaches asynchronously
while a fast action can finish before it is live. The snapshot merges live
records with the durable user-shell history, so a completed fast action is
still observed, including its persisted origin.
The client is dropped after every claim, leaving no attached waiter, and the
daemon's existing idle eviction remains in force. The dispatcher owns no
runtime, session DB, engine, approval system or process lifecycle.

### Phase 3.2 — OS wakeup (2026-11-04)

**Contract.** `schedule enable` writes an opt-in, per-user OS record whose only
job is to start the ordinary `schedule dispatch --source os` command;
`disable` removes the record; `status` reports the observed OS state beside
the definition count, next occurrence, overdue slots and last OS wake;
`doctor` compares platform support, registration, executable, exact arguments,
interval, environment, overdue slots and the last wake, and exits non-zero only
on failed checks; `uninstall` removes the record and keeps every definition,
occurrence and run. Adding a definition never registers the service. The OS
record carries no schedule content and no secret, so every dispatch rereads the
store and a definition change needs no OS update.

**macOS.** A `~/Library/LaunchAgents` LaunchAgent with a deterministic
FNV-1a label `com.leveler.schedule.<16 hex>`, `StartInterval=60`,
`RunAtLoad`, `ThrottleInterval=10`, `ProcessType=Background` and
`AbandonProcessGroup`, loaded into `gui/<uid>`. Enable is idempotent
(`bootout` → `enable` → `bootstrap`, with half-registration cleanup); disable
boots out, removes the plist and verifies the job is gone; status parses
`launchctl print`, including last exit code and run count. `RunAtLoad` is the
login path; launchd decides when a woken or sleeping machine runs the job, and
the misfire policy decides whether the overdue slot still runs. The exact
executable path, arguments, working directory and home are stored, including
spaces and CJK text; environment is limited to non-secret `PATH`, `HOME`,
`TMPDIR`, `LEVELER_HOME` and the idle-timeout override. Real sleep, wake and
logout/login behavior were not exercised and remain **NOT_VERIFIED**.

**Windows.** A per-user Task Scheduler XML task with `InteractiveToken` (runs
only in that user's interactive session) and `LeastPrivilege` (no
administrator, no stored password), `IgnoreNew`, `StartWhenAvailable`, a
one-minute repetition and a UTF-16LE BOM. `schtasks.exe` is the registration
authority: `/Create /F` is the idempotent update, `/Query /TN ... /XML` is the
observed state, `/Delete /F` is disable, a failed create removes a possible
half registration, and stdout/stderr are decoded from UTF-16 or UTF-8. This
was compile-checked only; a real Windows host is **NOT_VERIFIED**, including
dialogs, session availability, launch failures, duplicates and the actual GUI
session.

**Linux.** The backend is an explicit, typed `unsupported` result; no systemd
unit was added. A cross-compile check could not run because the host lacks the
Linux C toolchain needed by `libsqlite3-sys`, so Linux compilation is
**NOT_VERIFIED**.

**Misfire and recovery.** Default is `skip` with a 300 s grace. A pending slot
older than its grace is recorded `missed`, is never executed and never
backfilled; a recurring definition still gets exactly one future slot.
`run-once` leaves the single overdue slot claimable for this pass only. Policy
only rewrites `pending` rows: claimed, completed, interrupted and missed rows
are terminal facts. A claim has a 120 s lease renewed every 40 s. If the
dispatcher dies, the next pass recovers the expired claim as `unknown` with
`interrupted`, using whether delivery and runtime start had been observed to
say "never acknowledged", "acknowledged but never reported" or "already
started; effect unknown"; it never retries. A wake record is written for every
pass, including passes that claim nothing and passes that die before a verdict
(kept 30 days). Driver states remain distinguishable: not woken (no row),
woken but claimed nothing, claimed but never delivered, delivered but never
started, started with unknown effect, and succeeded/failed.

**Identity and authorization.** Delivery is the existing
`ClientCommand::RunUserShell` with `CommandId("schedule-run-<run_id>")` as both
command id and `origin_command_id`; the schedule occurrence is claimed by a
compare-and-set and the session is bound once. A duplicate OS start therefore
claims 0 in the second dispatcher. There is no approval prompt in this path and
no upgrade of a one-off tool approval into unattended authorization; the
definition's stored permission profile is the only authorization input.

**CLI surface.** `schedule add/list/remove/runs/dispatch/serve/enable/disable/
status/doctor/uninstall`; schedule subcommands accept the hidden global
`--home` so a registration, a dispatch and `doctor` can address the same
isolated home on every platform (Windows Task Scheduler cannot set an
environment map).

#### Real evidence (macOS, same worktree)

| Test | Observed |
|---|---|
| DF-S321 real OS wake | slot `13:00:12Z`; launchd started `schedule dispatch` at `13:00:46.218Z`; claim at `.218`; delivered `.527`; runtime running `.775`; finished `13:00:47.089Z`, exit 0; wake #2 `claimed=1 succeeded=1`, provider requests 0 |
| DF-S322b runtime restart and idle eviction | no daemon before the slot; slot `13:06:25Z`; wake/claim `13:06:39.508Z`; delivered `.600`; running `.626`; finished `13:06:45.114Z` (5.606 s incl. the 5 s action); dispatch RSS 34.4 MB; runtime RSS 46.4–47.0 MB; runtime last seen `13:06:52Z`, gone `13:06:53Z` with a 10 s idle override |
| DF-S323 duplicate trigger | two dispatchers in the same second: `dup-a claimed=1 succeeded=1`, `dup-b claimed=0`; exactly one effect-file line |
| DF-S324 dispatcher crash | claim `13:10:42.660Z`, delivered `.784`, running `.840`, `SIGKILL` at `13:10:43Z`; lease deadline `13:12:42.660Z`; next OS wake `13:12:43.095Z` recovered the claim; run `unknown` at `.096` with "the dispatcher's lease expired ... the action's effect is unknown and it was not retried"; occurrence `interrupted`; effect file written exactly once; `runs_count=1` |
| DF-S325 missed trigger | service disabled `13:13:19Z`–`13:14:04Z`: 0 wakes, 0 runs, no files; `skip` slot `13:13:49Z` → `missed` and no run; `run-once` slot → exactly one `succeeded` run and one file line; recurring definition → exactly one future `pending` slot; wake #18 `missed=1 claimed=1 succeeded=1` |
| DF-S326 delete ≠ delete service, uninstall | removing one definition left `registered=true loaded=true`, plist present and `launchctl print` exit 0; `uninstall` → `registered=false loaded=false`, plist gone, `launchctl print` exit 113; 7 definitions and 19 wake rows retained; 90 s later 0 new wakes, 0 new runs, no leveler processes |
| Opt-in | with the service uninstalled, `schedule add` left `registered=false loaded=false` and created no plist; removing it restored the definition count |
| Spaces and CJK paths | `/tmp/leveler-s32 spaced 中文/{bin,leveler-home,config dir}`: enable/status/doctor/uninstall all exit 0; the plist preserved the exact arguments, `WorkingDirectory` and `LEVELER_HOME`; after uninstall the plist was gone and `launchctl print` exited 113 |

Commands: `cargo test -p leveler-schedule` (23 lib + 13 integration tests, all
passed) and `cargo test -p leveler-cli --bin leveler schedule_subcommands_parse`
(passed). Windows compile only:
`CC_x86_64_pc_windows_gnu=... cargo check -p leveler-schedule --target
x86_64-pc-windows-gnu` and the same for `leveler-cli` (both succeeded). The
E2E scripts and logs are in `/tmp/leveler-s32-e2e/`; the launchd label used for
the real tests was `com.leveler.schedule.85433d029b31a693` (plus the isolated
CJK label), and both were uninstalled and verified gone.

### Driver comparison

Method: debug `leveler` binary on this macOS host; short `/private/tmp` home so
the runtime socket fits `SUN_LEN`; model requests stayed at 0. Phase 3.1
measured both arms with an external `sleep` loop; Phase 3.2 replaced that timer
with a real launchd LaunchAgent and repeated the short-lived arm. A is the OS
registration plus `schedule dispatch --source os`; B is a resident
`schedule serve --poll-secs 1`, measured idle on an empty store. Absolute
values are debug-build figures.

| Metric | A: OS wake + short-lived dispatch | B: resident `serve` |
|---|---|---|
| resident process | none between slots | one process |
| idle RSS | 0 | 26.0–26.6 MB |
| idle CPU | 0 | 0.0 %; 0.04 s CPU over 15 s |
| per-pass cost with no due slot | 0.01 s wall, 26.3 MB max RSS | n/a (resident) |
| claim → finished, real slot | 5.606 s including the 5 s action (0.61 s dispatcher/runtime overhead, cold runtime) | 0.34–0.44 s with a warm daemon for a quick action (Phase 3.1) |
| due → claim | 14.5 s in DF-S322b and 34 s in DF-S321, set by the 60 s OS interval | 3 ms (Phase 3.1) |
| dynamic definition change | next pass rereads the store; no OS update | next poll rereads the store |
| duplicate trigger | one claim, one effect (DF-S323) | one claim (same store compare-and-set) |
| dispatcher crash | expired lease recovered by the next OS wake (DF-S324) | process death stops every pass until manually restarted |
| login / reboot | `RunAtLoad` starts it at login | not started automatically; foreground only |
| binary upgrade | next wake executes the current binary at that path | old binary until the process is restarted |
| uninstall | `bootout` plus plist removal, no process left (DF-S326) | must stop the process; no OS record to remove |
| extra runtime | none | none; the same runtime daemon is used when work exists |

Decision: **A**. Claim timing is bounded by the OS minute interval, but a
scheduled action is minute-granular anyway; the cold-start overhead above the
action's own duration was 0.61 s, and A leaves no process resident. B removes
roughly 0.2–0.3 s of process start and returns a claim in 3 ms, but pays one
resident 26 MB process that does not survive logout and has no supervisor. B
does not solve sleep or login either: a suspended process misses slots exactly
like A, and a logged-out user has no session for either arm; B would still need
the same OS registration plus a resident process. A is therefore the simpler
reliable default; B remains available for sub-minute polling only.

### Real evidence

- `cargo test -p leveler-schedule` — 36 tests passed (23 lib tests, including
  9 wakeup tests, plus 13 integration tests). They lock atomic claim-once,
  one-slot recurrence, no `Once` successor, expired-lease recovery,
  non-rebinding session identity, cascade removal, the `skip` and `run-once`
  misfire policies, the terminal-row invariant, wake retention, launchd plist
  round-trip and launchctl parsing, and the Windows XML round-trip. The E2E
  tests run a real `Application`, `LocalSocketServer`,
  `LocalSocketRuntimeClient` and `Dispatcher`, assert one succeeded run with a
  bound session and execution id, assert the second dispatch claims nothing,
  assert one future slot for a recurring definition, assert
  `provider.request_count() == 0`, and assert `LocalWaiters == 0` after the
  dispatcher drops its client.
- `cargo test -p leveler-storage migration_0048_keeps_legacy_sessions_and_admits_no_model_free_turn`
  — passed (0.06 s): a legacy database upgrades without losing sessions and
  the nullable model column admits a model-free session without a half
  migration.
- `cargo test -p leveler-app --test deterministic_action_run` — 1 passed: the
  real socket runtime executed `printf 'leveler-action-ok'` for a session with
  zero provider requests, and the workspace gained no file.
- `cargo test -p leveler-native` — 33 unit tests + 1 live discovery test passed,
  including the two automation-probe regressions below.
- `cargo test -p leveler-cli --bin leveler schedule_subcommands_parse` — passed;
  `cargo check -p leveler-cli` passed.
- Phase 3.1 real macOS CLI run: `schedule add` → `dispatch` produced one
  `succeeded` run (exit 0, `session_id` and `execution_id` recorded), the
  action's file was written, a second dispatch claimed 0, and the loopback sink
  logged 0 requests. Phase 3.2 repeats this through a real launchd wake; see
  the DF-S321–DF-S326 table above.
- Phase 3.1 real crash window: the action was `sleep 12; printf 'crash-ok' > …`.
  The dispatcher was `SIGKILL`ed 2 s after the claim while the run was
  `running`; the shell still completed and its file existed. At the 120 s lease
  expiry the next dispatch reported `recovered=1`, the run became `unknown` with
  `"the dispatcher's lease expired before the run reported a terminal state; the
  action's effect is unknown and it was not retried"`, the occurrence became
  `interrupted`, and the following dispatch claimed 0. The recorded outcome is
  deliberately unknown even though the side effect happened. DF-S324 repeats
  this against a real OS-dispatched run.
- Idle eviction: with the 5 s override, the runtime daemon exited 5.1 s after
  the dispatch client was dropped; no process remained between slots. DF-S322b
  repeats it with the 10 s override and an OS-started dispatcher.
- Windows compile only: `cargo check --target x86_64-pc-windows-gnu -p
  leveler-schedule -p leveler-cli` passed (with the host's mingw `CC`).
  No Windows host ran the schedule.

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
- Correlation is by delivery identity, never command text.
  `origin_command_id` is additive and absent on executions recorded before the
  field existed, so a schedule cannot adopt a pre-upgrade execution; it waits
  for its own and settles `unknown` on timeout rather than guessing from text.
  The session is dedicated by its goal, but even two clients and a schedule
  sharing one session keep three separate identities (locked by a real-socket
  concurrency test).
- A model-free install is supported: the daemon starts with no configured
  model, `!command` and scheduled shells execute, and provider requests stay at
  0. A deterministic-only session stores `model = NULL`; an agent turn or goal
  on it fails with an explicit configuration error instead of inventing a
  model. Model-backed sessions are unaffected.
- A long `LEVELER_HOME` path exceeds the macOS `SUN_LEN` socket limit and
  produces a typed failed run (`path must be shorter than SUN_LEN`). The failure
  was recorded honestly, but no run is possible in such a home.
- The registration is opt-in and never enabled by adding a schedule. `enable`
  installs a per-user record; `disable` and `uninstall` remove it. No permanent
  background service is installed by default, and no second execution engine is
  introduced: the record only starts the ordinary dispatcher.
- macOS wake timing is launchd minute-granularity. A slot may start up to
  roughly one interval late, and launchd may coalesce or skip a wake while the
  machine sleeps. The misfire policy, not the OS, decides whether the slot then
  runs. Real sleep/wake and logout/login transitions were not exercised on this
  host and remain **NOT_VERIFIED**.
- `serve` is still foreground-only. It is not a supervisor and does not survive
  logout; the OS registration is what makes either arm start again at login.
- Windows registration is compile-checked only. A real Windows host has not
  confirmed the interactive-session restriction, login behavior, duplicate
  handling, launch failure reporting or `schtasks` output parsing; verbose
  output is locale-sensitive, so only `Last Result` is consumed when present.
  The real Windows result is **NOT_VERIFIED**.
- Linux has an explicit unsupported backend and no systemd unit. A Linux
  compile check could not run because the host lacks the Linux C toolchain
  required by `libsqlite3-sys`; Linux compilation is **NOT_VERIFIED**.
- The OS record can be edited or removed outside CodeLeveler. `doctor` reports
  the drift (executable, exact arguments, interval, environment, overdue
  slots) but does not silently repair it; re-running `enable` is explicit.
- On macOS, `launchctl enable` leaves an inert `<label> => enabled` entry in
  launchd's root-owned override database even after `uninstall` removes the
  plist and boots the job out. The entry cannot start anything without a plist
  and launchd exposes no unprivileged way to delete it; `disable` does not
  remove it either.
- There is no TUI or Desktop schedule surface yet; only the CLI exists.
- Windows runtime behavior, shell execution and daemon supervision remain
  **NOT_VERIFIED** beyond the compile check.

### Next step

Windows host acceptance of the registered task (interactive session, login,
duplicates, uninstall); real macOS sleep/wake and logout/login transitions; a
Linux compile environment with the C toolchain; native-action and real-mail
schedules only after their separate authorization and consent gates. Phase 1
stays **PARTIAL** and Phase 2 stays **PARTIAL / NA-02 BLOCKED**; this phase does
not promote any of their items.

## Phase 4 — Scheduled Automation product closure (2026-11-04)

### Verdict

**macOS: the three action kinds (`shell`, `native`, `agent`) share one schedule
store and one runtime; the agent path was observed end to end with a real
DeepSeek model, and native actions run through `leveler-native` and the host
execution authority.** A schedule can now be `once`, `every`, `daily`, `weekly`,
`cron` or an explicit run-now, in an IANA timezone, with explicit overlap,
misfire, session and unattended-authorization policies. An agent run is an
ordinary runtime session, task, turn and goal; the dispatcher hands the goal
over and returns, and a later dispatch pass settles the run from the runtime's
own terminal record. No second scheduler, agent loop or execution authority was
added.

Windows remains **compile-checked only**; a real Windows host did not run the
task, native actions or the schedule. Linux remains an explicit typed
`unsupported` OS-wakeup backend, and even a compile check is blocked on this
host by the missing Linux C toolchain for `libsqlite3-sys`. Real mail sending
remains **BLOCKED**: no run executed `mail.send.submit`, and that action is only
schedulable when the persisted grant explicitly covers external side effects.

Baseline: `82ea7dd` (OS wakeup and misfire) plus this worktree; debug
`aarch64-apple-darwin`, macOS 26.5.

### Trigger and time contract

| Rule | Stored facts | Next occurrence |
|---|---|---|
| `once` | `once_at` | exactly once, never a successor |
| `every` | anchor + interval seconds | first grid point strictly after now; no accumulated drift |
| `daily` | `HH:MM` + IANA timezone | next local wall clock |
| `weekly` | weekday set + `HH:MM` + IANA timezone | next listed local weekday |
| `cron` | five fields + IANA timezone | next matching local minute, floored to `:00.000` |
| run-now | one `run_now=1` occurrence | dispatched on demand; not part of the rule grid |

Local times use `chrono-tz`. A nonexistent wall time (spring forward) runs at
the first valid instant after the gap; an ambiguous wall time (fall back) runs
once at the earlier instant and is not repeated. Month-end, leap day, invalid
cron and sub-minute cron are rejected or resolved by the rule itself. Every
next occurrence is computed from the rule and the current instant, never from
the previous run's finish, so a late run cannot shift the grid. Cron slots are
minute-aligned after the fix below; daily/weekly slots already were. An expired
`once` slot is not resurrected; a recurring definition keeps exactly one future
slot and missed slots are not backfilled.

The attempt carries four distinct instants: `scheduled_for` (the rule), the
wake/claim instant, `delivered_at` / `handed_off_at` / `running_at` (what the
dispatcher observed), and `finished_at` (the terminal fact). None is derived
from another.

**Cron alignment defect found and fixed during dogfood.** The first cron
occurrences carried the creation instant's sub-second part (`16:15:00.904Z`,
`16:20:00.723Z`) because the cron engine preserved the query's sub-second
precision. A fixed 60 s OS wake that fires near `:00.x` can be earlier than
such a slot, delaying it by a whole interval. `ScheduleTrigger::next_after`
now floors the local query instant to the minute, and the live launchd job
produced `16:28:00.000Z` and `16:29:00.000Z` afterwards. Locked by
`rule::tests::cron_next_occurrence_is_minute_aligned` (red at
`16:26:00.752Z`, green at `16:26:00Z`).

### Action kinds

| Kind | Execution chain | Completion fact |
|---|---|---|
| `shell` | `ClientCommand::RunUserShell` with `CommandId("schedule-run-{run_id}")` | runtime `UserShellFinished`, exit code and `execution_id` |
| `native` | typed id + structured JSON → `leveler-native` classify/execute → `NativeActionExecutor` → host authority (AppleScript / UIA / Mail) | native report `status`, `capability_state`, `permission_state`; `Submitted` is not success |
| `agent` | ordinary `CreateSessionRequest` + `SubmitGoal` through the existing runtime, model and tools | a terminal task record owned by this run's turn (`sequence > baseline_sequence` and `latest_turn_ordinal == run.turn_ordinal`), `task_outcome` and usage delta |

No action is converted to a shell string. The native executor receives the
typed `ActionId` and validated input; an unknown field or an unsupported action
is `invalid_input` / `unsupported` and settles `blocked` without starting
anything. `mail.send.open_ui` is refused because it needs a person;
`mail.send.submit` is classified as external side effects.

### Agent run lifecycle

`SessionPolicy::Continue` binds one session per definition on the first run and
reuses it; `Fresh` creates one session per run and binds nothing. The session
is created with the definition's workspace, permission profile and model; a
`Continue` run re-asserts `AutoApprove` from the persisted grant before
submitting, because approval policy is not a persisted session fact, and
restores `Interactive` immediately after the admission on both success and
failure so the policy cannot leak into a later person's turn. A goal is
delivered once with `CommandId("schedule-goal-{run_id}")` and
`require_new_turn: true`: if another client's turn is already running, the
dispatcher records `skipped` instead of steering into that client's work. The
dispatcher records the submitted `TaskId`, the pre-submission task `sequence`,
the pre-submission newest turn ordinal, and — only after a successful
delivery — the ordinal of the turn the delivery started. A later pass observes
the snapshot: a terminal record at or below the baseline belongs to an earlier
turn and cannot settle this run; a terminal is attributable only while the
run's recorded ordinal is still the session's newest (`latest_turn_ordinal`).
If a person starts and finishes a newer turn, this run's terminal is no longer
projected, so the run settles `unknown` at its deadline and never settles,
cancels or charges that person's turn. Only a later terminal in the run's own
turn sets `succeeded`/`failed` plus `task_outcome` and the usage delta
(`final.since(baseline)`, missing components stay missing). A pending
interaction is not a failure and not a success: it settles
`waiting_approval` with a reason and sends a cancellation, and the runtime's
terminal record is the cancellation result. If no terminal arrives within the
timeout the run becomes `unknown` and is not retried.

The run's identity is `(task, task sequence baseline, turn ordinal)`, not just
`task_id + sequence`: a terminal satisfies `sequence > baseline` even when it
belongs to a later turn in the same session. `TurnOrdinal` is captured from
the runtime snapshot after delivery and stored on the run; `None` means the
identity could not be established, and reconciliation then never settles or
cancels from a terminal. `RunGoal.require_new_turn` and
`UiSessionSnapshot.latest_turn_ordinal` are additive, defaulted wire fields:
an older client omits the flag, and an older runtime reports `None`.

### Authorization

There are two independent layers:

1. **The definition's permission profile** (`assisted`, `request-approval`,
   `full-access`) is passed into the runtime session; it governs what the
   runtime/tools permit. Interactive approvals are never promoted into
   scheduled authority.
2. **The persisted unattended grant** is required by the dispatcher for every
   action kind (`shell`, `native` and `agent`), in every permission profile: a
   profile governs interactive admission, not durable background
   authorization. The grant is scoped by definition id, canonical workspace,
   permission mode, action kind, the exact action fingerprint (shell command,
   or native action + its full structured input, or agent goal), the
   authorized model, the trigger scope (cadence and timezone) and an explicit
   `external_side_effects` flag; `revoke-grant` removes it. A definition row
   whose action, input, goal, model or cadence was edited after issuance no
   longer matches its grant and is refused before the authority starts.
   Grants whose version is not `3` (including the older kind-only grants)
   fail closed and must be re-issued. A grant is created explicitly
   (`--grant-unattended`), never by adding a rule or by answering one prompt.
   A TCC/macOS permission is still enforced by the host authority;
   `full-access` bypasses the CodeLeveler gate, not the operating system.

The dispatcher decides from the persisted definition re-read immediately
before delivery, not from the snapshot carried by the claim, so a grant
revoked or narrowed after the claim cannot start the action. Revocation is not
a retroactive kill: an attempt that already passed the decision is authorized
work, and like `pause` it is not interrupted (use `cancel` for that). The
remaining window between the decision and the start of the external effect is
inherent in an in-flight action; there is no second approval engine and no
per-action re-approval.

A run that needs a person (`mail.send.open_ui`), lacks a capability, or lacks
TCC permission settles `waiting_approval` or `blocked` with the observed
reason. The dispatcher never waits for a dialog and never prompts repeatedly.

### Overlap, misfire, retry and cancel

- `skip-if-running` (default): a due slot suppressed by an active run of the
  same definition is recorded `skipped`. `queue-one`: the slot stays pending
  and becomes claimable after the active run settles. `allow-concurrent`:
  only when the type and resources permit; two GUI tasks contending for one
  desktop input are not allowed by the product contract, and a concurrent
  agent definition must use a fresh session per run.
- Claiming is one SQLite CAS transaction; the `(definition_id,
  scheduled_for)` unique key deduplicates OS wakes. The claim carries a 120 s
  lease; an expired lease becomes `interrupted`/`unknown` with an explicit
  reason and is never retried. Claims taken by one pass run concurrently,
  bounded by `max`, so a long shell or native action cannot delay a claimed
  agent handoff or another due schedule.
- Misfire: `skip` (default) records an older-than-grace slot `missed` and runs
  nothing; `run-once-on-recovery` leaves the single overdue slot claimable for
  one pass. Neither backfills multiple slots.
- Retry is absent by design. The persisted facts still keep the required
  distinctions: `failed` is a terminal error after an attempt (no automatic
  retry); `blocked` was refused before any authority started; `submitted` /
  `completed` may already have produced an external side effect; `unknown` was
  delivered or may have been delivered without a terminal; a claim with no
  `delivered_at` / `running_at` was not delivered; a claim with `delivered_at`
  but no terminal is delivered-unconfirmed; the bound session/task record is
  the agent's resumable identity, owned by the runtime, not by a re-run of the
  schedule. `unknown` is never retried; a person must read the run history.
  `run-now` is the explicit manual new attempt, never an automatic retry.
- `pause` stops future slots and never kills an active run; `remove` is refused
  while a run is active (`cancel` first); `cancel` records the request, the
  owning dispatcher sends the runtime cancellation while the process is
  observable, and the runtime's terminal record settles `cancelled`. At most
  one cancellation is issued per run.

### CLI

`schedule add` accepts exactly one of `--command`, `--native-action
--native-input` or `--agent-goal`, plus `--at`, `--every`, `--daily`,
`--weekly --local-time`, `--cron` and `--timezone`, `--model`,
`--session-policy`, `--overlap`, `--misfire`, `--timeout`,
`--grant-unattended [--allow-external-side-effects]` and `--permission`.
`show` prints action, trigger and timezone, next occurrence, permission, model,
bound/fresh session, overlap, misfire, grant, and recent runs; `runs` prints
state, slot, delivery/start/finish, exit code, provider, native facts, usage,
session and error; `run-now`, `cancel`, `pause`, `resume`, `revoke-grant`,
`remove`, `dispatch`, `serve`, `enable`, `disable`, `status`, `doctor` and
`uninstall` complete the surface. Schedule subcommands accept the hidden global
`--home` so an isolated home can be addressed on every platform.

### Real evidence (macOS)

| Dogfood | Observed |
|---|---|
| DF-41 daily/weekly/cron | `dogfood-daily` (`00:15 Asia/Shanghai`) first slot `16:15:00.000Z`, next `2026-11-05T16:15:00Z`; `dogfood-weekly` (`mon,thu`, same time) first slot executed, next `2026-11-08T16:15:00Z`; `dogfood-cron` (`*/5`) slots `16:15:00.904Z` and `16:20:00.723Z`, the latter `missed` under `skip` after its 300 s grace, next `16:30:00.039Z`. Real launchd wake #16 (`16:28:14.447Z`) claimed and executed the minute-aligned cron slot `16:28:00.000Z`; next slot `16:29:00.000Z`. The LaunchAgent then ran unattended until it was uninstalled: the cron marker reached 473 lines, wakes reached id 486, and every due slot was claimed and settled exactly once. All runs carry session id, slot, start and finish. |
| DF-42 native | `mail.sent.check` ran through the real `macos.mail` backend when dispatched from the terminal: run `e569ba27…` `succeeded`, `native_outcome=completed`, `capability_state=available`, `permission_state=granted`. The same action scheduled in the isolated OS-wake home settled `waiting_approval` (`native_outcome=requires_permission`, `capability_state=requires_permission`, `permission_state=not_determined`, reason "Apple Events automation consent has not been determined; the first action asks the user, and discovery does not prompt") because the LaunchAgent process has no Automation consent of its own: it did not wait, prompt or elevate. `mail.send.submit` (`native-send-refused`) settled `blocked` with "the action can create external side effects and the unattended grant did not authorize them"; the authority was never started and no mail was sent. Invalid native input now settles `blocked` / `invalid_input` with "unknown field `limit`, expected `to` or `subject`". |
| DF-43 agent | run `47264ef3…`, session `747ba0cf…`, goal "Read README.md … create SUMMARY.md …"; real provider `deepseek`, 4 model requests, 38,112 input (25,600 cached), 613 output, 167 reasoning tokens, 5,463 µs cost; 42 session events, 3 tool calls (`read_file`, `write_file`, `update_goal`); `SUMMARY.md` contains `blue-green`; `task_outcome=completed`, `task_sequence=42`. This real run predates the turn-identity hardening; the ownership rule itself is locked by the controlled-runtime tests in `scheduled_agent` (later turn cannot settle, unowned cancel refused, restarted dispatcher settles). |
| DF-44 no Desktop/TUI | launchd started `schedule dispatch --source os` from the LaunchAgent with no TUI or Desktop process; the whole launchd sequence (wakes reached id 486) claimed 0 when nothing was due and 1 when the store held a due slot, with no other driver involved. |
| DF-45 long run outlives the dispatcher | run Now handoff at `16:17:21.158Z`; its wake finished at `16:17:21.159Z` (`handed_off=1`, `succeeded=0`); the task finished at `16:18:14.984Z` and was settled by a later pass. |
| DF-46 duplicates/crash/pause/delete | duplicate OS wakes claimed 0; crash: claim `16:22:51.627Z`, lease `16:24:51.627Z`, `SIGKILL` at `16:22:58.42Z`; the shell kept running and wrote its marker, and the recovery pass at `16:25:20.039Z` settled the run `unknown` ("the runtime had already started this action; … not retried"), occurrence `interrupted`, exactly one execution marker; pause mid-run left the shell running to exit 0; remove was refused while active and succeeded after settlement; `cancel` requested at `16:22:29.082Z`, issued at `.147`, runtime terminal settled `cancelled` at `.430Z` with `execution_id`; model `deepseek/does-not-exist` failed with "not configured". |
| DF-47 security | unauthorized external side effect blocked before execution; no TCC permission was created or upgraded; a temporary approval is not a grant (grant tests: matching scope only, revocation effective); crash/unknown is not retried; model-free shell and native schedules ran with zero provider requests in a home with no `config.toml`. |
| Concurrency | Two `once` shell schedules due at the same instant (`00:46:19Z`) were claimed by one pass (`--max 4`): both runs started at `00:46:21.060Z` and finished at `00:46:25.319Z` / `.337Z`, so the two 4 s actions ran in parallel; each marker was written exactly once and the concurrent second dispatcher claimed 0. The claim is still bounded by `max` and at-most-once by the store CAS. `two_due_actions_are_delivered_concurrently` locks this behavior (red under the old sequential loop). |
| Interactive regression | The ordinary TUI agent was driven over a real PTY with the machine's real DeepSeek config while the schedule feature was present: session `e794c4b6…`, turn `c4a35c93…` `completed`, 2 provider rounds (7,971 / 8,702 input, 6,144 cached, 710 / 38 output, 2,523 µs total cost), the model wrote `probe_real.go` with exactly 103 lines, and all 8 PTY assertions (full diff paint, fold/unfold, `/copy diff`) passed. The interactive path never enters the scheduler. |

Commands: `cargo test -p leveler-schedule` (84 tests: 40 unit, 8 edge,
24 agent, 6 policy, plus the existing identity/model-free/action suites; all
green), `cargo test -p leveler-native --lib` (33 passed),
`cargo test -p leveler-cli --bin leveler schedule` (passed). Windows GNU
compile: `cargo check --target x86_64-pc-windows-gnu -p leveler-schedule -p
leveler-cli` (passed after the cron fix). Interactive regression:
`python3 scripts/pty_dogfood.py --provider real --only live` (8 assertions,
0 failures; re-run against this worktree). The dogfood home is
`/tmp/leveler-df4` (isolated `LEVELER_HOME`, no provider key in
`config.toml`; the key was supplied only through the environment for the agent
run). The real launchd label was uninstalled after the evidence was recorded.

### Limits and risks

- Windows is compile-only; no Windows host ran the task, native action or
  schedule. Linux is unsupported by design and not compile-checked here.
- Real TCC revocation during a scheduled native run was not exercised
  (`RequiresPermission → waiting_approval` is covered by a unit test); real
  sleep/wake and logout/login transitions remain **NOT_VERIFIED**.
- `mail.sent.check` is read-only; no real mail was sent and `mail.send.submit`
  remains blocked without an explicit external-side-effect grant. The control
  recipient is still not configured.
- A cron slot is minute-granular; launchd decides the actual start within the
  minute, and the misfire policy decides whether an overdue slot still runs.
- Concurrent dispatcher passes are at-most-once, but the candidate snapshot is
  bounded by `--max`. Two passes racing with an explicit `--max 1` can leave a
  claimable slot pending for the next pass (observed in the concurrency probe);
  the OS registration uses the default 4, and the slot is not lost. This is a
  latency edge, not a duplicate or a lost run.
- A live system timezone change or clock jump was not exercised; next
  occurrences are always recomputed from the current instant and the stored
  rule, and DST transitions are covered by unit tests, not by moving this
  host's clock.
- The dispatcher lacks a TUI/Desktop surface (Phase 5); only the CLI exists.
- An agent run is only as good as the runtime's own terminal record. A
  `waiting_approval` outcome means a person is needed; it is not silently
  converted into a retry.
- Usage is the session's cumulative delta across the run's own turn. A person
  who steers extra input into that same turn is included; the runtime does not
  persist a per-request turn link (`model_requests.turn_id` is currently
  unused), so per-request attribution is not claimed.
- A terminal that is no longer projected because a newer turn exists leaves
  the run `unknown`; the person's turn is never settled or cancelled by the
  schedule, and the run is not retried.

