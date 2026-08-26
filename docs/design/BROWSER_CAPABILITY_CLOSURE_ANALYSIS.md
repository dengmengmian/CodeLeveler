# Browser Capability Closure — Engineering Gate audit

**Status:** Engineering Gate evidence · 2026-08-27 · verified against `ca406985`

Repository-level audit of the Browser capability, re-verified at `ca406985`
(the Browser crates have **zero commits** since the original `b94c8a28` audit,
so every architectural claim below was re-read against current code and holds). Classifications follow the gate's vocabulary; every claim
below was read from the current source, not from prior reports.

## Architecture, as it actually is

```
leveler-tools/tools/browser.rs      12 structured tools (thin adapters)
        ↓
leveler-browser/runtime.rs          ownership authority: sessions, pages,
                                    generations, semantic refs
        ↓
leveler-browser/driver.rs           one Node child, kill_on_drop + own
                                    process group (killpg on shutdown)
        ↓
driver/browser_driver.mjs           Playwright; forward-proxy egress gate,
                                    page registry, console/dialog per page
```

One runtime, one driver, one page registry. No second browser system exists,
and none is needed.

## Capability classification

| Capability | Class | Evidence |
| --- | --- | --- |
| navigate / snapshot / click / type / select / press / wait / tabs / dialog / console / screenshot / drag | EXISTING_AND_PROVEN | 12 tools registered; Gate 6 recorded ~160 real structured calls, 0 bypasses; suite green at audit time |
| Semantic refs + generation stamping | EXISTING_AND_PROVEN | refs carry `generation`; `interact` validates against the page's current generation; `spa_route_and_dynamic_dom_keep_refs_correct` |
| Stale-ref refusal | EXISTING_AND_PROVEN | mismatched generation → explicit error, never a guess |
| Session/tab ownership | EXISTING_AND_PROVEN | `own_page` refuses cross-session access; `owned_tabs` filters; unit-tested pure |
| SSRF / egress gate | EXISTING_AND_PROVEN | fail-closed forward proxy; DNS-rebind refused; loopback is a page-scoped grant kept in lockstep with live origin via `framenavigated`; ws gated |
| Immediate popup adoption | EXISTING_AND_PROVEN | driver waits 800 ms for `popup`; runtime binds `newPage` to the clicking session; `sessions_and_new_tabs_are_owned_by_the_originating_session` |
| Process cleanup | EXISTING_AND_PROVEN | `kill_on_drop` + process-group `killpg(SIGTERM→SIGKILL)`; lazy bootstrap creates nothing until used |
| **Delayed popup (> 800 ms)** | KNOWN_LIMITATION — see below | read from `browser_driver.mjs:641-655` + `runtime.rs:361-377` |
| **Cancellation of an in-flight action** | KNOWN_LIMITATION | tools ignore the token (`_cancel`); bounded by `ACTION_TIMEOUT`, so cancel waits ≤ one action timeout |
| **Structured browser-verification fact** | REAL_GAP (scoped) | see "Completion truth" below |
| vite ws through proxy (R004-F6) | EXISTING_BUT_UNPROVEN | fixed as grant-scoped loopback ws; never re-observed by a real task since — Gate 6's "not re-observed is not re-proved" stands |
| Download/upload, file chooser, cookies, evaluate(), network interception, device emulation | OUT_OF_SCOPE | deliberately not built; no run has needed them |

## The delayed-popup boundary, precisely

The click path waits **800 ms** for a `popup` event. Within the window, the
popup id returns as `newPage` and the runtime binds it to the clicking
session — proven by the ownership test.

After the window:

- the driver's `context.on('page')` still registers the page (console buffer,
  dialog policy, loopback lockstep, close cleanup all attach), **but the
  runtime never learns its id**;
- no session can reach it: `own_page` refuses it as unknown — **fail-closed,
  not fail-open**. No cross-session leak is possible;
- `browser_tabs` never lists it; shutdown's `killpg` reaps it with everything
  else, so no process or profile outlives the runtime;
- the click result carries `newPage: null`, which is **indistinguishable from
  "no popup opened"**.

So the failure mode is a **silent miss**, not an orphan and not an ownership
violation. Per the gate's own standard (§20) this is outcome A —
KNOWN_CAPABILITY_LIMITATION — *provided* repeated trials show the 800 ms
window is reliable for ordinary same-machine popups. That is what the
reliability runs must establish with a denominator; a single green run does
not.

## Completion truth

Browser results do **not** feed the completion verdict. `Verified` is
produced solely by engineering gates over a mutated workspace (K19), and the
completion report never claims "browser-verified".

Consequence, stated honestly in both directions:

- a browser failure cannot silently *become* `Verified` — the invariant holds
  by construction, because the verdict never speaks about the browser;
- but there is also **no structured fact that says "the change was verified
  in the browser"**. A frontend fix whose UI verification failed while unit
  tests passed reports `Verified` — true about the gates, mute about the UI.
  The dogfood record must therefore be built from browser tool results, and
  any "browser verification" cell in the evidence tables below comes from
  those, never from prose.

Classified REAL_GAP but **scoped**: closing it means a completion-evidence
change, which is not a Browser-crate concern and must not be smuggled into
this gate. Recorded for the Beta gate discussion.

## Dev-server lifecycle (audited, Gate 3 territory)

The browser layer neither starts nor owns app servers. Agents start them via
`run_command background=true`; those are session-owned background tasks with
`KillOnDrop` + `kill_scope`, reaped at the engine's terminal settlement. No
broad process killing exists in the production path, and none will be added.
Readiness detection is the agent's job (poll/wait) — the R007/R007b failures
were app-boot failures, not browser defects, and that boundary stays.

## Engineering Gate results (2026-08-27)

Every number below came from a run, not from reading code.

### The instrument was validated first

`reliability.rs` skips silently when node/npm/Chrome are unavailable
(`ready_runtime()` returns `None`). A green suite could therefore mean
"nothing ran". Checked explicitly: **0 skips**, all 11 reliability tests
exercised real Chrome. Full crate: **29/29**.

### Repeated reliability, with a denominator

| Scenario | Trials | Pass | Fail |
| --- | --- | --- | --- |
| `sessions_and_new_tabs_are_owned_by_the_originating_session` (the historical flake) | 30 | 30 | 0 |
| `dialog_and_new_tab_do_not_hang` (popup) | 30 | 30 | 0 |

Serial, one process at a time — no shared-profile or port contention.
macOS 15.5 / Apple Silicon / system Chrome.

### Ownership and cleanup, measured on a live tree

A real dogfood daemon owning a Next dev server, a Playwright driver and
headless Chrome. Killing **only** that daemon reaped: `next dev`,
`next-server`, `browser_driver.mjs`, Chrome — **zero survivors**. No broad
process matching was used at any point.

### The defect this gate found

Not in the Browser crate. The Browser workflow was blocked from verifying
its own fix by the executor's no-progress loop guard:

```
browser_navigate  → ok
browser_snapshot  → "already ran 2 times … made no progress"   REFUSED
browser_type ref  → "browser ref is stale (page generation 5)"
browser_snapshot  → REFUSED
browser_navigate  → REFUSED
```

The guard counts only byte-identical results, but **refuses predictively**,
before the call runs. A refused call never executes, so it never records new
content and never clears its entry — a permanent lockout for the rest of the
turn. `browser_snapshot` takes no arguments, so its key is constant, and a
fresh snapshot is the **only** documented recovery from a stale ref. Fixed in
`2c609bf4` (novelty epoch); re-running the same dogfood produced **0**
no-progress refusals and a stale ref recovered by re-snapshotting.

### Real frontend workflows

| # | Task | Before-fix repro | Post-change browser verification |
| --- | --- | --- | --- |
| A | password-visibility toggle stuck open | ✅ type + click + snapshot | ✅ 7 snapshots, 5 clicks, stale ref recovered |
| B | dropdown not closing on outside click | ✅ | ✅ menu items enumerated verbatim; container `ref=3e678` gone from the DOM after the outside click |
| C | new "Total orders: N" indicator | ✅ absent at generation 2 | ✅ `paragraph [ref=10e177]: "Total orders: 5"` at generation 10 |

All three on TailAdmin (Next.js), real model, real dev server, real Chrome.

### Boundaries the gate revealed (environment, not Browser)

- **Next dev cold-compile race.** Navigating before a route has compiled
  returns a blank shell; the snapshot truthfully shows an empty tree, and the
  agent must wait. The first B attempt failed exactly here and passed on a
  warm cache. `browser_wait` exists and was not used.
- **Turbopack `EMFILE`** on this machine 404s every route until the watcher
  is changed (`WATCHPACK_POLLING=true` + webpack). The agent diagnosed and
  worked around this itself.
- **Unattended driving needs `--auto-approve`.** Without it a PTY session
  stalls on the approval overlay. This is a harness-usage fact, not a defect.

### Audit-record limitation

The EventLog stores a **truncated** tool preview. It is enough to reconstruct
which browser calls ran and whether they errored, but not to read a full
snapshot back — post-change verification had to be re-established from the
model's own quoted snapshot lines. For future gates, browser evidence should
be quoted into the answer, not mined from previews.

## Plan for the remaining phases

1. Reliability with denominators: full `leveler-browser` suite once, then the
   ownership/popup pair repeated ≥ 30× with counts recorded.
2. Dogfood on a real Next.js app (TailAdmin, the repo R010 already proved
   boots): Task A form/state bug, Task B navigation/modal flow, Task C small
   visible feature, plus one deliberate-failure scenario.
3. Fix only what the evidence demands; classify everything else.
