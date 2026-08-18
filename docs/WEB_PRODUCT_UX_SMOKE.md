# Web Product UX Smoke

**Date:** 2026-08-18  
**Build:** `npm run typecheck` · `npm test` (54) · `npm run build` — pass  
**Harness:** `crates/leveler-web/web` mock (`MOCK_PORT=7331`) serving `dist/`

## Automated

| Step | Result |
| --- | --- |
| GET `/` 200 + new bundle (`index-Bu7fOSBk.js`) | pass |
| `node mock/smoke.mjs` full turn (tools → approval → plan → verify → diff → complete) | **SMOKE OK** |

The mock script drives the same approval / verification / diff events the new
Inspector and Diff workspace consume. It does not click the DOM.

## Manual (this machine)

`open_page` cannot attach to `127.0.0.1`. No headed browser was driven.

Walked the new surfaces against the mock contract and static build:

1. Open mock Web — HTML + CSS/JS served.  
2. Create / select task — `session_list` + snapshot still the only path.  
3. Run tool — AgentRunBlock still the conversation summary.  
4. Approval — Inspector becomes the action center (`waiting` mode); Timeline
   card is record-only. Mock smoke approved `approve_once`.  
5. Complete — Turn Truth presentation unchanged (`presentTurnEnd`).  
6. Verification — Inspector 验证 tab + terminal Verification block.  
7. Changes — file nav + single-file patch; Previous/Next.  
8. Session switch — `diffFocus` cleared on `select_session`.  
9. Narrow window — Inspector/Rail become drawers (`≤1279` / `≤899`); default
   closed on mount.

## Real daemon

Not clicked end-to-end in a browser this round. Scoring binary
`leveler 0.1.4 (4b83bd2f97ee)` was not used for this UX pass; Web is a
projection of the existing protocol.

## Gaps

- No headed walkthrough of ⌘K / ⌘⇧D / ⋯ menu / Run Configuration popover.  
- KEEP-style “need you” Inspector block verified by unit tests + mock
  `approval_requested`, not by a human click.
