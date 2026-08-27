-- Durable semantic checkpoints for long-running goals (long-goal P3).
--
-- A checkpoint is a DERIVED projection of authoritative facts — the event
-- log stays the canonical truth. What this table adds is a durable semantic
-- boundary: "everything up to event_cursor is represented by this payload",
-- so continuation after compaction, interruption, or process restart loads
-- one checkpoint plus the recent delta instead of replaying old context.
--
-- event_cursor is a committed `events.sequence` of `session_id`, INCLUSIVE:
-- the recent delta is exactly the events with sequence > event_cursor. The
-- writer captures it only after the event flush barrier, so a cursor can
-- never point beyond durable EventLog state.
--
-- payload is versioned JSON (schema_version mirrors the events precedent
-- from 0004): a reader refuses a version newer than it understands instead
-- of guessing, and falls back to the previous valid checkpoint or to the
-- pre-checkpoint full-history path.
CREATE TABLE goal_checkpoints (
    id              TEXT PRIMARY KEY,
    goal_id         TEXT NOT NULL REFERENCES goals(id) ON DELETE CASCADE,
    session_id      TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    -- 'manual' | 'milestone' | 'context_compaction' | 'interrupted'
    reason          TEXT NOT NULL,
    event_cursor    INTEGER NOT NULL,
    schema_version  INTEGER NOT NULL,
    payload         TEXT NOT NULL,
    created_at      TEXT NOT NULL
);

-- Idempotency lives in the schema, not in application code: the same
-- semantic boundary reached again for the same reason (a repeated reaper
-- pass, a retried /recap, a re-fired milestone callback) collapses to one
-- logical checkpoint. A different cursor is a genuinely new checkpoint.
CREATE UNIQUE INDEX goal_checkpoints_dedupe
    ON goal_checkpoints(goal_id, reason, event_cursor);

-- The read the product actually performs: latest checkpoint for a goal,
-- ordered by how much of the log it represents.
CREATE INDEX goal_checkpoints_goal_idx
    ON goal_checkpoints(goal_id, event_cursor);
