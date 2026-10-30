-- A session's model is either an explicit user choice (`/model`) or an
-- inherited default. The boolean makes that distinction durable so a global
-- default change can reach sessions that never chose a model, without silently
-- rewriting a session whose model the user picked.
--
-- Existing rows stay on the model they were running with: they predate the
-- distinction, and changing them on upgrade would be exactly the silent
-- rewrite this column exists to prevent. New rows inherit (0) unless an
-- explicit model edit pins them (1).
ALTER TABLE sessions ADD COLUMN model_pinned INTEGER NOT NULL DEFAULT 0;
UPDATE sessions SET model_pinned = 1;
