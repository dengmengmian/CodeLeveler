# Gate 6 · Browser Capability Closure — Audit

**Result: NO_PRODUCT_CHANGE.** The Final Review's `CLOSURE_ONLY` verdict is confirmed by audit; nothing in Batch #1's evidence justifies a change here, and a rewrite is explicitly refuted.

## Capability inventory (12 structured tools at `c3bf11b`)

`browser_navigate` · `browser_snapshot` · `browser_click` · `browser_type` · `browser_select` · `browser_press` · `browser_wait` · `browser_tabs` · `browser_dialog` · `browser_console` · `browser_screenshot` · `browser_drag`

Every capability the Beta coding workflow was measured needing is present, including the two that earlier gates added in response to real gaps (`browser_drag` from R004-F5; `browser_dialog`/`browser_tabs` for multi-surface flows).

## Evidence

| Run | Calls | Workarounds | Utility |
| --- | --- | --- | --- |
| R006 | 22 | 0 | POSITIVE |
| R007 | 72 | 0 | MIXED — app never booted |
| R007b | 19 | 0 | NEGATIVE — app never booted |
| R010 | 47 | 0 | POSITIVE — real UI verified, `browser_console` used to assert no page exceptions |

**~160 structured calls, zero shell/Playwright/Puppeteer bypasses.** The R004-era workaround pattern is gone. `leveler-browser` tests: 29/29 green (17 unit + 1 acceptance + 11 reliability, including SSRF boundary, loopback dev grant, stale refs and tab scoping).

## The two negative cases are not Browser defects

R007 and R007b both failed for the same reason — a dev server that never booted in a large monorepo — and both produced zero evidence despite the Browser itself functioning. That is a Long-Goal environment problem (Gate 3's territory), not a capability gap. Separating **capability maturity** from **task utility** is what prevents a wasted rewrite here.

## Deliberately not built

No download/upload, file chooser, new-window handling, or cookie manager. The gate's rule is that a capability needs evidence, and no run in the batch needed any of these. Adding them because "browser products usually have them" is precisely the speculative work the Beta Closure Program forbids.

App-boot readiness observability was considered and rejected for this gate: it belongs to goal-owned service lifetime (Gate 3), and pushing dev-server orchestration into `BrowserRuntime` would put it in the wrong layer.

## Residual

`R004-F6` (vite websocket through the forward proxy) stays `OPEN_EVIDENCE_NEEDED`: it was fixed as a grant-scoped loopback ws, but the two later browser tasks that would have exercised it never booted their apps, and R010's app needed no websocket. **Not re-observed is not re-proved** — it is recorded as unproven rather than quietly closed.
