# Multi-Agent MA1 — Runtime Contract

Status: **implemented — see §9 Result**. Scope comes from
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
  **Not implemented (G7b, residual §9.4).**

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

---

## 9. Result

Base `ba15ab9` (MA0) → MA1 head `35f81e4`. Every change below went red first
for the missing behaviour, then green.

### 9.1 Gap → change

| Gap | Change | Commit | Proof |
|---|---|---|---|
| G2 typed terminal | `SubAgentFinished{outcome, stop}` from every writer (drive, reviewer, restart ghost, turn-end ghost); client protocol, schemas, `protocol.gen.ts`, CLI jsonl | `49f136f` | `multi_agent_test` child_completed_* / budget_limited_* / failed_child_*; `ma_restart_truth_test` ghost tests; `minimal_harness` cancelled-turn ghost; `event_bridge` / `render` tests |
| G3 exactly one terminal | `EventLog` refuses a second terminal (`DuplicateChildSettlement`), durable or same burst | `c5bfa0c` | `log::a_second_terminal_for_one_child_is_refused_at_write` |
| G10 child session | `SubAgentStarted.spec`; child sink → `SubAgentTranscriptAppended` on the barrier queue | `df272e2` | `a_spawned_child_records_its_spec_and_its_own_transcript` |
| G10 resume + G1 | `SubAgentInterrupted` (reaper + turn start), `LostChildVoice::continues`, `SubAgentResumed{attempt}`, `MAX_CHILD_RESUMES=3`; harness rebuilds the child and relaunches it with the same id, recovery note persisted; observability lists interrupted | `9dccb46` | `an_interrupted_child_is_resumed_with_the_same_identity_and_settles_once`, `a_resumed_worker_writes_inside_its_reclaimed_scope` (mutation-checked: fails without re-claim), `a_child_interrupted_too_many_times_settles_as_lost`, `an_interrupted_reviewer_is_not_resumed`, `the_restart_reaper_marks_an_open_child_interrupted`, `an_interrupted_child_is_not_listed_as_running` |
| G4a lost child with no outstanding entry | lost children from the engine are named to the parent even without an outstanding entry | `9dccb46` | covered by the resume/lost flow above |
| G4b terminal before transcript | barrier flush before the settlement notice and before a foreground spawn result | `1eec7c9` | `a_background_settlement_is_durable_before_its_notice`, `a_foreground_settlement_is_durable_before_its_result` |
| G5/G6 error exit | children keep their own token; an error exit cancels, waits, settles (`stop: cancelled`), releases after the child stopped | `2a1fbdd` | `a_run_ending_in_error_stops_and_settles_its_background_children` |
| G7a pinned-model pricing | child on another model is priced from that model's profile; unreadable pricing records no cost | `9c74171` | `a_child_on_a_pinned_model_is_billed_at_that_models_price` |
| G8 cancel one child | `SteeringSource::child_started/child_ended`; `ClientCommand::CancelChild`; remote-allowed | `41af8ab` | `a_host_can_cancel_one_child_without_cancelling_its_parent`, `child_cancel_tests`, protocol roundtrip, remote policy table |
| (found in dogfood) restart note prose | both restart notes used broken string continuations | `35f81e4` | `restart_notes_read_as_clean_prose` |

### 9.2 Real-provider acceptance

`target/release/leveler 0.2.0-beta.2`, `deepseek/deepseek-v4-flash` via the
configured gateway, isolated `LEVELER_HOME`, fixture `evals/fixtures/repos/navsvc`.
The task text asks for one background child explicitly: this checks restart
lifecycle, not delegation adoption. The process was killed with `SIGKILL`
after the child had finished at least two tool calls, then resumed with
`leveler run --resume`.

| Run | Binary | Child | Durable facts (session log) | Outcome |
|---|---|---|---|---|
| Explorer | `41af8ab` | `2f2b6e36…` | started (turn A) → interrupted (turn A) → resumed attempt 1 (turn B) → finished once, `stop: completed`, 7 findings adopted; transcript rows 4 → 11; 8 child model-request rows | `SUMMARY.md` written from the child's report; parent read "Sub-agents resumed after restart" |
| Worker | `35f81e4` | `bff46e4f…` | started → interrupted → resumed attempt 1 → finished once, `stop: completed`; recovery note in the child's own transcript: scope `internal/report/NOTES.md` re-claimed, no tool call after the last saved round | `internal/report/NOTES.md` present; the only workspace change |

Both resumed runs exited 1: `leveler run` exits 0 only for `Completed`, and
the isolated fixture had no verification commands, so the outcome was
`CompletedUnverified` — reported, not a failure of the resume.

Observation carried to MA2: in the Worker run the parent called `wait_task`
with the child's id and got `unknown task` — the model confused background
tasks with children.

### 9.3 Quality

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | pass |
| `cargo clippy --workspace --all-targets --all-features -D warnings` | pass |
| `cargo test --workspace --all-features --locked --no-fail-fast` | 3853 passed, 0 failed, 20 ignored |
| web typecheck / vitest / build | pass / 181 passed / pass |
| `flutter analyze` / `flutter test` | no issues / 54 passed |
| exact main CI | recorded in the MA1 report after push |

### 9.4 Residuals (non-blocking, stated)

- **G7b** reviewer model-request rows are written when the reviewer returns;
  a process death mid-review loses that review's rows. The reviewer is never
  resumed, the loss is bounded to one review, and completed runs are exact.
- **G7c** a child's model-request record still queued in the progress channel
  at process death is lost (bounded by one drain interval).
- A resumed child's pre-crash `absorb_child_work` (rounds/commands) is not
  carried; its model spend is, through its request rows.
- Clients do not yet present `interrupted` / `resumed` or the typed terminal
  (bridge ignores the lifecycle facts): MA3.
- Dangling mutating child tool calls still stop `run --resume` behind
  `RecoveryConfirmationRequired`; a chat turn does not run crash-window
  recovery first (pre-existing, same as the parent).

### 9.5 Gates

```text
DURABLE_CHILD_IDENTITY=YES
DURABLE_PARENT_CHILD_EDGE=YES
SPAWN_ACK_IS_DURABLE=YES
RUNNING_CHILD_RESTART_SAFE=YES
NO_OPEN_ORPHAN_AFTER_RESTART=YES
CHILD_CANCELLATION=PASS
CHILD_SETTLEMENT=PASS
SETTLEMENT_EXACTLY_ONCE=YES
OWNERSHIP_RELEASE=PASS
CHILD_USAGE_DURABLE=YES (G7b/G7c bounded residuals)
```
