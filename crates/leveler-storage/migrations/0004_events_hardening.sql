-- M4: harden the canonical event log.
--
-- The `events` table is the authoritative, append-only source of truth for a
-- session. `sessions.status/state/outcome` and `turns.*` are derived
-- projections written alongside for cheap querying — they can be rebuilt by
-- replaying events, but the log is the record of what happened.
--
-- 1. Enforce one event per (session, sequence). The sequence is assigned inside
--    the INSERT (COALESCE(MAX)+1); SQLite serializes writers, so this is atomic,
--    but the UNIQUE index makes a duplicate impossible even under a future
--    multi-writer setup — a racing append fails loudly instead of silently
--    overwriting order.
CREATE UNIQUE INDEX idx_events_session_sequence_unique ON events(session_id, sequence);

-- 2. Version each persisted event's payload format. Old rows default to 1.
--    Replay rejects a version it does not understand rather than guessing —
--    a newer writer's event is a hard, named error on an older reader.
ALTER TABLE events ADD COLUMN schema_version INTEGER NOT NULL DEFAULT 1;
