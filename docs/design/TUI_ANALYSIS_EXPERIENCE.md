# TUI Conversation Information Model

**Status:** frozen product model · 2026-08-26 · supersedes the visible-Analysis
experiment of the same date

The conversation's information hierarchy, as shipped:

| Surface | Default presentation |
| --- | --- |
| Raw provider reasoning | **absent** — no prose, no history disclosure, no click path |
| User-facing commentary (assistant) | visible, primary narrative |
| Current tool | auto-visible: `◌ 动作 摘要 · 12s`, one compact row |
| Completed tool | auto-collapsed to its `▸` disclosure row |
| Tool detail | click the disclosure row to expand/collapse |
| Final answer | visible normally |
| Tool live output streaming | **DEFERRED** |

The guiding rule: **current work earns screen space; completed work gets out
of the way.**

## Why raw reasoning is hidden — completely

A real dogfood session showed the failure of rendering it:

```
● 分析

  The user wants me to look at the project...
  Let me investigate...

● 我来详细看一下这个项目。先看目录结构和构建清单。
```

The model's own commentary (`●` prose) already narrates the work for the
user. `reasoning_content` is the same plan in provider-internal voice —
rendering both doubles the narration. An intermediate design folded sealed
reasoning to a `▸ 分析 · N 行` disclosure row; the final model removes even
that: raw reasoning is **not conversation content, not historical
disclosure, and not click-expandable** — there is no doorway back into it.

```
RAW_REASONING_DEFAULT_VISIBLE=NO
RAW_REASONING_HISTORY_DISCLOSURE=NO
```

What remains of `ReasoningDelta`:

- `AppState.live_reasoning` — a status-line scratch feeding the thinking
  indicator/token estimate while the model works. Cleared at every segment
  boundary (tool start, assistant start, turn end, session switch). It is
  the only reader; nothing renders it, nothing persists it.
- `TranscriptItem::Analysis` / `AnalysisBlock` are **removed** — no
  invisible transcript items, no ghost hit geometry, and reasoning deltas
  no longer invalidate the conversation cache at all.
- Nothing summarizes, parses, or transforms reasoning into commentary:
  `ReasoningDelta != Commentary`, strictly.

A consequence worth naming: reasoning between two tool calls no longer
splits their ToolGroup (only real items — commentary, tools — break
adjacency). Fewer, larger groups; the same truth.

Commentary is model-directed. The UI never manufactures narration and never
forces commentary before a tool.

## Current tool activity

While a call runs it is automatically visible as one compact row:

```
◌ 执行命令  $ go test ./... -count=1 · 12s
```

- the summary is derived from the tool arguments (`tool_summary`), truncated
  by display width; control characters and newlines flatten to spaces, so a
  multi-line shell script is still a single row
- elapsed time comes from the runtime clock, not a guess
- there is no live stdout: the runtime only emits `ToolCallStarted` /
  `ToolCallCompleted`, and the UI does not pretend otherwise

On completion the group folds to its disclosure row (existing behavior).
Failures stay prominent: `✗` glyph, first error line under the folded row.

### Truthful summaries

The summary must respect the tool's actual contract:

- a `run_command` with no `program` is an invalid call the runtime refuses —
  it renders `参数无效 · 缺少 program`, never a fabricated `$ test ./...`
- `cmd` belongs to shell_command only; `run_command {"cmd": …}` gets the same
  invalid marker, not a `$ go test …` that never ran
- the `$` prefix is reserved for genuine command lines
  (`summary_is_command_line`)

## Click-first disclosure

`▸` collapsed / `▾` expanded, click to toggle — for tool groups, sub-agents,
user shells alike. Visible `Ctrl+O` hints stay removed;
Ctrl+O remains an undocumented fallback toggling the latest tool group only.

## The machine-contract invariant (recorded per the gate)

> Published tool schemas MUST express every unconditional structural
> requirement enforced by runtime validation. Runtime validation MAY enforce
> additional semantic or conditional constraints.

See [TOOL_SCHEMA_CONTRACT](TOOL_SCHEMA_CONTRACT.md) for the run_command case
and the audit.
