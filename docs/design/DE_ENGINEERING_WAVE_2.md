# De-engineering wave 2 — the runtime stops supervising the model

Wave 1 removed the runtime's judgement of *completion*. This wave removes its
judgement of *how the model should work*: when to delegate, whether a turn
deserves a successor, what a finding means, what the user's prose asked for,
and how much context the model has earned.

The test every deletion had to fail:

> Can the runtime prove this mechanically, without understanding the user's
> intent?

```
Runtime owns mechanical truth.
The model owns semantic interpretation.
The user owns acceptance.
```

## What was deleted

| # | Responsibility | What it was | What replaced it |
|---|---|---|---|
| 1 | Delegation intelligence | `DelegationDecisionPoint`: a per-round state machine watching plan registration, counting mutating rounds, fingerprinting open steps, injecting a keep-vs-delegate offer plus one event-driven reconsideration; disposition facts persisted across windows | One static capability note, injected once at depth 0. `spawn_agent` and the whole multi-agent runtime are untouched |
| 2 | Continuation intelligence | `SupervisorPolicy` + `supervise()`: up to 32 runtime-initiated turns — `DriveGoalAgain` for a stall, `ExtendBudget` for +50%, `ExtendRoundBudget` for one more slice | A turn ends where the model stops or a hard limit stops it. `resume` is the explicit continuation |
| 3 | Finding intelligence | A six-state lifecycle with a `blocking` flag; an open blocking finding refused `update_goal(complete)` at two boundaries; `Addressed` was promoted to `Verified` by a green `cargo test` | A finding is id / source / role / kind / summary / file / symbol. Information the parent reads and acts on in its own words |
| 4 | Progress intelligence | Delegation dispositions, settlement debt, `allows_engine_continue`, `continue_streak_cap` on the ProgressLedger | Facts and resources only: rounds, spend, commands, paths, children, permission denials |
| 5 | Task-text interpretation | `TaskContract::parse` split the goal prose on `Request:` / `Constraints:` headers, guessed `accept:` commands out of it, and re-injected a `## Task contract` block | The user's words reach the model as the user wrote them. Mechanical acceptance is the project's own `verify:` commands |
| 6 | Adaptive context | `RereadPressure` → expand one tier of a ladder, `ExpansionPolicy`, `ContextBudgetState`, the restore-on-resume path, and two one-variant policy enums | One fold threshold: cross it, fold. Model re-reads are not evidence that it deserves a bigger window |
| 7 | Hidden model coaching | `max_files_per_step` defaulted to 8 — a patch touching nine files was refused | Default `0` (unlimited). A wide refactor is a wide refactor; only an explicit caller budget bounds it |
| 9 | Hidden mutation | The app turned `auto_format` on for every session: the runtime rewrote a file the model had just written, without the model asking | Deleted outright. A model that wants its code formatted runs the formatter |
| 10 | Capability taxonomy | `ChildCapability` (`repository_analysis`, `code_review`, …), `OutputContract`, `purpose`, `description` on every ChildProfile | The structural bound the runtime enforces: `read_only`. See the bug below |

Module 8 (eval knobs out of the production domain) fell out of 1 and 6: the
`delegation_timing` and `adaptive_context` ablation knobs are gone, and the
`offer_timing` project/global config that reached the first one with them.

## A bug the taxonomy was hiding

The TUI decided whether to render a child as read-only by scanning its
capability labels for `write` / `edit` / `apply_patch` / `mutation`. No label
was ever any of those words — the five labels were `repository_analysis`,
`code_review`, `implementation`, `testing`, `verification` — so
`is_read_only()` returned true for **every** profiled child, Workers included.
A semantic self-description was standing in for a structural fact the runtime
already knew. Replacing the taxonomy with `read_only` fixes it by construction.

## What did NOT change

The permission plane is Runtime Foundation and was explicitly protected:
ToolHost admission, approval, persistent permission rules, session approval,
sandbox, network permission, filesystem elevation, host-escape approval,
destructive-command detection, ownership fencing, stale-runtime protection,
durable side-effect barriers, crash/replay safety. Not one of them was touched
by this wave; the write-scope refactor that landed alongside it is a separate
body of work.

Also unchanged: the mechanical loop guard (identical call, identical result),
the all-refused streak, the absolute round ceiling, budgets, cancellation,
context compaction, the Verifier, persistence and recovery, and every
multi-agent primitive (spawn, ownership, settlement, cancellation, accounting).

## Replay safety

Deleting a durable concept is not deleting its history. Three shapes survive so
old sessions still decode and render:

- `WindowState` — the supervisor's control state, as a payload shape nothing
  reads;
- `EngineEvent::ContextExpanded` — the adaptive ladder's climb record;
- `FindingRecord`'s dropped `blocking` / `state` / `resolution_reason` — ignored
  on read rather than a decode failure, and the TUI still renders the
  "observe thrash" / "continue suppressed" stop tokens a pre-wave session wrote.

## Cost

```
Wave 2, excluding the concurrent permission work:   +1637  −9791   net −8154

Files deleted: continuation.rs   the supervisor and its continuation policy
                context_budget.rs the adaptive-context ladder
                contract.rs       the free-text task contract parser
                format.rs         post-edit auto-formatting
```

## The unmeasured candidates, deleted

Three mechanisms shipped switched off, reachable only by an eval ablation that
never ran. "We will measure it someday" is the debt that produced half of what
this wave deleted, so they went with it:

- **`auto_format`** — after an edit tool wrote a file, the runtime ran
  `gofmt` / `rustfmt` / `ruff format` on it and re-fingerprinted it so the next
  patch would not see the rewrite as an outside edit. A hidden mutation with no
  enabler. `format.rs` deleted.
- **`keep_reasoning`** — keep a streamed round's reasoning on its assistant
  message so a pass-back provider gets the chain. Unmeasured in both
  directions.
- **`prune_tool_results`** — trim the middle out of oversized stale tool
  results before folding. C2.2 measured the lossless variant reclaiming
  nothing. The whole path went: the drive branch, `prune_tool_results`,
  `reclaimable_tool_result_bytes` and the four `PRUNE_*` constants.

## Remaining suspicious complexity

Named rather than quietly kept:

- **`context_trace`** — kept, and it is not in the class above: `leveler eval`
  sets it on every run, so it is a live measurement seam rather than a
  candidate awaiting a verdict.
- **`independent_review: required`** — explicit-only and off by default, so it
  is a user capability rather than runtime judgement. Its reviewer child is now
  a plain read-only agent with no special completion authority.
- **`ProgressCaps::no_progress_rounds`** — still a runtime decision that two
  consecutive no-progress rounds end a turn. It is mechanical (all calls
  refused, or a quiet goal round), but it is a threshold, and thresholds drift
  back toward judgement.

## Not done

The unified dogfood run is deferred at the user's request until the eval tree
reorganisation settles. Until it runs, this wave is verified by the engineering
gate only: `cargo fmt --check`, `cargo clippy --workspace --all-targets
--all-features -D warnings`, and `cargo test --workspace`.
