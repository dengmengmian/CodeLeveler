-- Existing rows have no registered check. New rows carry only a fingerprint
-- and its registration identity, never the raw command arguments.
ALTER TABLE goal_requirement_events ADD COLUMN verification TEXT;
