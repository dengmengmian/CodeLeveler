-- Per-session snapshot of the project's `memory.enabled` switch.
--
-- Admission into `memory_inbox` is a trigger on the turn insert, so the
-- decision must be readable from the same row the trigger can see. A column on
-- `sessions` keeps admission crash-safe (the turn and its admission decision
-- are still one insert) while letting the runtime express a user policy that
-- is not spelled `work_profile`.
--
-- `DEFAULT 1` is the historical behaviour: before this switch existed, every
-- non-economy session was admitted. Backfilled rows therefore keep exactly
-- what they had, and an application that never writes the column (older
-- binaries, tests) is unchanged.
ALTER TABLE sessions ADD COLUMN memory_enabled INTEGER NOT NULL DEFAULT 1;

DROP TRIGGER admit_fresh_turn_to_memory_inbox;

CREATE TRIGGER admit_fresh_turn_to_memory_inbox
AFTER INSERT ON turns
WHEN NEW.kind IN ('user', 'chat')
 AND NEW.payload IS NOT NULL
 AND json_type(NEW.payload, '$.initiating_message') IS NOT NULL
 AND json_type(NEW.payload, '$.initiating_message') != 'null'
BEGIN
    INSERT INTO memory_inbox (turn_id, model, created_at, updated_at)
    SELECT NEW.id, sessions.model, NEW.created_at, NEW.created_at
    FROM sessions
    WHERE sessions.id = NEW.session_id
      AND sessions.work_profile != 'economy'
      AND sessions.memory_enabled = 1;
END;
