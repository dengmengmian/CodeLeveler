-- Global scheduled automation.
--
-- Definitions and their occurrence/run history are user-global: a schedule can
-- exist before any project is open and is visible across projects. The
-- workspace column names the runtime source that executes it; session/turn
-- state stays in that source's own database.

CREATE TABLE schedule_definitions (
    id              TEXT PRIMARY KEY NOT NULL,
    name            TEXT NOT NULL,
    workspace       TEXT NOT NULL,
    action          TEXT NOT NULL,
    command         TEXT NOT NULL,
    trigger_kind    TEXT NOT NULL,
    once_at         TEXT,
    anchor_at       TEXT,
    every_secs      INTEGER,
    permission_mode TEXT NOT NULL,
    model           TEXT,
    timeout_secs    INTEGER NOT NULL,
    session_id      TEXT,
    enabled         INTEGER NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

CREATE TABLE schedule_occurrences (
    id               TEXT PRIMARY KEY NOT NULL,
    definition_id    TEXT NOT NULL REFERENCES schedule_definitions(id) ON DELETE CASCADE,
    scheduled_for    TEXT NOT NULL,
    state            TEXT NOT NULL,
    claimed_at       TEXT,
    lease_expires_at TEXT,
    created_at       TEXT NOT NULL,
    UNIQUE (definition_id, scheduled_for)
);

CREATE INDEX schedule_occurrences_due
    ON schedule_occurrences (state, scheduled_for);

CREATE TABLE schedule_runs (
    id            TEXT PRIMARY KEY NOT NULL,
    occurrence_id TEXT NOT NULL UNIQUE REFERENCES schedule_occurrences(id) ON DELETE CASCADE,
    definition_id TEXT NOT NULL REFERENCES schedule_definitions(id) ON DELETE CASCADE,
    state         TEXT NOT NULL,
    session_id    TEXT,
    execution_id  TEXT,
    exit_code     INTEGER,
    error         TEXT,
    started_at    TEXT NOT NULL,
    finished_at   TEXT
);

CREATE INDEX schedule_runs_definition
    ON schedule_runs (definition_id, started_at DESC);
