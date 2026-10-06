# Execution Presentation Contract v1

This document freezes CodeLeveler's **execution presentation semantics** so that the
TUI, Web, Desktop and App derive the same semantic tree from the same runtime facts.
It is not a visual spec: color, spacing, glyphs, borders and animation remain each
surface's own decision.

> Share semantics, never pixels.

- Status: **FROZEN** (v1)
- Reference implementation: `crates/leveler-tui` (this contract is frozen from its
  real behavior)
- Executable contract: `testdata/execution_presentation/v1/*.json` plus each
  surface's conformance test
- Chinese version: [`EXECUTION_PRESENTATION_CONTRACT.zh-CN.md`](EXECUTION_PRESENTATION_CONTRACT.zh-CN.md)

The architecture authority remains [`ARCHITECTURE.md`](ARCHITECTURE.md). If this
document conflicts with it, the architecture document wins and this document is fixed.

---

## 1. Purpose and scope

Goals:

1. Freeze the execution presentation semantics that are already verified.
2. Make all four surfaces derive a consistent `AssistantText`, `ExecutionRound`,
   tool lifecycle/status, failure truth and `FinalAnswer` from one set of runtime facts.
3. Let every renderer keep its own visual style while forbidding any of them from
   guessing semantics.

In scope:

- Execution presentation (the execution part of the conversation transcript).
- Cross-surface semantic consistency and conformance.

Out of scope:

- Task Context / Plan / Progress as separate surfaces (Contract v2).
- Thought / raw reasoning disclosure UI.
- Stages, synthesized narration, automatic collapse or de-emphasis of historical rounds.
- New palettes, new compact header copy, a TUI rewrite.

---

## 2. Semantic tree

```text
Turn
├─ AssistantText?            the model's public content
├─ ExecutionRound*           the real model_step boundary
│  ├─ Run                   one serial call or kind of work
│  ├─ Batch                 an observed concurrent burst
│  └─ ToolRow               one tool call and its true status
├─ FinalAnswer?             the answer committed by this turn
└─ TurnEnd?                 the turn terminal
```

`ExecutionRound` IS the real `model_step`:

```text
different model_step              -> different ExecutionRound
same model_step parallel batch    -> same ExecutionRound
```

The following are forbidden as round boundaries:

- stages / investigation / fix / verification phases;
- time windows;
- tool activity class (tool kind);
- assistant narration boundaries;
- any grouping a front end infers on its own.

Whether a round constitutes a "stage" is a model-semantic question; the harness does
not infer it.

---

## 3. Frozen invariants

The ids map to fixtures. Each is mechanically verifiable from
`testdata/execution_presentation/v1/C*.json` and each surface's conformance test.

### I1 `model_step` decides the round (C1)

A different `model_step` is a different `ExecutionRound`. With no assistant prose,
round 1 / round 2 / round 3 / FinalAnswer in a row is legal and normal.

### I2 A parallel batch belongs to one round (C2)

Concurrent rows from one `model_step`, with or without `parallel`, are one round.

### I3 Run / Batch are aggregations inside a round (C3)

Batch identity is **observed**: two calls share a batch only when one started while the
other was still running. Close clocks that never overlapped never share a batch. A batch
is never a separate round.

### I4 Public text is preserved verbatim (C4)

`AssistantText` comes only from the model's public `content`
(`assistant_text_delta` / `assistant_message_completed`). The text is preserved
verbatim — no rewrite, truncation or synthesis.

### I5 Raw reasoning never enters the transcript (C5)

`reasoning_delta` / CoT never enters the conversation transcript and never becomes
`AssistantText` / `FinalAnswer`. A live status may say `Thinking` / `思考中` /
`Waiting for model`; the **raw reasoning body must not be shown**. Never branch on
model name.

### I6 Never split a round by tool kind (C6)

Read, search and command calls from one real `model_step` are one round.

### I7 Status truth (C7)

The tool lifecycle status set is fixed:

```text
Running | Ok | Failed | Cancelled | Unknown
```

- `Cancelled` = stopped and the runtime confirmed the process tree is gone
  (`stop = confirmed`).
- `Unknown` = no terminal fact arrived, or a stop could not be confirmed
  (`stop = unconfirmed`).
- **`all_ok` holds only when EVERY visible ToolCall == Ok.**
  `Ok + Failed`, `Ok + Cancelled`, `Ok + Unknown` and `Cancelled + Cancelled` are
  never all-success.
- Zero failures is not all-success: cancelled and unknown calls failed at nothing,
  but they did not succeed either.

### I8 Failure truth (C8)

The UI must express real failure attribution.

Runtime metadata / execution policy notes (`[execution policy]`, `[mutation rejected]`,
`[note]`, `exit: N`, `--- stream ---`, `[timed out after Ns]`) state HOW a command ran
and must never be presented as WHY it failed. Precedence:

```text
real stderr / structured failure  >  runtime metadata
```

Tool failed ≠ tool implementation bug. The UI must not collapse any of the following
into a generic "tool error":

- command exit non-zero;
- environment missing;
- permission denied;
- model bad args;
- background child failure.

The runtime keeps writing these notes; this is a presentation-interpretation rule, not
an execution-semantics change.

### I9 FinalAnswer survives bookkeeping (C9)

An assistant message's Final/Progress classification is decided by **event order**, not
by reading the prose:

- a *work* call after a message makes it Progress (interim narration);
- the pending message the turn ends on (Completed / Answered) is the FinalAnswer;
- `update_plan` / `update_goal` / silent observation bookkeeping after it does **not**
  demote a committed FinalAnswer;
- real read / search / edit / shell work after it does demote the answer to Progress
  (the already-verified behavior is kept).

If a Completed / Answered turn committed no FinalAnswer, the terminal must read
`no_final_answer` and **must not** show a green "completed". Tool calls finishing is not
a task finishing.

### I10 Live / reconnect / replay agree (C10)

One runtime fact sequence projected through:

```text
live
reconnect snapshot
replay (durable history)
resume
```

must produce the same semantic structure. Visuals may differ; semantics may not drift.

Frozen fields:

- `UiActiveToolCall.model_step` — a reconnect restores it and never re-derives the round;
- the reconnect snapshot and the replay state the same round identity;
- a replay mints no orphan round and duplicates no answer.

### I11 Legacy round identity (C11)

`model_step = None` (old session / old protocol) keeps the existing compatibility
fallback: no crash, no broken old session, no invented round. No new heuristic is added
for the legacy protocol.

### I12 BTW isolation (C12)

A `/btw` side question stays a side surface:

- never enters the main `ExecutionRound`;
- never changes the main transcript authority;
- never changes the `FinalAnswer`;
- never changes the main Goal / Plan state;
- never mixes into the main replay.

### I13 Runtime injection is not a user message (C13)

A `ProtocolRepair` / runtime closeout:

- may still enter the model context;
- is **not** a user-authored transcript line;
- must not reappear as a user message after a resume.

The decision must use structured origin / event semantics and must **not** filter by
matching English strings.

A runtime notice (`kind = runtime_notice`) is a user-role message the runtime wrote into
the model context. In the UI it is a note, never user speech.

---

## 4. Shared projection decision

**Verdict: no new shared production projection type is introduced in this round.**

Reasons:

1. **The wire already carries every fact.** `RuntimeEvent` carries `model_step`,
   `parallel`, `ok`, `stop`, `exit_code`, `preview`, `applied_diff`,
   `UiMessage.kind`/`ordinal`/`images`; `UiSessionSnapshot.active_tools` carries
   `model_step`. There is no protocol gap.
2. **There is no second Rust consumer.** Web, Desktop and App are TypeScript,
   JavaScript and Dart. A new Rust production type would be unusable to them and would
   add an abstraction without a consumer — the anti-overengineering rule forbids it.
3. **The semantic tree already is the projection rule.** It is a pure function
   `runtime facts -> semantic tree`. Freezing it as a spec plus a language-neutral
   fixture corpus, locked by per-surface conformance tests, is more direct than a
   cross-language abstraction layer.

Consistency is therefore guaranteed by:

```text
Runtime facts (wire)
      ↓  the frozen pure function (§2-§3)
Semantic tree
      ↓  per-surface implementation + the same fixtures
TUI / Web / Desktop / App conformance tests
```

**Known architecture debt (NON-BLOCKER, to resolve in v2):** the work / bookkeeping
classification behind FinalAnswer currently has its owner in
`leveler-tui::tool_taxonomy` (`acts_on_answer` + `ActivityVisibility`). Cross-surface
consistency requires that classification to have one owner. v1 handles it by:

- freezing the classification **outcome** here;
- covering it in the fixture corpus (C4 / C5 / C9);
- requiring every surface to implement the same classification;
- hoisting it into a shared owner (a runtime stamp or a shared crate) once a third
  consumer appears, instead of copying it again.

---

## 5. Fixture corpus and conformance

Fixture directory: `testdata/execution_presentation/v1/`.

Each fixture:

```json
{
  "id": "C4",
  "title": "assistant text plus tools",
  "invariant": "…",
  "paths": { "<name>": [ step, ... ] },
  "rendered": { "contains": [...], "excludes": [...] },
  "expect": { "items": [...], "user_texts": [...], "reasoning_visible": false }
}
```

- `paths` are **real wire facts**. A step is `{"event": …}`, `{"snapshot": …}` or
  `{"history": [UiHistoryEntry, …]}`.
- When a fixture declares several paths they **must project to the same `expect`**
  (that is how C10 and C12 are locked).
- `expect` is a structured semantic tree, not a screenshot.
- `rendered` is used only where the semantic tree cannot carry the fact (C8's failure
  attribution).

Reference-implementation (TUI) conformance:

```sh
cargo test -p leveler-tui --test execution_presentation_contract
```

Each surface's own conformance (same corpus):

```sh
# Web
cd crates/leveler-web/web && npx vitest run src/lib/executionPresentation.test.ts
# Desktop
cd apps/leveler-desktop && node --test test/executionPresentation.test.mjs
# App
cd apps/leveler-mobile && flutter test test/execution_presentation_test.dart
```

`UPDATE_EXECUTION_PRESENTATION_FIXTURES=1` regenerates `expect` from the reference
implementation. The generator records what the product does; it does not decide what the
product should do. Every regeneration must be reviewed by hand.

Each surface's conformance test reads **the same JSON** and maps its own projection onto
the same semantic tree before comparing. A test-local "surface type -> semantic tree"
adapter is allowed; relaxing status, round identity or failure attribution inside that
adapter is not.

---

## 6. Per-surface obligations

| Surface | Semantic-tree source | May differ | Must agree |
| --- | --- | --- | --- |
| TUI | the reference implementation | glyphs, color, layout | every invariant |
| Web | wire facts → its own projection | cards, collapse, icons, hover | round identity, status, failure, FinalAnswer, reasoning hidden |
| Desktop | wire facts → its own projection | compact list, disclosure, native window | same; the renderer only presents |
| App | wire facts → its own projection | mobile row style, gestures | same; a reconnect must not guess the round |

Boundaries:

- The renderer only presents. `ExecutionRound` grouping, failure attribution,
  final-answer semantics and `model_step` inference must **not** enter Electron Main or
  the Desktop bridge.
- Never treat a TUI widget type as the shared domain model.
- Never put ANSI / terminal concepts on the wire.
- Never put CSS / UI labels into the Rust runtime.
- Never treat presentation strings as a protocol contract.
- Collapse is a visual behavior and must not change the underlying semantic tree.
- A surface may mark a missing capability `Deferred` / `Unsupported`; it must **not**
  fake support with wrong semantics.

---

## 7. Conformance matrix (v1 acceptance)

CI / Gate must print this matrix; each cell is only `PASS` / `DEFERRED` / `N/A` /
`FAIL`. A `PASS` requires a fixture or real evidence.

| # | invariant | TUI | Web | Desktop | App |
| --- | --- | --- | --- | --- | --- |
| C1 | multi-round | PASS | PASS | PASS | PASS |
| C2 | parallel batch | PASS | PASS | PASS | PASS |
| C3 | run/batch | PASS | PASS | PASS | PASS |
| C4 | public text | PASS | PASS | PASS | PASS |
| C5 | tool-only | PASS | PASS | PASS | PASS |
| C6 | mixed kinds | PASS | PASS | PASS | PASS |
| C7 | status truth | PASS | PASS | PASS | PASS |
| C8 | failure truth | PASS | PASS | PASS | PASS |
| C9 | final bookkeeping | PASS | PASS | PASS | PASS |
| C10 | reconnect/replay | PASS | PARTIAL | PARTIAL | DEFERRED |
| C11 | legacy | PASS | PASS | PASS | PASS |
| C12 | BTW | PASS | PASS | PASS | PASS |
| C13 | runtime injection | PASS | PASS | PASS | PASS |

`PARTIAL` is an explicitly allowed intermediate state, used only for "the semantics are
correct but the coverage is incomplete". It is never used for "looks fine".

The three readings of C10:

- TUI: live, reconnect snapshot and durable replay are all verified (its
  `replay_history` runs the real reducer).
- Web: live and the reconnect snapshot pass; durable replay is DEFERRED — the Web
  client has no `query_session_history` consumer yet, so a session opened from a
  snapshot restores its text but not its finished rounds. The conformance test asserts
  that gap explicitly.
- Desktop: live and "snapshot + history" pass; the path that rebuilds a turn from
  durable history ALONE (no snapshot) is DEFERRED, and the gap is asserted too.
- App: DEFERRED — the app consumes the live event stream and snapshots only, with no
  history consumer.

---

## 8. Evolution

- v1 unifies execution presentation only. Task Context / Plan / Progress belong to v2.
- After v1 each surface may do its own visual polish; as long as the semantic tree does
  not change, the contract need not be re-frozen.
- Any change to §2-§3 is a contract change and must also:
  1. update this document and its Chinese counterpart;
  2. update the fixture corpus;
  3. update every affected surface's conformance test;
  4. update the `execution_presentation_v1` contract in dogfood.
