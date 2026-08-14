# Browser Capability V1 — Phase 0 Architecture Analysis

**Baseline:** `main` @ `d3bb347`. Branch `feat/browser-capability-v1`.
**Status:** Phase 0 (read-only audit + driver decision). No fundamental
architecture conflict found — the STOP conditions (§95) are all clear, so
implementation proceeds.

This document is the contract for how a structured Browser Capability lands on
the existing CodeLeveler runtime. It reuses the just-completed Home & Runtime
ownership model; it does not invent a second runtime, permission engine, event
bus, or filesystem namespace.

---

## 1. Current Architecture (the seams a browser plugs into)

| Concern | Authority | Where |
| --- | --- | --- |
| Per-daemon shared services | `Application` struct (`Arc`-shared, process-lived) | `leveler-app/src/lib.rs:121` |
| Long-lived resource ownership precedent | `Application.background_tasks: Arc<BackgroundTaskRegistry>` — built once, cloned into every turn (fixes the "服务活不过一个回合" bug) | `lib.rs:139`, `leveler-execution/src/background.rs:143` |
| Lazy connect-once idiom | `open_database` (`Mutex<Option<T>>` double-checked), `mcp_tools`, `runtime_id: OnceLock` | `lib.rs:335`, `:126`, `:147` |
| Tool contract | `trait Tool` (async `execute(input, ctx, cancel)`) | `leveler-tools/src/tool.rs:333` |
| Shared capabilities into tools | `ToolContext.services: ToolServices` (holds `lsp_sessions`, `artifact_store`, `background_tasks`) | `tool.rs:130` |
| Risk / permission | `RiskLevel` + `ApprovalPolicy::evaluate` | `leveler-execution/src/risk.rs:8`, `approval.rs:605` |
| Turn ownership / disconnect survival | `ActiveTurns` (only `cancel`/`cancel_all` stops a turn; socket EOF does not) | `leveler-app/src/active_turns.rs:17`, regression test `local-transport/src/lib.rs:2396` |
| Events | `EngineEvent` → `EventBridge` → `RuntimeEvent` (per-session broadcast, resyncs on reconnect) | `leveler-engine/src/event.rs:121`, `leveler-app/src/event_bridge.rs:144`, `client-protocol/src/event.rs:39` |
| Path authority | `LevelerHome` + `Layout` (canonical `~/.leveler`) | `leveler-core/src/home.rs`, `leveler-project/src/layout.rs` |
| Structured-IPC subprocess precedent | `McpClient` (newline JSON-RPC 2.0, reader task + id→oneshot map) | `leveler-tools/src/mcp.rs:47` |
| Binary-safe framing precedent | LSP `Content-Length` codec | `leveler-lsp/src/codec.rs:1` |
| Generic tool disclosure | `tool_lines` + `presentation_label` fallback | `leveler-tui/src/tool_cell.rs:582`, `tool_taxonomy.rs:480` |

## 2. Existing Browser Support — NONE (Web UI ≠ Browser Capability)

No browser-automation code exists. `leveler-web` is a **WebUI server** (SPA +
REST + WebSocket bridge to the runtime, loopback + bearer token). Every
"browser" reference in the docs denotes that UI client, not automation:

- `README.md:17` — *"…coding agent runtime with terminal and browser clients…"*
- `README.md:187` — *"| Browser UI (same runtime) | `leveler web` |"*
- `docs/ARCHITECTURE.md:645` — *"leveler-web (browser UI)"*

No `chromium`/`puppeteer`/`webdriver`/`cdp` code anywhere in Rust source. The
only browser-tooling seam is an optional MCP **preset** that scaffolds
`npx -y @playwright/mcp@latest` (`leveler-app/src/mcp_config.rs:365`) — a config
helper, not automation, not wired by default. **A Browser Capability is a
genuinely new capability, cleanly distinct from the Web UI.**

## 3. Driver Options

| Option | Semantic snapshot + refs | Actionability / auto-wait | Chrome | Install model | Verdict |
| --- | --- | --- | --- | --- | --- |
| **A. Playwright (managed Node driver)** | First-class: ARIA/AI snapshot with element refs (`aria_snapshot`/ref-annotated) — *is exactly* §21's format | Built-in actionability checks + auto-wait | system Chrome via `channel:'chrome'`; managed Chromium via `playwright install` | Node + `playwright` pkg + browsers, all user-space under a chosen dir (`PLAYWRIGHT_BROWSERS_PATH`) | **CHOSEN** |
| B. CDP direct (Rust: chromiumoxide / hand-rolled) | None — must build the whole AX-tree→ref→stale→actionability stack ourselves | Must build | Chrome only | pure Rust, no Node | Rejected: reimplements the exact reliability-critical layer §6 warns about; highest risk/time |
| C. WebDriver (fantoccini/thirtyfour) / headless_chrome | None (no semantic ref snapshot) | Weak | needs chromedriver | Rust | Rejected: worse snapshot, extra driver dependency |

## 4. Driver Decision — Playwright, CodeLeveler-owned

**Chosen: Playwright, run as a CodeLeveler-managed driver subprocess that
CodeLeveler fully owns.** Rationale:

- §21's semantic snapshot (`[ref=12] role "name"` + state) **is** Playwright's
  ARIA/AI snapshot; its refs + actionability are the exact reliability-critical
  pieces that would otherwise have to be reinvented over raw CDP. §6 explicitly
  says не to force pure-Rust at the cost of product reliability.
- CodeLeveler owns the Browser **domain** (runtime, refs, tool schema,
  permissions, profile, artifacts, error model). Playwright is only the engine.
  This satisfies §89: MCP/Playwright is a **driver adapter**, never the core.
- **Not** the opaque `@playwright/mcp` server as core (§89): we run a small
  **CodeLeveler-authored driver script** on managed Node + the Playwright
  *library*, so we control the exact primitives, ref semantics, snapshot
  shaping, and error mapping. The existing `@playwright/mcp` preset remains an
  optional user convenience, not V1's architecture.

**The one real tradeoff (recorded):** Playwright has no production-grade Rust
port, so V1 depends on a **Node** runtime. It lives managed, lazy, and
user-space under `runtimes/browser/` — never in the repo, never global, never
the user's Chrome profile. This fits the reserved `runtimes/` namespace exactly
and trips no STOP condition (§95).

**Transport:** `Content-Length`-framed JSON-RPC 2.0 over the driver's
stdin/stdout — request/response + id→oneshot pending-map logic copied from
`McpClient` (`mcp.rs:82–176`), with the LSP `Content-Length` codec
(`leveler-lsp/src/codec.rs`) for binary-safe/large frames (screenshots). Raw
frames never escape the driver adapter; tools return typed domain results.

## 5. Crate Ownership

New crate **`crates/leveler-browser`** owns the domain (no dependency cycle):

```
leveler-browser  (new)
  ├─ domain types: BrowserPageId, BrowserRef, BrowserSnapshot, BrowserNode,
  │                BrowserActionResult, BrowserRuntimeStatus/Info, BrowserError
  ├─ BrowserRuntime  (ensure_ready, discovery, managed install, sessions/pages)
  ├─ BrowserDriver   (Content-Length JSON-RPC transport to the Node subprocess)
  └─ deps: leveler-core (LevelerHome/EnvSnapshot), tokio, serde, fs2
```

- `leveler-tools` depends on `leveler-browser` (via `ToolServices.browser`) and
  implements the thin tool structs in `tools/browser.rs`. The **network gate +
  SSRF check stay in `leveler-tools`** (reuse `web_fetch::is_blocked_ip`), so
  `leveler-browser` carries no policy logic.
- `leveler-app` constructs one `Arc<BrowserRuntime>` on `Application` and wires
  it into `ToolServices`.
- `leveler-project` gains a `Layout::browser_profile_dir()` accessor;
  `leveler-app` passes the resolved profile path to the runtime.

No cycle: `leveler-browser → leveler-core` only; `leveler-tools/app → leveler-browser`.

## 6. Runtime Ownership

`BrowserRuntime` is owned on `Application` (`lib.rs:121`), `Arc`-shared and
lazily initialized — **identical to `background_tasks`** (`lib.rs:139`) and the
`open_database` connect-once idiom (`lib.rs:335`). It is threaded into each turn
via `Application::engine_for_with_profile` → `ToolContext.services.browser`
(`lib.rs:430`, new `.with_browser(...)` next to `.with_background_tasks(...)`),
reaching tools through `Tool::execute`'s `ToolContext`.

The **driver subprocess follows the long-lived-child model** of
`BackgroundTaskRegistry` (`background.rs:143`, `KillOnDrop` `:86`,
`kill_on_drop(true)` + recorded pgid `:244`) — reaped only on daemon process
exit, **never at turn end and never at client disconnect**. It is NOT the
per-command sandbox-scratch model (`run_unix_process_group` kills its tree at
command end).

## 7. Managed Runtime (install)

There is **no** existing "download+install a runtime under home" pattern —
`runtimes/` is reserved and unused. Build it by composing two in-repo idioms:

- **Create-once + atomic finalize** — `runtime_identity.rs:55`
  (`load_or_create_runtime_id`): flock a sibling `*.lock`, re-check under lock,
  write to a temp path, `sync_all`, atomic `rename`.
- **Lease + crash-reaper + no-follow dir creation** — `host_cache.rs`
  (`acquire_sandbox_lease` `:66`, reaper `:81`, `ensure_real_private_chain`
  `:162`, workspace-external guard `:271`).

Install flow: download the pinned Node + `playwright` package + Chromium into a
**temp dir under `runtimes/`**, then atomic-`rename` into
`runtimes/browser/<version>/`, guarded by an `fs2` exclusive flock on a sibling
lock (singleflight; a racing daemon waits and reuses). Corrupt/partial installs
are detected (missing executable / metadata) and safely re-installed. All
user-space: no `sudo`, no `brew`/`apt`, no global `npm`, no project `npm`.
Prefer a **system Node** binary to *run* the install/driver (still installing
the Playwright package + browsers under `runtimes/browser/`); fall back to a
managed pinned Node only when no usable system Node exists.

## 8. System Browser Discovery

Prefer system **Google Chrome**, then compatible Chromium, via Playwright's
`channel:'chrome'` (and `channel:'chromium'`). Discovery is centralized in
`leveler-browser` (not scattered in tools), testable, per-platform:

- macOS: `Google Chrome.app`
- Linux: `google-chrome`, `google-chrome-stable`, `chromium`, `chromium-browser`
- Windows: standard install locations / registry

If no compatible system browser: use managed Chromium (§7). **Even with a system
Chrome binary, always launch with a CodeLeveler-owned `user-data-dir` — never
the user's real profile, never attach to a running Chrome.**

## 9. Profile Ownership

Isolated, durable, **project-scoped** browser profile:
`~/.leveler/state/projects/<id>/browser/profile/` — reached via a new
`Layout::browser_profile_dir()` (mirrors `memory_dir()` `layout.rs:117`, keyed
by `encode_repo_path`). Launched with Playwright's `launchPersistentContext`.

- Project A ≠ Project B (separate profiles).
- Same project, different sessions: may share login/cookies (same profile), but
  active pages/refs are session-isolated (§10).
- Sensitive: never in the workspace, never in git, never dumped to prompt/logs;
  snapshots never return password values; no cookie/storage dump tool in V1.
- Add `LevelerHome::browser_runtime_dir()` for the managed runtime; both are new
  accessors (tripwire forbids raw home-root joins).

## 10. Session Isolation

One daemon, multiple sessions share the **project profile** but not page state.
Refs are scoped to `(session, page, snapshot-generation, element-identity)`.
Session A can never target Session B's page/ref — enforced by the ref carrying
its owning session/page and the runtime rejecting cross-session refs.

## 11. Tool Surface

Registry tools in `leveler-tools/src/tools/browser.rs` (stateless structs
reaching `context.services.browser`):

`browser.navigate` · `browser.snapshot` · `browser.click` · `browser.type` ·
`browser.select` · `browser.press` · `browser.wait` · `browser.tabs` ·
`browser.dialog` · `browser.console` · `browser.screenshot`

Registered in `full_registry()` (`registry.rs:271`) + a `"browser"` arm in
`expand_tool_category` (`registry.rs:317`). Update the registry count test
(`registry.rs:451`) and add TUI taxonomy entries (`tool_taxonomy.rs:518` test).
No `browser.evaluate` (§39). Not added to `read_only_subset` except the `Safe`
reads.

## 12. Snapshot Protocol

Playwright ARIA/AI snapshot → CodeLeveler `BrowserSnapshot` (typed). Semantic,
text-model-friendly, interactive-first:

```
Page  id=page-1  url=http://localhost:3000/users  title="Users"  revision=18
[ref=12] heading "Users"
[ref=13] textbox "Search users"  value="alice"
[ref=14] combobox "Status"  value="Active"
[ref=15] button "Create user"
table:
  [ref=20] row  "Alice"  "Active"
```

Includes role, accessible name, visible text, state (checked/selected/disabled/
expanded/level/value). **Never** full DOM/HTML/CSS/hidden nodes. Password values
are redacted. **Bounded** (§23): `max nodes`, `max chars`, per-node text cap,
depth, interactive-first ordering; on overflow return
`{truncated:true, nodes_returned:N, approximate_total:M}` — never a silent cut.

## 13. Ref Lifecycle

`BrowserRef` binds `session + page + generation + element-identity`. A ref valid
only within its page generation; a structural page change → old ref returns
`BrowserRefStale`, **never** a guessed similar element (no same-text/same-coord
fallback). Playwright's ref identity (per aria-snapshot) backs this; the runtime
tracks the current generation and rejects stale refs. Correctness > ref
longevity. Unrelated live-region/timer updates need not invalidate all refs when
identity is provably preserved. This is a BLOCKER-level invariant (§53).

## 14. Permissions

Reuse the existing taxonomy — no second permission engine:
- `browser.snapshot`, `browser.wait`, `browser.tabs`(list), `browser.console`(read) → `RiskLevel::Safe`
- `browser.navigate`, `browser.click`, `browser.type`, `browser.select`, `browser.press`, `browser.dialog`(accept/dismiss) → `RiskLevel::Network`

`Network` risk means auto under `Assisted`, prompted under `RequestApproval`
(`approval.rs:605`) — the honest floor, matching how MCP tools escape the
sandbox (`mcp.rs:294`). Browser actions can have real external side effects
(submit/delete/purchase) and are gated by the same `ApprovalPolicy`.

## 15. Network Policy

`browser.navigate` must satisfy all three existing layers or it becomes an SSRF
bypass around the current gate:
1. Reject when `context.policy.network_denied()` (`tool.rs:105`; mirror `web_fetch.rs:63`).
2. Reuse `web_fetch::is_blocked_ip` (`web_fetch.rs:188`) on the resolved target
   host — block loopback/private/link-local/metadata; re-check on redirects.
   (`is_blocked_ip` is `pub(crate)`; browser tools live in the same crate.)
3. Carry `RiskLevel::Network`.

Model is deny-private / allow-public (no positive allowlist). **Localhost
nuance:** `web_fetch` blocks `localhost`, but browser dev-verification needs
`http://localhost:3000`. Resolution: browser navigation allows loopback **only**
when the target is an explicit localhost/127.0.0.1 dev URL AND the session is not
`network_denied` — a narrow, documented exception for local dev servers, decided
in the browser tool, never a blanket bypass. (Finalized in Phase 3 with a test.)

## 16. Error Model

Typed `BrowserError`: `BrowserUnavailable`, `BrowserRuntimeInstallFailed`,
`BrowserRuntimeCorrupt`, `BrowserExecutableNotFound`, `BrowserLaunchFailed`,
`BrowserProfileUnavailable`, `BrowserDriverDisconnected`, `BrowserRuntimeCrashed`,
`BrowserActionTimeout`, `BrowserRefStale`, `BrowserPageClosed`,
`BrowserActionFailed`. Tools map these to `Ok(ToolOutput::error(msg))`
(model-visible) vs `ToolError` (infra). Never `"browser failed"`.

## 17. Process Lifecycle

`ensure_ready()` singleflight = in-process `Mutex<Option<Arc<BrowserRuntime>>>`
on `Application` (mirror `open_database`) + cross-process `fs2` flock lease
(mirror daemon election `local-transport/src/lib.rs:573` + `host_cache` lease/
reaper). Driver child is `kill_on_drop(true)` with recorded pgid; crash → next
call returns `BrowserRuntimeCrashed` and the runtime can restart (old refs all
invalid after restart).

## 18. Disconnect / Reconnect

Turn execution is owned by daemon tokio tasks keyed in `ActiveTurns`
(`active_turns.rs:17`); only `cancel`/`cancel_all` stops a turn — socket EOF does
not (`local-transport/src/lib.rs:499`, test `:2396`). Because the
`Arc<BrowserRuntime>` lives on `Application` (daemon-held, not per-connection),
the browser session + driver persist across disconnect; a reconnecting client
resyncs via `snapshot()` + `LiveViews`. This extends Long Task Reliability
(Client ≠ Execution Owner).

## 19. Security Boundaries

S1 profile isolation · S2 session isolation · S3 ref safety · S4 path
containment (profile/artifacts under LevelerHome, cannot escape) · S5 driver
args host-controlled (never model-controlled arbitrary flags) · S6 remote never
directly owns the driver (goes through daemon tool execution — `mcp`/remote
boundary) · S7 permission boundary · S8 no cookie/token/secret in logs or
snapshot · S9 no arbitrary JS · S10 managed install user-space + integrity ·
S11 user Chrome profile never used/attached · S12 Zero Workspace Pollution.
Full checklist gated before Dogfood (§66) and re-reviewed at the end (§87).

## 20. Dogfood Plan

Three categories (§67–71): **A** form/CRUD (search/filter/input/validation/
submit/state/empty-error), **B** SPA/dynamic (client route/async/modal/dynamic
DOM), **C** long-running development with daemon ownership + disconnect/reconnect.
≥1 must be a real external open-source frontend (React/Next/Vue), D3-comparable
complexity. Success = structured browser is the primary verification path,
browser-related shell workarounds ≈ 0, manual intervention 0, false completion 0.
A/B vs the D3 historical signal (Pre-Browser shell/headless churn).

## 21. Explicit NEXT (out of V1 scope)

Firefox/Safari, multi-engine matrix, mobile emulation, extensions, CAPTCHA,
stealth, MFA/OAuth frameworks, download manager, PDF/video understanding, HAR/
full network interception, advanced DevTools, `browser.evaluate` (arbitrary JS),
vision/coordinate control, advanced drag-drop/canvas, remote cloud browser,
replay platform. Recorded as NEXT if surfaced by Dogfood; never expands V1.

---

## STOP-condition check (§95) — all clear

1. modify user repo deps — NO (managed under home) · 2. must use user Chrome
profile — NO (`launchPersistentContext`, isolated) · 3. process owned by TUI —
NO (owned by `Application`/daemon) · 4. sudo/system install — NO (user-space) ·
5. can't use existing permission system — NO (reuses `RiskLevel`/`ApprovalPolicy`)
· 6. refs can't guarantee identity — NO (Playwright ref identity + generation
scoping) · 7. needs a second Agent runtime — NO · 8. breaks Home ownership — NO
(new accessors, canonical paths) · 9. remote boundary bypass — NO · 10. Dogfood
needs unbounded scope — TBD, guarded by §80.

**Proceed to Phase 1 (browser domain types).**
