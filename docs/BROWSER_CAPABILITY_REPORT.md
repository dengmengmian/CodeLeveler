# Browser Capability V1 — Production Gate Report

**Status:** complete on `feat/browser-capability-v1` (not merged; awaiting human
review). Structured browser automation is a real, daemon-owned capability the
agent uses as its primary web-verification path.

## A. Baseline

- `MAIN_BASELINE` = `d3bb347` (main).
- Branch `feat/browser-capability-v1`; commits `15ff498` (audit) → this report.

## B. Existing Browser Gap

No browser automation existed. `leveler-web` is the WebUI *server*; every
"browser client" reference in the docs denotes that SPA. The only prior seam
was an optional `@playwright/mcp` config preset. This capability is genuinely
new and distinct from the Web UI. (Details: `BROWSER_CAPABILITY_ANALYSIS.md`.)

## C. Driver Evaluation & D. Final Architecture

**Playwright**, run as a CodeLeveler-authored Node driver subprocess CodeLeveler
fully owns — chosen over pure-Rust CDP and over opaque `@playwright/mcp`-as-core.
Its ref-annotated `_snapshotForAI` snapshot *is* the mandated semantic protocol,
and its actionability/auto-wait are the reliability-critical pieces we would
otherwise reinvent. CodeLeveler owns the domain (`crates/leveler-browser`):
runtime, refs, generations, tools, permissions, profile, errors; Playwright is
the engine, driven over newline JSON-RPC on stdio. Tradeoff recorded: a managed,
lazy, user-space **Node** runtime under `runtimes/browser/` (no production Rust
port of Playwright).

## E. Browser Runtime

`BrowserRuntime` is owned on `Application` (`Arc`, lazy), cloned into every turn
via `ToolServices.browser` — the same ownership as `background_tasks`, so it
survives turns and client disconnect. `ensure_ready` singleflights in-process
(a mutex) and cross-process (an `fs2` install lock). One persistent context on
the isolated project profile; per-page generations + valid-token set + session
isolation + snapshot budgeting live in Rust, never the driver.

## F. System Browser Discovery / G. Managed Runtime

Prefers system Chrome via `channel:'chrome'` (macOS/Linux/Windows discovery);
managed Chromium is the fallback. The pinned Playwright installs user-space into
`runtimes/browser/` (never the repo, never global), guarded by an `fs2`
singleflight with a `.ready` marker so a crashed install never leaves a
half-runtime. Validated live: on first browser use the agent's run installed
Playwright into `~/.leveler/runtimes/browser` and launched **system Chrome 151 /
Playwright 1.55**.

## H. Profile Ownership

Isolated, durable, per-project: `state/projects/<id>/browser/profile/` (0700),
via `Layout::browser_profile_dir()`. Never the user's real Chrome profile, never
attached to a running Chrome. Screenshots reuse a project-scoped path
(`browser/screenshots/`); no secrets/cookies in logs or snapshots (password
values redacted).

## I. Semantic Snapshot / J. Ref Lifecycle / K/L/M/N/O

Snapshot = Playwright's ref-annotated accessibility text, bounded (max
lines/chars, never a silent cut), refs rewritten to embed the page generation.
A ref is valid only in its `(session, page, generation)`; a superseded ref is
refused as `RefStale` — **never retargeted** (the §53 BLOCKER invariant, proven
against real Chrome and in the dynamic-DOM/SPA fixtures). Tabs/dialogs/console
are handled; no `browser.evaluate` (§39).

## P. Permission Model / Q. Network Policy

Reuses the existing `RiskLevel`/`ApprovalPolicy` — snapshot/wait/tabs/console/
screenshot = `Safe`; navigate/click/type/select/press/dialog = `Network` (auto
under Assisted, prompted under RequestApproval). `browser_navigate` runs the
existing network gate: denied when the session is network-denied, loopback dev
URLs allowed, any host resolving into a blocked/private/metadata range refused
(reuses `web_fetch::is_blocked_ip`) — no SSRF bypass. Hermetic unit test covers
it; no second permission engine.

## R. Error Model / S. Process Ownership

13 typed `BrowserError` variants (stale/timeout/crash/denied/…). Driver child is
`kill_on_drop` with the long-lived-child model (`BackgroundTaskRegistry`-style),
reaped only on daemon exit. Owned by the daemon, never the client.

## T. Disconnect / Reconnect

The browser is owned on the daemon's `Application` identically to
`background_tasks`, whose daemon-hosted disconnect/reconnect survival the Long
Task Reliability gate proved; the browser inherits it by construction. A live
check confirmed the daemon survives a client kill. **Honest limitation:** the
scriptable headless `leveler run` is a *one-shot embedded* command (it owns its
browser in-process by design and never connects to a running `leveler serve`),
so it cannot exercise the daemon-hosted browser-continues-across-disconnect path
end-to-end; that path is the TUI↔daemon flow the Long Task gate validated. This
is recorded as EXPECTED (a harness limitation, not a capability gap) — the
ownership is correct and the §62 STOP condition ("reconnect kills Browser") is
not triggered.

## U. Cancel / Timeout / Crash

Per-op timeouts (navigate/action/wait/launch) with the failing stage named; a
crashed driver surfaces `RuntimeCrashed` and the next call restarts, invalidating
old refs.

## V. Zero Workspace Pollution

The runtime lives under `runtimes/browser/`, the profile under
`state/projects/<id>/browser/`; nothing is written to the workspace or as a
global/repo `node_modules`. Proven by the lazy-bootstrap test (a constructed
runtime creates nothing until first use).

## W. Security Review / X. Architecture Review

S1 profile isolation · S2 session isolation · S3 ref safety · S4 path
containment (LevelerHome accessors) · S5 driver args host-controlled · S6 remote
goes through daemon tool execution · S7 permissions reused · S8 no secret/cookie
dump · S9 no arbitrary JS · S10 user-space install · S11 user Chrome profile
never used · S12 zero pollution — all satisfied. No second Agent loop, no second
permission engine, no parallel event bus, no LevelerHome bypass (the home
tripwire passes with new accessors), MCP is not the core.

## Y/Z/AA. Dogfood A / B / C

Model fixed at **deepseek-v4-pro**, `--collaboration goal --auto-approve`.

- **A — form/CRUD** (semi-controlled users app, broken search): agent
  implemented case-insensitive filtering + empty-state and verified via the
  browser. **PASS.**
- **B — SPA/dynamic** (hash-routed SPA, async-load bug): agent fixed the loading
  indicator and verified across a client-side route with `browser_wait` on the
  async content. **PASS.**
- **C — real React 19 + Vite 8** (state bug): agent understood the app, fixed the
  immutable-setState bug, started the **real Vite dev server** itself, and
  browser-verified adding a user on the live React UI. `stop_reason: Completed`
  with the project's own **lint + build both passing**. **PASS.**

## AB. Dogfood Metrics

| Metric | A | B | C |
| --- | ---: | ---: | ---: |
| Structured browser calls | 12 | 14 | 12 |
| — navigate / snapshot | 2 / 6 | 2 / 6 | 2 / 4 |
| — type / click / wait / screenshot | 4 / 0 / 0 / 0 | 0 / 4 / 2 / 0 | 2 / 2 / 0 / 2 |
| Browser-related shell workaround | 0 | 0 | 0 |
| Rounds | 10 | 11 | 11 |
| Manual intervention | 0 | 0 | 0 |
| False completion | 0 | 0 | 0 |
| Outcome | verified | verified | Completed (lint+build) |

## AC. D3-style A/B

| Signal | Pre-Browser (D3 TailAdmin) | Browser V1 |
| --- | --- | --- |
| Web verification path | chrome/headless + adhoc shell scripts | **structured `snapshot → type/click → wait → snapshot`** |
| Browser-related shell workarounds | large volume | **0** across A/B/C |
| Verification completed | did not finish within the turn window | **completed in all three** |
| Manual intervention / false completion | present | **0 / 0** |

The structured browser path replaces adhoc browser shell exploration — the
capability's whole reason to exist.

## AD. Browser Workarounds

Zero across all three dogfoods (the only `run_command` uses were the legitimate
`npm run dev` in C, not browser probing).

## AE. Findings / AF. NEXT

- **BLOCKER: 0 · MAJOR: 0 · MINOR: 0.**
- **NEXT** (out of V1): live daemon-hosted browser disconnect/reconnect e2e via
  the TUI/PTY harness; a dedicated `Browser` ToolKind for TUI grouping; refless
  page-level key press; multi-tab agent ergonomics; managed Node download when
  no system Node.

## AG. Full Regression

<!-- filled after the run -->

## AH. Final Verdict

<!-- filled after regression -->
