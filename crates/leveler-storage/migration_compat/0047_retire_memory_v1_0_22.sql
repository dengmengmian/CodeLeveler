-- Memory is retired. Keep historical migration checksums intact while removing
-- its admission state; sessions, turns, messages, goals and plans are retained.
DROP TRIGGER IF EXISTS admit_fresh_turn_to_memory_inbox;
DROP TRIGGER IF EXISTS admit_fresh_turn_to_experience_inbox;
DROP TRIGGER IF EXISTS admit_continuation_to_experience_inbox;
DROP TABLE IF EXISTS memory_inbox;
DROP TABLE IF EXISTS experience_inbox;
ALTER TABLE sessions DROP COLUMN memory_enabled;
ALTER TABLE sessions DROP COLUMN user_memory_enabled;
ALTER TABLE sessions DROP COLUMN experience_memory_enabled;

-- Erase retired event bodies without deleting log positions needed by resume.
UPDATE events
SET payload = json_object('type', type, 'payload', json('{}'))
WHERE type IN (
    'memory_recalled', 'memory_changed', 'memory_change_proposed',
    'memory_review_decided', 'memory_write_started', 'memory_write_settled'
);

-- A restored goal keeps its other capabilities without a retired membership.
DELETE FROM goal_capabilities WHERE capability_id = 'memory';
