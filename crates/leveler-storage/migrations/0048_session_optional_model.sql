-- A session may exist without a model.
--
-- Deterministic execution (`!command`, a scheduled shell action) is a real
-- runtime execution that never calls a model. Requiring a model to create any
-- session made a model configuration a runtime-wide prerequisite and forced a
-- fake model reference into rows that never use one. NULL says exactly what is
-- true: this session has no model. Agent turns on such a session fail with a
-- configuration error; they never inherit a made-up model.
--
-- SQLite cannot drop NOT NULL in place, so this is the same table rebuild the
-- 0037 migration used. Startup runs migrations with foreign keys disabled; no
-- index or trigger on `sessions` currently needs recreating.
CREATE TABLE sessions_without_required_model (
    id               TEXT PRIMARY KEY,
    repository       TEXT,
    goal             TEXT NOT NULL,
    status           TEXT NOT NULL,
    model            TEXT,
    state            TEXT NOT NULL,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL,
    mode             TEXT NOT NULL DEFAULT 'workspace_write',
    sandbox          INTEGER NOT NULL DEFAULT 0,
    kind             TEXT NOT NULL DEFAULT 'direct',
    outcome          TEXT,
    collaboration    TEXT NOT NULL DEFAULT 'goal',
    work_profile     TEXT NOT NULL DEFAULT 'balanced',
    archived_at      TEXT,
    thinking         TEXT,
    pending_goal     TEXT,
    model_pinned     INTEGER NOT NULL DEFAULT 0
);

INSERT INTO sessions_without_required_model
    (id, repository, goal, status, model, state, created_at, updated_at,
     mode, sandbox, kind, outcome, collaboration, work_profile, archived_at,
     thinking, pending_goal, model_pinned)
SELECT
    id, repository, goal, status, model, state, created_at, updated_at,
    mode, sandbox, kind, outcome, collaboration, work_profile, archived_at,
    thinking, pending_goal, model_pinned
FROM sessions;

DROP TABLE sessions;
ALTER TABLE sessions_without_required_model RENAME TO sessions;
