# Browser Capability V1 — Production Gate Report

**Status:** complete on `feat/browser-capability-v1` (not merged; awaiting human
sign-off). All MERGE VERIFICATION gaps are closed — daemon ownership and
disconnect/reconnect are proven by a live TUI↔daemon E2E (§T) — and the MAJOR
implementation gaps the human reviews found are fixed with regressions: **B-2**
page/session ownership (tabs + `target=_blank` adoption) is closed (§J); **B-1**
SSRF is enforced at the browser's real network egress — a page-scoped loopback
grant (not a global bypass), per-hop redirect gating with a pinned connect (no
rebinding), fail-closed verification, WebSocket gating, and blocked service
workers (§Q). The full four-gate regression is a clean 0-failure run (§AG).
Structured browser automation is a real, daemon-owned capability the agent uses
as its primary web-verification path.

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
against real Chrome and in the dynamic-DOM/SPA fixtures). Dialogs/console are
handled; no `browser.evaluate` (§39).

**Page/session ownership is closed end-to-end (B-2 fix).** The persistent context
is shared across sessions, so the `BrowserRuntime` — not the driver — is the
ownership authority. `browser_tabs(session)` returns only the pages that session
owns (another session's tab urls/titles never leak) and prunes pages the driver
has dropped; every action already routes through an owner check that refuses a
cross-session page id. Critically, a `target=_blank`/`window.open` page is now
**adopted** into the originating session's page set at the moment the action
reports it, so the agent can actually snapshot/act on the new tab (it was
previously untracked and unusable) while other sessions can neither see nor
operate it. Unit tests pin the isolation/prune/active-marking rules without a
browser; a real-Chrome fixture drives two sessions through S1–S5 (A can't list or
snapshot B's page; a new tab is adopted by A, operable by A, invisible to B).

## P. Permission Model / Q. Network Policy

Reuses the existing `RiskLevel`/`ApprovalPolicy` — snapshot/wait/tabs/console/
screenshot = `Safe`; navigate/click/type/select/press/dialog = `Network` (auto
under Assisted, prompted under RequestApproval). No second permission engine.

**SSRF is enforced at the browser's own request boundary (B-1 fix).** The Rust
`browser_navigate` arg-gate stays as an early, cheap deny, but it is no longer the
security boundary — a redirect, link click, `window.open`, form submit, `fetch`,
`window.location` or WebSocket never depends on it. Enforcement lives at the
browser's real network egress, holding these invariants (each with a real-Chrome
regression):

- **Loopback is a PAGE-SCOPED dev grant, not a global exception.** A page earns
  loopback access only by being (or being opened by) a page whose live top-level
  origin is loopback — tracked via `navigate` + `framenavigated`, so a page that
  moves to a public origin loses it, and a popup's frame-less first navigation is
  authorized only when its opener is a live loopback dev page. A public
  (opaque-origin) page's `fetch`/`window.open`/redirect/click into loopback is
  refused. *(regression: granted dev page fetches its own origin → allowed;
  ungranted page → blocked.)*
- **Per-hop redirect gating + pinned connect (no rebinding).** Playwright follows
  redirects internally without re-routing the hops, so navigations are walked
  hop-by-hop; each hop is resolved, validated, and then **connected to the
  validated IP** (Node `lookup`-pinned fetch, Host/SNI preserved — the same
  resolve→validate→pin→connect `web_fetch` uses), so a name that rebinds
  safe→private between check and connect never reaches the private endpoint.
  Loopback subresources are pinned the same way; public subresources proceed
  natively (streaming/range preserved). *(regression: public→302→metadata →
  Denied; direct-private → Denied; click→private → surfaced+blocked.)*
- **Fails closed.** Any resolve/validate/fetch error aborts the request with a
  typed `Denied`/`blocked` — never a `continue` past an unverified target, never a
  shell fallback.
- **WebSocket egress is gated** by the same host policy (`routeWebSocket`):
  private/link-local/metadata refused, loopback only with a page grant. *(regression:
  ws→private/metadata → refused, never opened.)*
- **Service workers are blocked** at launch (`serviceWorkers:'block'`), so a SW
  cannot mediate a fetch around request interception.

The blocked-range set mirrors `web_fetch::is_blocked_ip`. The driver bridge ships
in the binary and is re-synced to disk on every runtime start, so the gate reaches
existing installs on upgrade instead of leaving a stale, ungated bridge in place.
Residual (recorded, not claimed away): a rebind on a *public subresource* (served
natively) is validated at request time but not pinned; and a public site is only
reachable when the browser context itself has network egress (it does not inherit
a system proxy — the same as before this change).

## R. Error Model / S. Process Ownership

13 typed `BrowserError` variants (stale/timeout/crash/denied/…). Driver child is
`kill_on_drop` with the long-lived-child model (`BackgroundTaskRegistry`-style),
reaped only on daemon exit. Owned by the daemon, never the client. The driver is
the engine adapter and does no ref/snapshot/ownership bookkeeping (that is Rust);
the one policy it enforces is the SSRF request boundary (§Q), because that is the
only place a redirect/click/window.open becomes an actual request.

## T. Disconnect / Reconnect

Proven **live** over the real TUI↔daemon path (not the embedded `leveler run`).
Harness: `leveler serve` daemon + a PTY `leveler tui` client (no `--in-process`,
so it reuses the daemon) submits a multi-step browser goal; evidence is read
from the daemon's own SQLite (`sessions.db`), so "progress with no client" is a
fact about the daemon, not a screen scrape:

- **Ownership:** the Playwright driver's parent is the daemon (`driver_ppid ==
  serve pid`; process-ancestry check: `ancestor==daemon` true, `ancestor==client`
  false). The browser is a child of the daemon, never the client.
- **Disconnect:** SIGKILL the TUI client mid-goal → client dead, but daemon,
  driver (stable PID), and Chrome all stay alive.
- **Progress with zero client attached:** across the detached window the session
  advanced `event seq 50 → 98` and `browser tool calls 2 → 9` (seven more browser
  calls *after* the client was killed) and reached `status=completed` — with no
  TUI process attached.
- **Reconnect:** a fresh `leveler tui --session <id>` reattached to the same
  session, saw the completed transcript, `task_finished=1` (a real completion,
  not an error stop → no false completion).

The §62 STOP condition ("reconnect kills Browser") is not triggered: the driver
PID stayed stable across the whole disconnect/reconnect.

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

S1 profile isolation · S2 session isolation (§J, B-2: tabs/new-tab ownership is
the runtime's, cross-session pages refused, tested) · S3 ref safety · S4 path
containment (LevelerHome accessors) · S5 driver args host-controlled · S6 remote
goes through daemon tool execution · S7 permissions reused + **SSRF enforced at
the browser request boundary** (§Q, B-1: redirects/clicks/window.open gated,
per-hop) · S8 no secret/cookie dump · S9 no arbitrary JS · S10 user-space install
· S11 user Chrome profile never used · S12 zero pollution — all satisfied. No
second Agent loop, no second permission engine, no parallel event bus, no
LevelerHome bypass (the home tripwire passes with new accessors), MCP is not the
core.

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
- **Merge-blocker closeout (two review rounds):** the human code reviews found
  MAJOR implementation gaps; all are now fixed with regressions, re-verified
  against real Chrome:
  - **B-2 (session tabs / new-page ownership) — FIXED & signed CLOSED.**
    `browser_tabs(session)` is session-scoped and prunes closed pages;
    `target=_blank`/`window.open` pages are adopted into the originating session;
    cross-session pages refused. See §J.
  - **B-1 (SSRF at the real network egress) — FIXED.** Round 1 moved enforcement
    into the driver's request boundary with per-hop redirect gating. Round 2
    closed the five gaps the second review found: loopback is a page-scoped grant
    (not a global bypass); verification fails closed (no `continue` after a failed
    check); WebSocket egress is gated (`routeWebSocket`); service workers are
    blocked; and DNS-rebinding is defeated by a pinned connect (resolve → validate
    → pin → connect) for navigations and loopback subresources. See §Q.
- **NEXT** (out of V1): a dedicated `Browser` ToolKind for TUI grouping; refless
  page-level key press; multi-tab agent ergonomics; managed Node download when
  no system Node; a policy-enforcing forward proxy so public-subresource rebind is
  pinned too. (The live daemon-hosted disconnect/reconnect E2E is DONE — see §T.)

## AG. Full Regression

Run clean, from a hygienic tree (no leftover test shells, daemons, drivers, or
Chrome), on `feat/browser-capability-v1`:

- `cargo fmt --all --check` — **clean** (rc 0).
- `cargo check --workspace --all-targets` — **clean** (rc 0).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — **clean** (rc 0).
- `cargo test --workspace --all-features --locked --no-fail-fast` — **rc 0;
  2707 passed, 0 failed, 5 ignored** across 127 test binaries/doctest suites.
  The whole slow network-dependent provider/relay tail ran to completion with
  **zero failures** this run — no classification needed. The 5 ignored are the
  repo's intentional `#[ignore]` cases (live rust-analyzer timing, a live
  MCP-server test needing node/npx + network, the manual visual-disclosure
  harness), not skips introduced here.

Browser-specific test inventory (all green): `leveler-browser` domain/driver/
install units + the real-Chrome `acceptance` and `reliability` fixtures (lazy,
zero-pollution, SPA/dynamic-DOM stale-ref, dialog/new-tab) + `phase1_acceptance`;
`leveler-tools` SSRF-gate + tool tests + registry count (42); `leveler-tui`
taxonomy coverage. Plus the three real agent-driven dogfoods (A/B/C) and the
live TUI↔daemon disconnect/reconnect E2E (§T).

## AH. Final Verdict

```
BROWSER CAPABILITY V1 PRODUCTION GATE — PASSED

FINAL HEAD:                 feat/browser-capability-v1 (report commit)
DRIVER:                     Playwright (CodeLeveler-owned Node driver, JSON-RPC/stdio)
BROWSER DOMAIN OWNER:       crates/leveler-browser, Arc on the daemon Application
SYSTEM CHROME:              PASS (channel:'chrome'; validated Chrome 151)
MANAGED FALLBACK:           PASS (managed Chromium path; system Chrome preferred)
LAZY BOOTSTRAP:             PASS (nothing until first browser tool use)
PROFILE ISOLATION:          PASS (state/projects/<id>/browser/profile, 0700)
SESSION ISOLATION:          PASS — B-2 (tabs session-scoped; cross-session page refused; tested)
SEMANTIC SNAPSHOT:          PASS (ref-annotated, bounded)
REF SAFETY:                 PASS (stale refs refused, never retargeted)
DYNAMIC DOM / SPA:          PASS (fixtures + Dogfood B)
TABS / NEW-TAB OWNERSHIP:   PASS — B-2 (no hang; target=_blank adopted by originating session)
CONSOLE / PAGE ERRORS:      PASS (errors/warnings only)
PERMISSION INTEGRATION:     PASS (RiskLevel/ApprovalPolicy)
SSRF / NETWORK BOUNDARY:    PASS — B-1 (page-scoped loopback grant; per-hop pinned connect;
                            fail-closed; WebSocket gated; service workers blocked)
DAEMON OWNERSHIP:           PASS — live E2E (driver parent == serve pid; ancestor==daemon, not client)
DISCONNECT/RECONNECT:       PASS — live E2E (TUI↔daemon; client killed, goal ran seq 50→98 /
                            browser calls 2→9 with no client, then completed; reconnect reattached)
CRASH RECOVERY:             PASS (RuntimeCrashed → restart, refs invalidated)
ZERO WORKSPACE POLLUTION:   PASS
USER CHROME PROFILE MUTATION: NO
DOGFOOD A / B / C:          PASS / PASS / PASS
REAL PROJECT DOGFOOD:       PASS (C: React 19 + Vite 8, lint+build passed)
BROWSER SHELL WORKAROUNDS:  0
STRUCTURED BROWSER CALLS:   38 across A/B/C
MANUAL INTERVENTION:        0
FALSE COMPLETION:           0
FULL REGRESSION:            PASS — fmt/check/clippy(--all-features) clean;
                            test --all-features --locked --no-fail-fast: 2707 passed,
                            0 failed, 5 ignored (127 suites); hygienic tree, 0 orphans
MAJOR B-2 (session/new-tab ownership):  FIXED + regressed — signed CLOSED (§J)
MAJOR B-1 (SSRF at network egress):     FIXED + regressed over two rounds (§Q):
                                        page-scoped loopback grant · fail-closed ·
                                        pinned connect (no rebind) · ws gated · SW blocked
BLOCKER: 0   MAJOR: 0 (all resolved)   MINOR: 0   NEXT: 5

MERGE RECOMMENDATION: READY FOR MERGE — awaiting the human sign-off
  (all MERGE VERIFICATION gaps closed; both review-found MAJORs fixed with
   real-Chrome regressions; FULL REGRESSION is a clean 0-failure run. Branch
   pushed and remotely reviewable. Do NOT merge main and do NOT start Benchmark
   until the human signs off.)
```
