# Multi-Agent MA1 — Runtime Contract

Status: **contract (pre-implementation)**. Scope comes from
`docs/MULTI_AGENT_MA0_REALITY_AND_GAP_AUDIT.md` §9 with decision D1 = B.

No second lifecycle, persistence or ownership system: every fact below is an
`EngineEvent` in the existing event log, every child still runs through
`Executor`, and every resumed side effect still goes through the host.

## 1. Child lifecycle

```text
spawn_agent call            (model decision, not durable by itself)
  → admitted                (profile / scope / caps; denial is a tool result)
  → durable                 SubAgentStarted, barrier-flushed BEFORE the child runs
  → running                 activation = one in-process Executor run
  ├─ settled                SubAgentFinished  (exactly one per child id)
  └─ interrupted            SubAgentInterrupted  (process died; written by the reaper)
       → resumed            SubAgentResumed      (same id, new activation)
       → running …
       or → settled         SubAgentFinished{stop: lost}  (not resumable)
```

| State | Durable fact | Writer |
|---|---|---|
| durable | `SubAgentStarted{…, spec}` | drive (flushed) |
| interrupted | `SubAgentInterrupted{id}` | engine reaper, once per dead activation |
| resumed | `SubAgentResumed{id, attempt}` | engine turn start, for children the harness continues |
| settled | `SubAgentFinished{id, ok, outcome, stop, summary, contribution}` | drive fold, engine ghost settle, reviewer path |

`SPAWN_ACK ⇒ durable`: the tool result carrying the child id is produced only
after `SubAgentStarted` is flushed (already true; kept under test).

## 2. Typed terminal

`SubAgentFinished` gains two additive fields (serde-default; old rows replay as
`None`):

- `outcome`: `completed_with_findings | completed_no_findings |
  incomplete_partial | incomplete_no_result` — the harness's four-way reading,
  never a bool.
- `stop`: `completed | incomplete | budget | cancelled | failed | lost` — how
  the activation ended, mechanically (`incomplete` = ended on its own without
  finishing: blocked, stalled, refused).

`ok` stays (= `outcome` is a completed reading) for existing consumers. Clients
receive both fields; no client derives them from prose.

## 3. Exactly one settlement

At most one `SubAgentFinished` per `(session, child id)` is ever **written**.
A second write is refused at the log, not filtered on read.

## 4. Durable child session

A child session = durable id + spawn spec + child transcript.

- **Spec**: `SubAgentStarted` records what re-creates the activation: role,
  write scope files, pinned model, named-agent tool subset and round cap,
  background flag. A row without a spec is not resumable.
- **Transcript**: every `TranscriptSink::append` of a child is persisted as
  `SubAgentTranscriptAppended{id, messages}` on the same ordered queue as its
  tool events. Rounds are appended as assistant+tool-result pairs (existing
  drive behaviour), so the restored transcript never ends on an unanswered
  tool call. Local-only; never projected to remote clients.

## 5. Restart

1. Reaper: for every open child of a reaped session, write
   `SubAgentInterrupted` (once). No child is `running` in the log after a
   restart. `NO_OPEN_ORPHAN_AFTER_RESTART` = every open edge is either
   interrupted-and-tied-to-an-interrupted-parent or settled.
2. Next turn of that session, before the harness runs: the engine asks the
   harness which interrupted children it continues. For each:
   `SubAgentResumed{attempt}`, then the harness launches a new background
   activation with the **same id** from the restored transcript plus a
   host-authored recovery note. The rest get `SubAgentFinished{stop: lost}`.
3. A child resumed `MAX_CHILD_RESUMES = 3` times and interrupted again is
   settled `lost` instead of resumed.

Coding harness resume policy (all must hold):

| Condition | Why |
|---|---|
| spec recorded | nothing else can rebuild the activation |
| role is not `reviewer` | reviewer reruns fresh (bounded, harness-launched) |
| the turn inherits the prior epoch | a closed epoch has no parent waiting for the child |
| no finding with `source_child = id` is adopted | adoption happens only at settlement; adopted ⇒ it had settled |
| a Worker's scope can be re-claimed | stale authority is never resurrected |

Recovery note (host-authored, structured): the activation was interrupted;
the write scope is re-claimed now (Workers) or not held; tool calls recorded
after the last persisted transcript round are listed with their outcome
(finished / unknown) — inspect before repeating any of them.

Foreground children at the crash resume in the background; the parent is told
which children were resumed and that their settlements will arrive.

Side-effect invariant (unchanged): resuming never replays a tool call. Dangling
mutating calls still stop the session behind `RecoveryConfirmationRequired`.

## 6. Cancellation

| Path | Terminal |
|---|---|
| parent turn cancelled / task cancelled | child token cancelled → `stop: cancelled` |
| user cancels one child | `CancelChild{session, child}` → that child's token → `stop: cancelled` |
| wall / round / token budget | `stop: budget` |
| runtime shutdown (clean) | turn cancelled → `stop: cancelled` |
| process death | `SubAgentInterrupted` → resume or `lost` |

Cancel is terminal; activation loss is resumable. After a child's scope is
released no write admitted for that child may still land.

## 7. Accounting

- Child model calls carry `agent_id` (existing) and are priced with the
  child's own model profile; unknown pricing records no cost rather than the
  parent's.
- Reviewer model calls are persisted as they occur, not after the review.

## 8. Gates

```text
DURABLE_CHILD_IDENTITY
DURABLE_PARENT_CHILD_EDGE
SPAWN_ACK_IS_DURABLE
RUNNING_CHILD_RESTART_SAFE
NO_OPEN_ORPHAN_AFTER_RESTART
CHILD_CANCELLATION
CHILD_SETTLEMENT
SETTLEMENT_EXACTLY_ONCE
OWNERSHIP_RELEASE
CHILD_USAGE_DURABLE
```
