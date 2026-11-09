-- Provenance for one applied user-intent declaration: the turn whose
-- transcript holds the newest user message the `source_quote` was taken from.
--
-- The quote alone proves the words exist; the turn identity makes the raw
-- message findable after a restart. Rows written before this column existed
-- carry '' and remain readable: their authority was already exercised, and a
-- missing pointer cannot retroactively invent an authorization.
ALTER TABLE goal_intents ADD COLUMN turn_id TEXT NOT NULL DEFAULT '';

CREATE INDEX idx_goal_intents_turn ON goal_intents(turn_id);
