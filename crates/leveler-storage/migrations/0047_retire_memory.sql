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

-- Historical events are KEPT verbatim, including the retired `memory_*` types.
-- The runtime no longer emits or interprets them, and unknown event types are
-- skipped on replay; rewriting their bodies would destroy an audit row that a
-- user's existing session already owns. The retirement removes live storage,
-- not the record of what happened.

-- A restored goal keeps its other capabilities without a retired membership.
DELETE FROM goal_capabilities WHERE capability_id = 'memory';
