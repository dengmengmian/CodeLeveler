# TUI Multi-Agent Product Closure

**Status:** closed · 2026-08-25 · **No runtime behavior changed**

Closes the TUI half of Multi-Agent productization: the runtime's facts are now
visible, and the visibility has been checked against real repositories rather
than only against tests.

Prior stages: [analysis](TUI_MULTI_AGENT_UX_ANALYSIS.md) ·
[Phase 1 + 2 implementation](TUI_MULTI_AGENT_UX_IMPLEMENTATION.md) ·
[UX design](MULTI_AGENT_UX.md)

## What closed

| Phase | Outcome |
| --- | --- |
| Full TUI regression baseline | 695 tests, 0 failed |
| Task Team View | +13 tests |
| Real-repository dogfood | **2 defects found that the tests did not**, fixed |
| Final | **710 tests, 0 failed** |

## Final layout

The task is the primary object. The team is a caption on it — not a dashboard,
not a second chat surface.

```
┌ header ─────────────────────────────────────────────────┐
│ repo · branch · model                                   │
├ conversation ───────────────────────────────────────────┤
│ …                                                       │
│ ✓ reviewer · 已完成 · 已审查，无阻塞问题 · read_file ✓   │
│   结果：[sub-agent reviewer] …                          │
├ status ─────────────────────────────────────────────────┤
├ AI team ────────────────────────────────────────────────┤   ← new
│ 协作完成                                                │
│ ✓ 审查 Agent  已审查，无阻塞问题                        │
├ plan ───────────────────────────────────────────────────┤
├ composer ───────────────────────────────────────────────┤
└─────────────────────────────────────────────────────────┘
```

The panel sits with the other task-level chrome (status / plan / attachments),
above the composer, capped at 5 rows.

## Data flow

```
EventLog  →  what happened, in order        →  SubAgentUpdated  →  TaskTeamView
Ledger    →  what is true now               →  QueryChild…      →  ChildAgentView.detail
```

Two paths, deliberately:

- **Counts** ride the event stream. Small, always needed, already computed.
- **Findings** are queried on demand. They are ledger facts; streaming them
  would duplicate the record and re-grow the event payloads the pipeline work
  trimmed.

Nothing in the UI parses prose. The old `count_adopted_findings(&summary)` —
which scanned a sentence written for a language model — is deleted.

## UX decisions, and why

### The count is never the headline

`"3 agents running"` is true and useless. It answers *is something happening*,
which the spinner already answers, while hiding the only question a user asks
about a delegated agent: *was that worth waiting for?*

Locked by test: `the_title_never_leads_with_a_head_count`.

### One child is not a team

The panel needs ≥2 children, or a reviewer. A panel reading "1 agent" spends a
row of terminal to say nothing the transcript does not already show. The
reviewer is the exception because an independent review is precisely the thing
a user would not otherwise know had happened.

### Waiting must explain itself

A running child shows its **purpose**, not its state:

```
○ 探索 Agent  analyzing repository structure     ← not "waiting"
```

### Zero findings is a result

```
✓ 审查 Agent  已审查，无阻塞问题
```

Not blank, not `0 findings`. The
[Reviewer boundary study](../evaluations/MA-VALUE-REVIEWER-CAPABILITY-BOUNDARY.md)
measured 15/15 control runs where nothing-to-flag was the *correct* answer. A
UI that renders the most common outcome as absence is hiding the product's
normal behaviour.

### Four contribution states, never collapsed

| State | Meaning | Rendered |
| --- | --- | --- |
| `Pending` | not finished | nothing yet |
| `NotMeasured` | finished, no projection | `contribution not measured` |
| `NothingToFlag` | finished ok, reported nothing | `reviewed, nothing to flag` |
| `Incomplete` | **did not finish** | `stopped before finishing · {n} reported` |

The last one came out of dogfood. See below.

## Dogfood findings

Two real repositories, PTY-driven against the installed binary, reviewer forced
on (`independent_review = "always"`) so the reviewer path was exercised —
an observation setting, not a product default.

| Task | Repo | Rounds | Reviewer |
| --- | --- | ---: | --- |
| A · architecture + doc change | `ripgrep` (785 MB, 10 crates) | 10 | completed, 0 findings |
| C · red-tests bug fix | `hard-httparse-multispace` | 9 | stopped mid-review |

Both rendered the panel. Both exposed defects that 708 passing tests did not.

### Defect 1 — the title contradicted the line beneath it

```
协作完成                    ← "AI team · done"
✗ 审查 Agent 未完成          ← the very next row
```

`team_panel_title()` asked *is anyone still running* and *is anything
blocking*. It never asked *did anyone fail*. A team with a failed child has
not finished.

Fixed; locked by `a_failed_child_is_not_a_finished_team`.

### Defect 2 — an interrupted reviewer read as a clean review

```
✗ reviewer · 未完成 · 已审查，无阻塞问题
```

That reviewer's real state was `INCOMPLETE_PARTIAL (stopped before it could
finish)`. Its `findings_total` was 0 **not because the code was clean but
because it never got there**.

This is the third time this project has made the same mistake — a missing
measurement read as a measured zero:

| Where | Defect |
| --- | --- |
| `eval/lib/reviewer.py` | `contribution: null` counted as 0 findings |
| `eval/lib/value.py` | same, in the profile aggregation |
| **TUI projection** | **`ok: false` with 0 findings read as "nothing to flag"** |

The first two were caught by reasoning about the data. This one needed a real
run to surface, because it only appears when a child is actually interrupted.

Fixed via `Contribution::Incomplete`; locked by
`zero_findings_from_a_failed_child_is_not_a_clean_review`.

## Remaining gaps

### 1. The team is usually a team of one — a product decision, not a UI bug

Task A was an exploration task over a 785 MB repository, exactly the shape the
[Explorer value eval](../evaluations/MA-VALUE-A-FINAL.md) showed Explorer helps
with. **No Explorer launched.** The EventLog holds one `sub_agent_started`, and
it is the harness-launched reviewer.

So in real use this panel will usually show a single row. The "AI team"
framing needs more than one role present, and spontaneous spawn is stochastic —
already measured, not a new finding.

This is trigger policy, frozen for this phase, so nothing was changed. But it
decides whether the panel earns its rows in production, and that is a call for
the owner:

- leave `auto` conservative and accept that the panel is usually one row, or
- measure whether an Explorer trigger on exploration-shaped tasks pays — which
  is a new experiment, not a UI change.

### 2. Per-finding rows still need runtime data

The Contribution Inspector shows the lifecycle breakdown and the capability
contract. It does not show per-child changed files or per-child verification,
because neither exists as structured data:

| Missing | Where it is today |
| --- | --- |
| per-child changed files | prose inside the finish summary |
| per-child verification | task-level only; no per-child record |

Rendering them means parsing prose — the defect this whole phase removed.
Options, recorded not chosen: `modified_files` as an additive field on
`SubAgentFinished`, or a `FindingsUpdated` event carrying findings only.

### 3. The dogfood harness reports FAIL on round count

`tui_drive.py` expects ≥11 rounds per project; these tasks produced 10 and 9,
so its own gate says FAIL. That gate is about matrix coverage, not about this
phase — the frames and EventLogs it captured are valid and are the evidence
above. Not fixed: out of scope, and adjusting a gate to make a run pass is the
wrong habit.

## What a new protocol variant must register

Adding `QueryChildContribution` touched six places. Four are compiler-enforced;
two are not, and one of those silently passes when you do not ask for it.

| Place | What it decides |
| --- | --- |
| `command.rs::session_id()` | which session it belongs to |
| `policy.rs` | **remote allow/deny — a security classification** |
| `bridge.rs::command_kind()` | audit label |
| `interactive.rs` | the handler |
| `schemas/*.json` | contract for non-Rust clients |
| `protocol.gen.ts` | Web client types |

The last two are guarded by `schema_export.rs`, which is behind
`#![cfg(feature = "schema")]`. Without the feature it reports **"0 tests, ok"** —
which looks like a pass and is not one. `cargo test -p leveler-client-protocol`
alone does not run it.

`QueryChildContribution` was classified **deny** for remote, matching
`QueryObservability`: a finding carries the file path it was found in and a
summary of the code. The counts a remote client needs already ride on
`SubAgentUpdated`.

## Test inventory

| Area | Tests |
| --- | ---: |
| `multi_agent.rs` — view model, inspector, team panel | 41 |
| `workbench.rs` — panel rendering, layout non-displacement | 4 |
| `contribution_query.rs` — ledger projection | 8 |
| `event_bridge.rs` — contribution/profile survive the hop | 3 |
| `leveler-tui` total | **710** |

## Method notes worth keeping

- **A PTY log cannot be grepped.** ratatui does per-cell diffing, so rendered
  text arrives split by cursor moves (`[29;37Hme[29;40Hread`). Replay it
  through pyte and read the current screen. Grepping the raw stream produced a
  confident, wrong "the panel did not render" during this very phase.
- **"0 tests, ok" is not a pass.** A feature-gated test skipped silently is a
  measurement that did not happen — the same class of error as `null` read as
  `0`, which this phase fixed for the third time.
- **Dogfood earns its cost.** 708 green tests and two contradictory screens.

## Related

- [TUI_MULTI_AGENT_UX_ANALYSIS](TUI_MULTI_AGENT_UX_ANALYSIS.md)
- [TUI_MULTI_AGENT_UX_IMPLEMENTATION](TUI_MULTI_AGENT_UX_IMPLEMENTATION.md)
- [MULTI_AGENT_UX](MULTI_AGENT_UX.md)
- [MA-VALUE-REVIEWER-CAPABILITY-BOUNDARY](../evaluations/MA-VALUE-REVIEWER-CAPABILITY-BOUNDARY.md)
