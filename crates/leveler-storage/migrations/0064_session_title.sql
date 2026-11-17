-- Separate the display title from the user's original request.
--
-- Historically `sessions.goal` carried both meanings: creation wrote the
-- request there, and rename overwrote it. A rename must not destroy the
-- record of what the user asked for. `title` is the presentation value; `goal`
-- stays the verbatim request. NULL here (rather than a copy of `goal`) keeps
-- every existing row displaying exactly what it displayed before: the read
-- projection falls back to `goal` (and, for the interactive placeholder, to
-- the first user message).
ALTER TABLE sessions ADD COLUMN title TEXT;
