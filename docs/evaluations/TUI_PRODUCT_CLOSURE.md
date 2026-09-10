# TUI Product Closure

Phase B of the Beta Product Closure. The question is not whether the widgets
work — 596 tests already say they do — but whether a person running a real
coding task can tell what the agent is doing, how far it got, what it changed,
and whether it is stuck.

## Verdict

```text
TUI_PRODUCT_CLOSURE = PASS
CORE_FREEZE         = UNCHANGED
```

Four defects found, three fixed, one recorded. No runtime behaviour changed;
every fix is in presentation.

## 1. How this was checked

The TUI renders through `ratatui`, so it can be driven headlessly: real
`RuntimeEvent`s through the real reducer, the real renderer, and a
`TestBackend` whose cell buffer is read back as text. The scenarios live in
`crates/leveler-tui/tests/product_scenarios.rs` and print frames on demand:

```
cargo test -p leveler-tui --test product_scenarios -- --ignored --nocapture
```

Every event shape is taken from runs that actually happened — the tool
argument envelopes, the call counts, the plan lengths and the failures all come
from `evals/baselines/beta-phaseA-98d21bc/` and
`evals/baselines/progressive-comparative-a1534eb/`. That mattered: the first
draft invented `apply_patch {"path": …}`, the real tool takes
`{"patch": "*** Begin Patch …"}`, and the wrong shape produced a fake defect
that evaporated once the arguments were right.

## 2. Scenarios

| # | scenario | frames read | verdict |
| --- | --- | --- | --- |
| 1 | simple edit | mid-flight, terminal | PASS |
| 2 | medium multi-file | after two edits, terminal | PASS |
| 3 | long task | post-compaction, long command, model wait | PASS |
| 4 | many reads and searches | 18 exploration calls | PASS |
| 5 | task with a plan | 9 steps, step 6 running, terminal | PASS |
| 6 | task without a plan | terminal | PASS |
| 7 | blocked task | `TurnIncomplete`, tree untouched | PASS |
| 8 | failed command | build error | PASS |
| 9 | verification failure | `TurnCompletedChecksFailed` | PASS |
| 10 | resume | reopened with prior plan, diff, history | PASS with a gap |

Plus two diagnostics: every tool kind through the activity stream, and the plan
viewport at 24, 18, 14 and 11 rows.

## 3. What already holds

**Diff truth (§9).** An edit carrying an applied diff renders with the line
numbers the change actually landed on:

```
✓ 编辑文件  src/lib.rs
  └ 1 处修改 · +4 −1
    3 │ - pub fn first_even(v: &[i32]) -> i32 {
    3 │ + pub fn first_even(v: &[i32]) -> Option<i32> {
    4 │ +     if v.is_empty() {
```

An edit whose location could not be established — `applied_diff: None` —
renders the same content with the gutter **blank**. It invents no line number,
exactly as the protocol requires.

**The success mark stays scarce (§13).** Exploration never spends `✓`. The
ladder is `◌` happening, `·` settled inside an open burst, `▸` closed history
and a click target. Only edits and passed commands earn `✓`. The classification
comes from the existing tool taxonomy, and a shell probe (`ls`, `tree`, `find`)
demotes itself to silent — no second tool-name table.

**Exploration does not drown the narrative (§14).** Eighteen consecutive reads
and greps collapse to a single row, `· 已检查代码库`, while the two edits that
followed keep full diffs.

**The plan viewport never truncates silently (§15).** At 24 rows the window
follows the running step with explicit overflow counts; at 18 it falls back to
the current step plus `… 前 7 项 · 后 4 项`; at 14 and 11 only the header
survives, and it still carries `7/12 已完成 · 当前 8/12`. The current step is
visible or counted at every height.

**The plan retires at the terminal, and is not falsified (§11).** A turn that
ends with a plan at 5 done, 1 running and 3 pending drops the live plan dock
and reports the outcome alone. It does not leave "第 6/9 项进行中" on screen,
and it does not manufacture 9/9. The reducer says why in a comment: a stale
plan on a finished turn would be two authorities arguing on one line.

**Terminals are visually distinct (§9).**

```
── ✓ 任务已完成 · 4 次工具 · 1 个文件 · 验证 ✓ ──
── ⚠ 未完成 · 任务要求与 internal/report/zero_test.go 的既有断言冲突 ──
── ⚠ 已完成 · 验证未通过 · 2 次工具 · 验证 ✗ · go build ./... 失败 ──
```

The blocked turn also pre-fills the composer with `继续`.

**Long work is legible (§21, §22, §24).** A running command names itself with
its own elapsed; a model round says `等待模型` rather than a bare spinner; a
fold prints `上下文已压缩 84 → 12 条`.

## 4. Defects

### F1 — the turn-end summary spoke two languages at once (P2, TUI)

`turn_end_summary` composed its parts from literals: `"{} files"`,
`"verify ✓"`, `"verify ✗"`, `"verify {ok}/{n}"` in English, and
`"计划 {k}/{n}"` in Chinese. Both locales therefore got a mixed line — a
Chinese session read `1 files · verify ✓`, and an English one would have read
`plan` as `计划`. `1 files` was also the wrong plural.

Fixed: six locale entries, and one file is now `1 个文件` / `1 file`.

### F2 — the long-command heartbeat was hardcoded Chinese (P2, TUI)

`CommandProgress` built `"运行 {label} · {elapsed}"` in the reducer, so an
English session was told 运行 while a command ran. Fixed through the same
table.

### F3 — a running command showed two unlabelled durations (P2, TUI)

Because the heartbeat baked its elapsed into the activity label, and the status
line then appended the turn's, the strip read:

```
⠋ 运行 cargo test --workspace · 2m 17s · 0s
```

Two adjacent durations, nothing to tell them apart. The elapsed now belongs to
the activity that owns it (`AppState::activity_elapsed_secs`), and the status
line shows that one in place of the turn's. A `clear_activity` helper keeps the
label and its clock from drifting apart at the seven places the label is
cleared.

### F4 — a resumed session does not say the tree already changed (P2, TUI, open)

Reopening a session restores the goal, the prior messages and the plan at 2/4.
The working-tree diff arrives in the same snapshot and renders nowhere on the
default screen. Nothing false is shown, and the Diff screen has it, but a user
resuming a long task cannot see from the conversation that a file is already
modified.

Not fixed. It needs new persistent chrome rather than a correction, and §59
puts closure ahead of enhancement. Recorded for Post-Beta.

## 4b. What only a real session showed

> Added after Phase B, on the same day. Two recorded sessions were replayed
> through the real bridge, reducer and renderer —
> `crates/leveler-cli/tests/real_session_replay.rs`, which writes no events of
> its own — and found two defects the hand-written scenarios could not.

### F5 — a large edit lost its filename (P2, TUI, fixed)

A tool call's arguments were bounded by cutting the **serialized** JSON at
1,200 characters and appending an ellipsis, which leaves text that no longer
parses. The interface reads those arguments to learn which file an edit
touched, so the largest edits arrived unparseable and their row fell back to a
bare `补丁`.

| | tool calls | at the bound | unparseable | degraded rows in sampled frames |
| --- | ---: | ---: | ---: | ---: |
| Before, `yq-doc-count` at `98d21bc` | 81 | 3 | 3 | 2 |
| After, same case at `3ccbe94` | 57 | 1 | 0 | 0 |

All three casualties were `apply_patch`, including both new files the session
created. `compact_json` now bounds the string **values** and re-serializes, so
the envelope stays valid and a patch keeps its `*** Add File:` header — the
bounded call in the verification run reads
`*** Begin Patch\n*** Add File: cmd/doc_count.go`.

**§4's F-numbers were partly wrong about this.** Phase B saw the same bare
`补丁`, traced it to a scenario that sent `{"path": …}` where the real tool
takes `{"patch": …}`, and dismissed it as a harness artefact. The dismissal was
right about the cause and wrong about the defect: it was real, and a synthetic
event could not have shown it, because a synthetic event carries whatever shape
its author believed.

### F6 — a blocked goal wore the success mark (P2, TUI, fixed)

Replaying the honesty run that ended `Blocked`:

```
✓ 目标收尾  受阻：Requirement conflicts with internal/report/zero_test.go…
```

`update_goal(blocked)` is a call that ran, so `ToolStatus::Ok` is the correct
runtime fact — and `status_glyph` turned that into `✓`, the success mark, on
the one row whose own text says the work could not be done. That is exactly the
mistake `23afdc8` removed from exploration, still living where a contradiction
costs most. A blocked goal now carries `⚠`; a completed one still earns `✓`.
The blocked test comes from the existing taxonomy, not a second tool-name
table.

### Not defects

A replayed frame shows `· 0s` on the status line because the turn clock is
driven by wall-time ticks that replay does not produce. And the replay stops at
the last persisted event, so it never shows the turn-end marker — that comes
from the run's outcome, which the app sends and the log does not carry.

## 5. What was deliberately not changed

A passed command shows `✓` while it is the settled tail of an open burst and
`▸` once the burst closes into clickable history. That reads like an
inconsistency and is not one: `23afdc8` chose the three-tense ladder on
purpose, and `▸` is a disclosure affordance, not a status downgrade. Left
alone.

The footer gauges (`Context`, `cache`) were the same hardcoded-literal defect
two lines from F3, so they went through the locale table in the same change
rather than being left as a visible half-fix.

## 6. Regression protection

Four new tests, all red before the fix:

| test | pins |
| --- | --- |
| `the_turn_end_summary_speaks_one_language_in_each_locale` | no cross-language leakage in either locale |
| `one_changed_file_is_not_reported_in_the_plural` | `1 file`, not `1 files` |
| `the_running_command_heartbeat_follows_the_locale` | no Chinese in an English session |
| `a_running_command_shows_one_elapsed_not_two` | exactly one duration on the strip |

Five existing tests asserted the old literals or probed for a bare `"C"` to
locate the footer. Their intent was preserved and their probes rewritten
against the locale table, so a future translation cannot break them again.

## 7. Gate

| requirement | result |
| --- | --- |
| no misleading progress | PASS |
| no hidden plan steps | PASS — windowed with explicit overflow at every height |
| no contradictory terminal UI | PASS — plan retires, outcome stands alone |
| no misleading tool success marks | PASS |
| diff presentation mechanically truthful | PASS — no invented line numbers |
| long transcript remains navigable | PASS — bursts collapse, history is a click target |

`TUI_PRODUCT_CLOSURE = PASS`. Remaining: F4.
