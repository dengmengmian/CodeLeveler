-- Tool exposure belongs to a goal, rather than a session-wide work profile.
-- Membership is append-only through the fenced GoalStore port. A new goal
-- has no memberships; reopening the same goal retains its loaded surface.
CREATE TABLE goal_capabilities (
    goal_id TEXT NOT NULL REFERENCES goals(id) ON DELETE CASCADE,
    capability_id TEXT NOT NULL,
    PRIMARY KEY (goal_id, capability_id)
);

-- Keep the legacy column so existing sessions can still be decoded. It has
-- no runtime policy meaning; every historical spelling maps to one semantics.
UPDATE sessions SET work_profile = 'single';

-- Memory policy is independently owned by memory_enabled. The former work
-- profile must neither disable admission nor redefine this explicit policy.
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
      AND sessions.memory_enabled = 1;
END;
