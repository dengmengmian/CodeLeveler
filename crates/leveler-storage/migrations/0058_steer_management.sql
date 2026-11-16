ALTER TABLE session_inputs ADD COLUMN origin_turn_id TEXT;
ALTER TABLE session_inputs ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;
ALTER TABLE session_inputs ADD COLUMN edit_id TEXT;
ALTER TABLE session_inputs ADD COLUMN edit_expires_ms INTEGER;
ALTER TABLE session_inputs ADD COLUMN priority INTEGER NOT NULL DEFAULT 0;

UPDATE session_inputs SET origin_turn_id = turn_id WHERE state = 'pending';
