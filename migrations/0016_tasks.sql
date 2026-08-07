-- Durable task identity (runtime evolution plan, phase 1 / batch B).
--
-- A task is the engine-owned unit of work; a session stays the
-- conversation/client aggregate. This migration records identity and the
-- task↔session association ONLY — lifecycle columns (status/state/outcome)
-- remain on `sessions`, which is still the 1:1 lifecycle projection. Moving
-- them here would create a second writer for the same fact.
--
-- `session_id` is UNIQUE: today every task has exactly one primary session.
-- Multi-session tasks are a future migration, not an implicit capability.
CREATE TABLE tasks (
    id           TEXT PRIMARY KEY,
    session_id   TEXT NOT NULL UNIQUE REFERENCES sessions(id) ON DELETE CASCADE,
    created_at   TEXT NOT NULL
);

-- Deterministic backfill: every existing session gets a task whose id equals
-- the session's own id. Stable across re-runs (the migration applies once),
-- derivable without guessing, and keeps old external references resolvable:
-- a legacy session id IS its task id.
INSERT INTO tasks (id, session_id, created_at)
SELECT id, id, created_at FROM sessions;
