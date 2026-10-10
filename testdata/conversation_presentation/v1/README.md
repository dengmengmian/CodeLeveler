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
| Exploration receipt | Consecutive read-only calls (read/list/glob/grep/search/lsp) collapse into ONE `读取 N 次 · 搜索 M 次` receipt, collapsed by default and reversible. A single explorer renders as its own compact row. |
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
| C5 | a successful Run is collapsed, names its command and hides its stdout |
| C6 | narration is progress and the turn's last answer is the Final |
| C7 | a runtime-authored row is never the user speaking |
| C8 | a cancelled turn keeps what it saw and claims nothing |
| C9 | a Goal turns its own plan and closes out as a runtime fact |
| C10 | a compacted session reopens with its whole conversation |

Item kinds: `user`, `thought`, `exploration_receipt`, `exploration_row`,
`edit_diff`, `run_receipt`, `assistant_text`, `final_answer`, `runtime_notice`,
`turn_end`. A fixture may also declare `session` (the axis and objective, which
arrive on the session snapshot rather than in the event stream), `plan` (the
plan panel is not part of the transcript), and `forbidden_text`.

An item marked `"optional": true` is a fact a client may express in another
place: the terminal paints a dedicated failure block where the Web and Desktop
state the same failure in their run row and turn terminal. It is asserted where
it exists and never forces the other clients to invent a row.

Every path a fixture declares is compared — `live`, `reconnect` (a snapshot with
the ACTIVE context, then the durable history) and `replay` (durable history
alone). A path that a client cannot serve yet must be recorded as a deferral in
that client's test, never silently skipped.

Each fixture carries `paths.live` (the wire events a client receives) and
`expect.items` (the semantic tree). A client that projects these fixtures to a
different tree is out of contract.

## Who checks itself against this

| Surface | Where |
|---|---|
| terminal (reference) | `crates/leveler-tui/tests/transcript_fold.rs` — the same wire events through the real reducer, asserted against the same JSON, plus the reference behaviour suite for folds, receipts and the always-full diff |
| Web | `crates/leveler-web/web/src/lib/conversationPresentation.test.tsx` — the real reducer through the real bridge (live, reconnect and replay paths), plus the rendered components |
| Desktop | `apps/leveler-desktop/test/conversationPresentation.test.mjs` |

The shared rules live once, in
`packages/conversation-presentation/conversation.mjs`: the Web client and the
Desktop renderer import the same projection, so a fold, a failure line or a
Diff can no longer be decided twice. The terminal keeps its own renderer, which
is the point — the contract is shared, the widgets are not.
