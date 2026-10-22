-- Independent memory lanes.
--
-- `memory.enabled` remains the total switch: it closes both lanes. These two
-- new columns say whether each lane keeps its own admission. They default to
-- 1, so every existing database keeps exactly the behaviour it had.
--
-- Admission must be decided in the same insert as the turn (see
-- 0032_session_memory_policy.sql), so the lane flags live on `sessions` and
-- the triggers read them. The worker also checks its lane, but the trigger is
-- what prevents a closed lane from accumulating turns that would be extracted
-- later, after the switch is reopened.
ALTER TABLE sessions ADD COLUMN user_memory_enabled INTEGER NOT NULL DEFAULT 1;
ALTER TABLE sessions ADD COLUMN experience_memory_enabled INTEGER NOT NULL DEFAULT 1;

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
      AND sessions.memory_enabled = 1
      AND sessions.user_memory_enabled = 1;
END;

DROP TRIGGER admit_fresh_turn_to_experience_inbox;
CREATE TRIGGER admit_fresh_turn_to_experience_inbox
AFTER INSERT ON turns
WHEN NEW.kind IN ('user', 'chat')
 AND NEW.payload IS NOT NULL
 AND json_type(NEW.payload, '$.initiating_message') IS NOT NULL
 AND json_type(NEW.payload, '$.initiating_message') != 'null'
BEGIN
    INSERT INTO experience_inbox (turn_id, model, created_at, updated_at)
    SELECT NEW.id, sessions.model, NEW.created_at, NEW.created_at
    FROM sessions
    WHERE sessions.id = NEW.session_id
      AND sessions.memory_enabled = 1
      AND sessions.experience_memory_enabled = 1;
END;

DROP TRIGGER admit_continuation_to_experience_inbox;
CREATE TRIGGER admit_continuation_to_experience_inbox
AFTER INSERT ON turns
WHEN NEW.kind IN ('user', 'chat')
 AND NEW.payload IS NOT NULL
 AND (json_type(NEW.payload, '$.initiating_message') IS NULL
      OR json_type(NEW.payload, '$.initiating_message') = 'null')
 AND json_type(NEW.payload, '$.objective') IS NOT NULL
 AND json_type(NEW.payload, '$.objective') != 'null'
BEGIN
    INSERT INTO experience_inbox (turn_id, model, created_at, updated_at)
    SELECT NEW.id, sessions.model, NEW.created_at, NEW.created_at
    FROM sessions
    WHERE sessions.id = NEW.session_id
      AND sessions.memory_enabled = 1
      AND sessions.experience_memory_enabled = 1;
END;
