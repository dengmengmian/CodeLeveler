-- Continuation lineage v2 gives headless resumes a payload even though they
-- carry no new user message. Memory admission is about fresh user input, not
-- merely the presence of turn metadata.
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
      AND sessions.work_profile != 'economy';
END;
