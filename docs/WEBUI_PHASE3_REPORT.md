# WebUI Phase 3 — Engineering Workflow Alignment

**Date:** 2026-08-19  
**Scope:** Productize Changes / Verification / Completion Truth / Artifact
flow. No EventLog, QueryObservability, protocol, Agent Graph, or Replay
changes.

---

## 1. Implemented

| Piece | What |
| --- | --- |
| Completion Truth | `lib/completionTruth.ts` — one projection for Conversation, Changes, Inspector |
| Changes workspace | File groups Modified / Added / Deleted; truth strip; next actions |
| Execution → Changes | Click a timeline path **only if** it matches a current `UiDiffFile.path` |
| Inspector Result | Status, Trust, Artifacts from the same truth |
| Conversation | `AgentRunBlock` terminal facts use Completion Truth (same labels as Inspector) |

---

## 2. Data flow

```
lastTurn + verification + diff + pending waiters
        ↓
completionTruth()
        ↓
Conversation AgentRunBlock
Changes workspace header/footer
Inspector Result / Trust / Artifacts
```

```
Execution step.detail  --exact/unique path-->  focus_diff  -->  DiffView
```

No copied artifact store. Diff still comes from `snapshot.diff` /
`diff_updated`. Verification still `UiVerification`. Approvals still
Inspector-only actions.

---

## 3. Files changed

- `web/src/lib/completionTruth.ts` + `.test.ts`
- `web/src/lib/changeFiles.ts` + `.test.ts`
- `web/src/components/DiffView.tsx`
- `web/src/components/ExecutionView.tsx`
- `web/src/components/Inspector.tsx`
- `web/src/components/AgentRunBlock.tsx`
- `web/src/app.css`
- `docs/WEBUI_PHASE3_REPORT.md`

---

## 4. Validation

```
npm test          81 passed
npm run typecheck PASS
npm run build     PASS
```

Headed browser not run in this environment.

---

## 5. Remaining gaps

- **`UiDiffFile` has no kind field.** Added/deleted/modified is inferred from
  patch (`--- /dev/null` / `+++ /dev/null`) or add/del counts. Binary / empty
  patch files fall back to `modified`. Not a protocol change.
- **Execution → Changes** only jumps when observatory `target` equals (or
  uniquely suffixes) a path already in `diff.files`. Truncated targets or
  tools without a path do not invent a jump.
- **No Accept / Reject file command** on the wire. Changes footer can open
  Inspector or retry; it cannot accept a subset of files.
- Agent Graph / Replay remain Phase 4.
