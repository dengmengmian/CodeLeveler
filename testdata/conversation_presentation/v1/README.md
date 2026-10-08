# Conversation Presentation Contract v1

The conversation's semantic tree, frozen as fixtures. One contract, three
renderers: the terminal is the reference implementation, and the Web and
Desktop clients project the SAME runtime facts onto the SAME tree. Nothing here
names a glyph, a colour or a widget — only what the reader must be able to see.

```
User → Thinking (live) → Thought (completed, folded) → Exploration receipt
     (collapsed, reversible) → Tool row → Run (collapsed; failure visible)
     → Confirmed Diff (always in full) → Narration → Final
```

The order is the order the facts arrived. A presentation may fold and group;
it may not reorder, rewrite or invent.

## The items

| Item | The contract |
|---|---|
| User | The user's own line, exactly once, in its place. |
| Thinking (live) | `思考中…` plus the body being written. Visible while the model is thinking. |
| Thought (completed) | ONE folded `思考 · Ns` block per runtime reasoning segment; `Ns` is the runtime's measurement. The body comes back on demand. Never assistant prose. |
| Exploration receipt | Consecutive read-only calls (read/list/glob/grep/search/lsp) collapse into ONE `读取 N 个文件 · 搜索 M 次` receipt, collapsed by default and reversible. A single explorer renders as its own compact row. |
| Tool row | The runtime's own status. No lifecycle noise (`Tool started`, `等待中`). |
| Run (collapsed) | A command's output is not painted by default; its failure IS: status, exit code and the first line that reports the failure, chosen by the reference's rule. A runtime execution row is never the reason. |
| Confirmed Diff | The runtime's `applied_diff`, displayed **in full, directly**: no collapse, no click, no preview budget, no row cap, no `… +N lines`, no diffstat-only summary. The requested patch is a different fact and is never a substitute. |
| Narration | Assistant text, verbatim, no card and no invented stage. |
| Final | The answer the turn committed, complete. |
| Runtime notice | The runtime's own note, never painted as something the user typed. |

## Fixtures

| id | proves |
|---|---|
| C1 | a confirmed edit is displayed in full, with no click |
| C2 | consecutive exploration is one collapsed, reversible receipt |
| C3 | a completed Thought is folded, with the runtime's own duration |
| C4 | a failed Run shows its failure without expanding anything |

Each fixture carries `paths.live` (the wire events a client receives) and
`expect.items` (the semantic tree). A client that projects these fixtures to a
different tree is out of contract.

## Who checks itself against this

| Surface | Where |
|---|---|
| terminal (reference) | `crates/leveler-tui/tests/transcript_fold.rs` — the same wire events through the real reducer, asserted against the same JSON, plus the reference behaviour suite for folds, receipts and the always-full diff |
| Web | `crates/leveler-web/web/src/lib/conversationPresentation.test.tsx` — the real reducer through the real bridge, plus the rendered components |
| Desktop | `apps/leveler-desktop/test/conversationPresentation.test.mjs` |

The shared rules live once, in
`packages/conversation-presentation/conversation.mjs`: the Web client and the
Desktop renderer import the same projection, so a fold, a failure line or a
Diff can no longer be decided twice. The terminal keeps its own renderer, which
is the point — the contract is shared, the widgets are not.
