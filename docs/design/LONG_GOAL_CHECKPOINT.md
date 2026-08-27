# Long Goal P3 — Durable Semantic Checkpoint & Context Continuity

Status: architecture decision (Phase 0), decided before implementation.
Baseline: main = 884b40a4.

This document answers the ten Phase 0 questions, fixes the event-cursor
contract, and records the integration decisions the implementation must
follow. The invariant everything below serves:

    EventLog / runtime evidence  = canonical truth
    GoalCheckpoint               = derived durable projection

A checkpoint can summarize truth; it can never manufacture it.

## 1. The event cursor

**Cursor value: `events.sequence`, scoped by `session_id`.**

- Column `events.sequence INTEGER NOT NULL` (Rust `i64`), assigned atomically
  inside the INSERT (`COALESCE(MAX)+1`, `event_repo.rs:72-78`), gapless,
  1-based, enforced unique by `idx_events_session_sequence_unique`
  (migration 0004). `created_at` is explicitly non-authoritative for order.
- **Monotonic per session.** Not global, not per goal, not per project.
- A goal belongs to a task (`goals.task_id`); a task maps 1:1 to a session
  today (`tasks.session_id UNIQUE`, migration 0016). So every event a goal
  produced lives in exactly one session's sequence space. The checkpoint
  therefore records **both** `session_id` and `event_cursor`; the cursor is
  meaningless without its session, and storing the pair keeps the contract
  valid if tasks ever span sessions.
- **Inclusive semantics:** a checkpoint with `event_cursor = X` represents
  all committed events of its session with `sequence <= X`. The recent delta
  is `load_after(session, X)` — the existing exclusive-`>` query
  (`event_repo.rs:237-253`).
- **Committed-only:** the cursor is captured by reading
  `latest_sequence(session)` (committed `MAX(sequence)`) **after** the
  engine's event flush barrier (`EventEmitter::flush` / `PumpBarrier`,
  `recorders.rs:102-147`) has drained — every event emitted before the
  capture point is durably on disk first. A cursor can therefore never point
  beyond durable EventLog state. Crash between cursor capture and checkpoint
  INSERT loses only the checkpoint, never truth.
- Concurrent events appended after capture simply land `> X` and belong to
  the delta. No lock is needed beyond SQLite's writer serialization.

**Transcript companion: `transcript_ordinal`.** The model context is
assembled from `session_messages` ordinals, not raw events (resume loads the
message transcript, `session_context.rs:39-90`). The checkpoint additionally
records the persisted message count at capture time — the same
`through_ordinal` watermark semantics `ContextSnapshot` already uses
(`event.rs:318`): messages `[0..transcript_ordinal)` are represented; resume
context = checkpoint + messages `[transcript_ordinal..]`. `event_cursor`
remains the canonical truth boundary; `transcript_ordinal` is the context
-assembly companion, captured at the same barrier.

## 2. Phase 0 answers (spec §5)

1. **Durable cursor value:** `(session_id, events.sequence)`; see above.
2. **Monotonic per:** session (gapless, unique-indexed). Goal→task→session
   is 1:1, so per-goal ranges are well-defined within one session.
3. **Final model context assembled at:** two layers sharing primitives —
   engine pre-turn: `RawTranscript::assemble` (`session_context.rs:75`) →
   `budget_prior_messages` (`engine.rs:233`); agent loop per-round:
   `drive.rs:744` (`ModelRequest::new(model, messages.clone())`).
4. **Real compaction path exists:** yes. Agent loop end-of-round
   (`drive.rs:3408-3515`): `decide_context_action` → `summarize_for_compaction`
   (real model call, 30s cap) → `compact_messages` (head + breadcrumb +
   objective pin + bounded recent tail). Engine pre-request fold at the 24k
   chat threshold (`engine.rs:241-285`). The persisted transcript is never
   destroyed; compaction shrinks only what is resent.
5. **Resume today:** `run --resume` → `RawTranscript::load_strict` → optional
   `summarize_if_over` → `assemble` (merge latest `ContextSnapshot` event
   with post-watermark tail) → `TurnInput::Resume(prior)`. No event replay.
6. **Deterministically derivable checkpoint fields:** goal_id/objective/state
   (`goals` row), plan progress (`EvidenceLedger.plan` from last
   `EvidenceLedgerUpdated` / `PlanUpdated`), findings counts + blocking state
   (`ledger.findings`, `open_blocking_findings()`), verification state
   (`VerifyRecord` freshness, `VerificationFinished` events), child
   contributions (`SubAgentFinished.contribution: ChildResultProjection`),
   git snapshot (`head_sha` / `current_branch` / dirty via existing helpers),
   event_cursor + transcript_ordinal, modified-file summary (`TurnFinished`).
7. **Genuinely semantic fields:** `goal_summary`, phase/current-step wording,
   completed-milestone wording, known-limitation wording, `next_action`
   wording, `display_summary`. All grounded in the structured facts.
8. **Interrupted checkpoint without a model call:** yes — structured-only
   projection from durable stores; `display_summary` falls back to a
   deterministic rendering. Required, since the interruption reaper runs at
   daemon startup with no turn in flight.
9. **Goal association across sessions:** `checkpoint.goal_id` +
   `checkpoint.session_id`. The goal row already survives process death
   (P1); the checkpoint is keyed to it, not to a turn.
10. **Recap without a second truth object:** the TUI history item carries the
    `checkpoint_id` and the structured fields projected from the persisted
    checkpoint, delivered over a new `RuntimeEvent`. The TUI renders; it
    never constructs its own variant, and expansion re-presents the same
    persisted fields.

## 3. Data model & persistence

New migration `0019_goal_checkpoints.sql`:

    CREATE TABLE goal_checkpoints (
        id              TEXT PRIMARY KEY,               -- CheckpointId (uuid)
        goal_id         TEXT NOT NULL REFERENCES goals(id) ON DELETE CASCADE,
        session_id      TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
        reason          TEXT NOT NULL,                  -- manual|milestone|context_compaction|interrupted
        event_cursor    INTEGER NOT NULL,               -- inclusive, committed-only
        schema_version  INTEGER NOT NULL,
        payload         TEXT NOT NULL,                  -- serde_json GoalCheckpoint body
        created_at      TEXT NOT NULL                   -- RFC3339
    );
    CREATE UNIQUE INDEX goal_checkpoints_dedupe
        ON goal_checkpoints(goal_id, reason, event_cursor);
    CREATE INDEX goal_checkpoints_goal_idx
        ON goal_checkpoints(goal_id, event_cursor);

- **Idempotency (spec §22):** the unique index IS the dedupe rule. Same
  `(goal_id, reason, event_cursor)` → `INSERT .. ON CONFLICT DO NOTHING` +
  read-back returns the existing logical checkpoint; a different cursor is a
  new checkpoint. Safe under concurrency because SQLite enforces it, not
  application code.
- **Latest-by-goal:** `ORDER BY event_cursor DESC, created_at DESC LIMIT 1`
  — deterministic, index-backed.
- **Versioning (spec §39):** `schema_version` column mirrors the
  `EVENT_SCHEMA_VERSION` precedent (migration 0004): the reader refuses a
  version greater than it understands (fail closed → treated as no usable
  checkpoint, resume falls back to the pre-P3 path), and the payload struct
  evolves additively with `#[serde(default)]` fields.
- **Types:** payload struct `GoalCheckpoint` lives in `leveler-lifecycle`
  (`checkpoint.rs`) next to the ledger/findings vocabulary it projects;
  `leveler-storage` (which already depends on lifecycle) gets
  `goal_checkpoint_store.rs` — `GoalCheckpointStore` trait + SQLite +
  memory impls, wired into `EngineStores`. `CheckpointId` in
  `leveler-core::ids` is claimed for this (currently declared, unused —
  the in-memory conversation `CheckpointStore` in `leveler-app` is a
  different, transcript-rollback concept and is not touched).

Truth rules baked into the payload types (spec §16-§19): counts and states
are projected from the ledger/events; absent facts are explicit `Unknown` /
`Unmeasured` variants, mirroring `Verdict::Unverified`, `measured: false`,
`IncompleteNoResult` vs `CompletedNoFindings`. Findings are referenced by id
+ status counts, never copied into a new lifecycle. Git status failure →
`unknown`, never `clean`.

## 4. The one canonical builder

`leveler-engine/src/checkpoint.rs` — `CheckpointProjection`: consumes
`GoalRecord` + `EventStore` reads (last ledger/plan, `SubAgentFinished`
contributions, `TurnFinished`/verification events, `latest_sequence`) +
bounded git metadata, produces the structured `GoalCheckpoint`. The optional
semantic layer reuses the existing bounded summarization pattern
(`summarize_with_model`: hard timeout, small `max_output_tokens`, advisory
failure). Semantic failure degrades to a deterministic display summary; the
structured checkpoint persists regardless. Raw reasoning cannot enter by
construction — `ContentPart::Reasoning` is dropped at the stream boundary
(`stream.rs:215-219`) and never reaches transcript, events, or the builder;
tests still pin this with marker prose.

`/recap`, milestone, interruption, and compaction all call this one builder.
The TUI never builds its own.

## 5. Triggers

| Reason | Deterministic signal | Model call? |
|---|---|---|
| Manual | `/recap` slash command → `ClientCommand::Recap` → runtime resolves the task's current goal | yes (bounded, optional) |
| Milestone | engine window boundary where `goal_continues` is true (`finish_from_result`, `engine.rs:521-527`) — an existing, explicit predicate; plus the narrow internal `create_goal_checkpoint(goal, Milestone)` API | yes (bounded, optional) |
| ContextCompaction | the agent loop's existing `Compact` decision (`drive.rs:3454`), via a host port (below) | reuses the existing compaction summary call |
| Interrupted | the P2 reaper path, after `TurnFinished{Interrupted}` commits (`reap_after_restart` / cancel-path reap), for sessions whose task still has a running goal | **no** — structured-only |

No scheduler, no background runtime, no per-event or per-N-seconds firing.
Spam control: each trigger is already sparse; dedupe collapses repeats.

## 6. Context compaction integration (spec §30)

The agent loop cannot see storage (layering). It already takes host ports
(`EventBarrier`, installed at `turn.rs:264`). P3 adds one more:

    trait CompactionCheckpointHook (leveler-agent):
        called when the loop decides Compact, with the semantic summary text;
        returns the durable checkpoint's context block, or an error.

The engine implementation: flush the event barrier → capture
`latest_sequence` + transcript ordinal → build via the canonical builder
(structured facts + the summary the loop already produced) → persist →
return the `[GOAL CHECKPOINT]` block. The loop then folds using that block
as the breadcrumb content — i.e. the compacted context's summary IS the
durable checkpoint's projection, not a parallel ephemeral one.

**Fail-closed:** when the session has a goal and the hook errors, the fold
is skipped this round (context kept intact, advisory emitted, retried at the
next boundary). When there is no goal (plain chat) or no hook installed, the
pre-P3 behavior is preserved unchanged. Note CodeLeveler's compaction never
destroys the durable transcript; fail-closed here protects the *model's*
continuity, which is the P3 contract.

No second token policy: thresholds, budgets, tiers, and the fold algorithm
are untouched.

## 7. Resume integration (spec §33-§36)

`Engine::resume` (and the goal-continuation context path): after
`load_strict`, load the task's latest **valid** checkpoint. If present and
its `transcript_ordinal` ≤ transcript length: model context =
system/policy + `[GOAL CHECKPOINT]` block (rendered from structured fields,
a plain `Role::User` message — no provider hacks) + messages
`[transcript_ordinal..]`. Newer events/messages are authoritative over stale
checkpoint wording because they FOLLOW it in the context. If absent,
invalid, version-unknown, cursor-beyond-log, or ordinal-beyond-transcript:
fail closed to the existing full-history assembly (pre-P3 behavior), with a
diagnostic. Boundary exactness (no duplicate ≤ watermark, no gap) is pinned
by tests.

## 8. TUI Recap (spec §40-§53)

- New `TranscriptItem::GoalRecap(GoalRecapBlock { checkpoint_id, expanded,
  structured fields })` in the conversation **history** (`chunks[1]`),
  rendered `✽ 阶段回顾 · <display> / 下一步：<next>` (1-2 lines), click
  expands via the existing hit-map + `toggle_tool_group_at` mechanism into
  structured sections (phase / completed / verification / open findings /
  limitations / next), driven by checkpoint fields — never by parsing
  `display_summary`.
- Delivered by a new `RuntimeEvent::GoalRecapCreated { recap }` (schema
  regenerated via the existing `UPDATE_SCHEMAS=1` gate).
- The existing turn-end `※ 回顾` (`RecapBlock`, mined from `update_goal`
  args) is a different, pre-existing handoff hint and is left untouched, as
  are Collaboration / Plan / Current Activity / lower chrome.
- Visibility policy: Manual + Milestone → visible item; ContextCompaction →
  compact item; Interrupted → persisted, surfaced on resume as a compact
  continuation marker.
- Unknown truth in UI: unmeasured verification / unknown findings render as
  explicit absence, never `✓` / `0`.

## 9. What P3 does NOT build

No CheckpointRuntime/scheduler, no second EventLog/AgentLoop/completion or
findings ledger/context manager, no checkpoint-management UI, no automatic
semantic milestone detection, no changes to the lower TUI stack, no Browser
or release work.
