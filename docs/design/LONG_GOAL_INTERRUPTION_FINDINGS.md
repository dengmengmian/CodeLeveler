# Long Goal — real interruption findings

**Status:** validated on real runs · 2026-08-26 · **No resume implemented**

Three scenarios against real repositories with a real model, one of them killed
with `kill -9` mid-work. Evidence for whether resume is worth building, and
under what conditions it would be safe.

Prior stages: [audit](LONG_GOAL_CLOSURE_ANALYSIS.md) ·
[P1 identity](LONG_GOAL_IDENTITY.md)

## What was run

Isolated `LEVELER_HOME`, headless `leveler run`, binary `aa33036a-dirty`.
The probe reads the SQLite tables directly rather than calling the discovery
layer — **the code under test must not be the instrument that reports on it.**

| # | Scenario | Repo | Action |
| --- | --- | --- | --- |
| A | interrupted | `rust-csv` (real) | read + modify README; `kill -9` at ~50 s |
| B | normal | scratch | question only, ran to completion |
| C | mixed | both | A unfinished, B and a third settled |

## Results

Final state of the goals table across all three:

```
settled  running_turns=0  | README.md 里写的是什么？          ← B, completed
running  running_turns=0  | 阅读 src 下的解析实现...           ← A, killed
settled  running_turns=0  | README.md 的第一行是什么？         ← third, completed
```

| Expectation | Result |
| --- | --- |
| A appears as unfinished after the kill | ✅ stayed `running`, owner recorded |
| B does not appear as unfinished | ✅ `settled` at completion |
| C shows only A | ✅ one `running` among three |
| A's history is not overwritten by later goals | ✅ three rows, none replaced |

## What the experiment found before it cost a model call

**The wiring was on the wrong seam.** Goal recording was attached to
`spawn_direct_goal_turn` — the interactive TUI path. `leveler run` goes through
`run_in_session`, which never touches it.

So **headless runs recorded nothing** — and headless runs are precisely the
ones that get killed unattended, which is the entire population P2 exists to
serve.

Found while checking which CLI command to use for scenario A, before spending
anything. Fixed by moving both calls to `run_in_session_with_policy`, the seam
both paths cross, and deleting the interactive copy: two writers for one fact
is worse than one writer in the wrong place.

## The limitation this validated

`driving` — "somebody is working on this right now" — is derived from whether
any turn is still marked `running`. After a crash that mark is stale until the
reaper clears it, and **the reaper only runs when a session is opened against
that database** (`create_session` calls `reap_after_restart`).

Observed directly:

```
kill A's process           → A: running, running_turns=1   (stale)
work in a different repo   → A: running, running_turns=1   (still stale)
open a session in A's repo → A: running, running_turns=0   (reaped)
```

Consequence: between the crash and the next session in that repository, A reads
as *being driven* rather than *needing attention*. It is never lost, never
mis-settled, and never claimed by the wrong runtime — but for that window the
label is wrong.

**A cross-project "what do I owe in total" view is not accurate before
reaping.** Within one repository — where a user actually returns to work — it
is accurate from the first session they open.

Not fixed here. Fixing it means either reaping more eagerly (a write, on a
path that currently only reads) or deriving `driving` from the in-process
active registry rather than the table. Both are decisions, not clean-ups.

## 1. When does interruption happen?

Three kinds, and they are not equally safe to continue:

| Kind | Workspace state | Observed |
| --- | --- | --- |
| **Killed mid-tool** | Arbitrary. An edit may be half-applied, a command may have run without its result being recorded | Scenario A: killed during exploration, before the README edit |
| **Budget/ceiling stop** | Consistent. The engine settled the turn | Already `BudgetLimited`, already resumable, already handled |
| **Process exit between windows** | Consistent. The window boundary is a settled point | The supervisor's own continuation path |

Only the first is genuinely dangerous, and only the first leaves a goal
`running` with no terminal event.

## 2. What must be known before resuming?

From the evidence, four things — and the runtime has two of them:

| Needed | Available today |
| --- | --- |
| What was being attempted | ✅ `goals.objective`, verbatim |
| How much has been spent | ✅ `windows_run`, `cumulative_rounds` |
| **Whether the workspace is consistent** | ❌ nothing records this |
| **What the last tool call actually did** | ⚠️ the event log has the call; whether it completed is not always inferable |

The two missing ones are the two that matter for safety.

## 3. Would automatic resume be safe?

**No — not for the interrupted case, on this evidence.**

A goal killed mid-tool has an unknown workspace. Resuming means handing a model
a repository in a state nobody inspected, with a transcript that ends in a tool
call whose outcome is not recorded, and asking it to continue confidently.

This project already has the finding that makes this concrete: the Multi-Agent
dogfood produced a reviewer that reported zero findings **because it was cut
off**, and the UI read that as "reviewed, nothing to flag". An agent that
cannot tell "I did not get there" from "there was nothing there" is exactly the
agent that should not silently resume into an unknown workspace.

Automatic resume would be safe for the *budget-stop* case, where the engine
settled the turn and the workspace is consistent — but that case is already
handled by the continuation supervisor and does not need a resume policy.

## 4. What confirmation would be required?

If resume is built, the user needs to see, before deciding:

- the objective, verbatim
- what changed in the workspace since the goal opened (`git status` is the
  honest source; the ledger's `modified_files` is the agent's claim)
- where the transcript stops, and whether it stops inside a tool call
- how much has already been spent

That is a review, not a confirmation dialog. "Resume? [y/N]" without those
facts is a button that transfers responsibility without transferring
information.

## 5. What evidence must be checked before continuing?

Ordered by how cheaply they falsify safety:

1. **Does the project still build?** A goal killed mid-edit may have left
   syntactically broken files. Cheapest check, strongest signal.
2. **Is the working tree what the transcript says it is?** If the last recorded
   mutation is not present, or files changed that no mutation claims, something
   else touched the repo.
3. **Does the transcript end inside a tool call?** A dangling call means the
   model's next turn starts from a lie unless the gap is acknowledged —
   `acknowledge_crash_window` already exists for exactly this.
4. **Is the goal still what the user wants?** Time has passed. The objective is
   verbatim and stale by construction.

## What this does not tell us

- **Whether users want resume at all.** Three scripted interruptions are not a
  user study. Nobody has yet been surprised by lost work in real use.
- **How often unattended kills actually happen.** The dogfood harness kills on
  purpose; production frequency is unmeasured.
- **Whether the stale-`driving` window matters.** It is wrong for a bounded
  period; whether anyone notices is unknown.

P2 ships visibility. The next honest step is to leave it in place and see
whether anyone comes back to an owed goal — not to build a resume policy for a
need that has been reasoned about rather than observed.

## Related

- [LONG_GOAL_RESUME_POLICY](LONG_GOAL_RESUME_POLICY.md) — the options, undecided
- [LONG_GOAL_IDENTITY](LONG_GOAL_IDENTITY.md)
- [TUI_MULTI_AGENT_PRODUCT_CLOSURE](TUI_MULTI_AGENT_PRODUCT_CLOSURE.md) — the
  interrupted-reviewer defect this reasoning leans on
