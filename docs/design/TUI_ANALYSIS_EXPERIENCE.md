# TUI Analysis Experience

**Status:** implemented · 2026-08-26 · **No runtime behavior changed**

Makes model reasoning a first-class, visible part of the conversation —
CC-style `Analysis → Tool → Analysis → Final answer` — and makes tool
disclosure click-first.

## Why Analysis is product content

The conversation carries three semantically different things, and the UI must
not collapse them into one visual language:

| Surface | Answers | Presentation |
| --- | --- | --- |
| **Analysis** | *why* the agent is doing something | visible, muted, labeled `分析` |
| **Tool activity** | *what* the runtime executed | collapsed disclosure, click to expand |
| **Assistant** | what the agent tells the user | primary prose |

Before this change, reasoning rendered as:

```
▸ 思考 · 23 lines  Ctrl+O
```

— collapsed, gated behind a shortcut, and **erased by the next step**
(`reasoning_superseded` replaced each step's thought with the next). The user
could never read back why the agent chose an action. Verified on a real PTY
run: 21 of 94 frames showed the collapsed row; the content itself was never
visible without Ctrl+O, and history never survived.

After:

```
● 分析

先确认 program 的机器契约。
Runtime 把它当必填，但 schema 没有声明 required。

▸ 搜索了代码库

● 分析

找到根因了。program 是 Option<String>，schema 把它当可选。

▸ 读取了 3 个文件

● 最终回答
…
```

## Representation

One typed transcript item, one source of truth:

```rust
TranscriptItem::Analysis(AnalysisBlock { text: String, done: bool })
```

The previous triple state (`AppState.reasoning` + `reasoning_superseded` +
`reasoning_expanded`) is **deleted**, not wrapped. Reasoning now lives only in
the transcript; the status line's thinking-token estimate reads
`transcript.live_analysis()`. There is no second buffer to drift.

Cache safety comes free: `push_reasoning_delta` bumps `transcript.version`,
which is already the conversation cache key.

## Segmentation rule

```
ReasoningDelta      → append to the live Analysis block, or start one
                      (a block is live iff it is the LAST transcript item
                       and not sealed)
tool call starts    → seal
assistant starts    → seal
turn ends / fails / cancels → seal
```

Segmentation falls out of transcript ordering: once a tool group is pushed,
the Analysis block is no longer last, so the next delta starts a new block
below the tool. `Analysis / Tool / Analysis` needs no bespoke state machine.
A defensive sweep in `push_reasoning_delta` seals any stale open block, so a
missed seal point cannot wedge scrollback finality.

### Segmentation is not completion

`done` means exactly one thing: *another item began after this text*. It is
never rendered as `✓ 分析完成` or `⚠ 分析未完成` — nothing in the runtime
measures whether a reasoning segment "completed", and absence of further
deltas is not proof of anything. A test asserts no completion glyph or wording
is rendered for either state.

On failure or cancellation the emitted text stays, sealed, exactly as written.
The turn-end marker carries the outcome; the analysis is a separate fact.

## Density

Measured, not guessed: typical segments run 10–25 lines (23 observed live),
but this project has recorded single rounds of **6.7k reasoning tokens**.
Unbounded rendering would bury the conversation under one thought.

A block renders its **tail** — the last 16 wrapped lines — under a truthful
marker:

```
  … 前面还有 37 行分析
  <last 16 lines>
```

The tail is the decision-relevant end of a segment and is also what a live
block needs (latest lines at the live edge, auto-follow unchanged). Hidden
lines are counted honestly; nothing is silently dropped and presented as
complete. There is no summarizer and no prose parsing.

## Durability boundary

Analysis is presentation, not truth:

- no EventLog persistence was added; `ReasoningDelta` remains transient
- nothing parses analysis prose into findings, counts, or completion facts
- `EvidenceLedger` / `FindingRecord` / tool results / turn state remain the
  only authoritative sources
- **historical Analysis does not survive a process restart.** Accepted and
  documented; persisting it would create a durable chain-of-thought contract
  this phase explicitly refuses.

## Click-first disclosure

Tool detail interaction is the disclosure row itself: `▸` collapsed, `▾`
expanded, click to toggle — the glyph already communicates interactivity.

Visible `Ctrl+O` copy is removed from disclosure rows and results:

| Was | Is |
| --- | --- |
| `▸ 思考 · 14 lines  Ctrl+O` | gone (analysis is visible content) |
| `… 还有 {} 行 · Ctrl+O 展开` | `… 还有 {} 行` |
| `(+{} 行 · Ctrl+O)` | `(+{} 行)` |
| `… Ctrl+O 查看完整 Diff` | `… 点击展开完整 Diff` |
| ` · Ctrl+O` on collapsed results | gone |

`Ctrl+O` itself remains implemented as an undocumented fallback that toggles
the **latest tool group only** (its reasoning branch is gone with the state it
toggled). The keyboard help screen keeps one truthful line for it; inline UI
no longer advertises it. No new keyboard navigation was added.

## What was deliberately not done

- No `ToolOutputDelta` / tool streaming — **DEFERRED**. Agent tools still
  expose only `ToolCallStarted` / `ToolCallCompleted`. If real use shows
  "tool name + elapsed" is not enough to judge progress, the design direction
  is a bounded / ephemeral / lossy live-output channel — not stuffing stdout
  into the canonical EventLog.
- No reasoning persistence, no analysis collapse interaction, no semantic
  labels beyond `分析`, no changes to `DisclosureClass` or collapse semantics.

## Known limitations

- Analysis history is session-local (see durability boundary).
- A block longer than 16 lines hides its head behind the honest marker with
  no way to expand it in this phase.
- Whether reasoning appears at all depends on the model emitting
  `reasoning_content`; with thinking disabled the conversation simply shows
  no Analysis blocks, which is truthful.
