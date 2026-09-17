-- CodeLeveler initial schema (spec §36).
-- SQLite, WAL mode, foreign keys enforced. Secrets (API keys, auth headers)
-- are NEVER stored (spec §36, §47.3); large tool output goes to `artifacts`.

PRAGMA foreign_keys = ON;

CREATE TABLE sessions (
    id           TEXT PRIMARY KEY,
    repository   TEXT NOT NULL,
    goal         TEXT NOT NULL,
    status       TEXT NOT NULL,
    model        TEXT NOT NULL,
    state        TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

CREATE TABLE turns (
    id           TEXT PRIMARY KEY,
    session_id   TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    ordinal      INTEGER NOT NULL,
    created_at   TEXT NOT NULL
);
CREATE INDEX idx_turns_session ON turns(session_id);

CREATE TABLE events (
    id           TEXT PRIMARY KEY,
    session_id   TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    turn_id      TEXT REFERENCES turns(id) ON DELETE SET NULL,
    sequence     INTEGER NOT NULL,
    type         TEXT NOT NULL,
    payload      TEXT NOT NULL,
    created_at   TEXT NOT NULL
);
CREATE INDEX idx_events_session_seq ON events(session_id, sequence);

CREATE TABLE artifacts (
    id           TEXT PRIMARY KEY,
    session_id   TEXT REFERENCES sessions(id) ON DELETE CASCADE,
    content_hash TEXT NOT NULL,
    media_type   TEXT NOT NULL,
    size_bytes   INTEGER NOT NULL,
    path         TEXT,
    created_at   TEXT NOT NULL
);
CREATE INDEX idx_artifacts_session ON artifacts(session_id);

CREATE TABLE model_requests (
    id             TEXT PRIMARY KEY,
    session_id     TEXT REFERENCES sessions(id) ON DELETE CASCADE,
    turn_id        TEXT REFERENCES turns(id) ON DELETE SET NULL,
    provider       TEXT NOT NULL,
    model          TEXT NOT NULL,
    input_tokens   INTEGER NOT NULL DEFAULT 0,
    output_tokens  INTEGER NOT NULL DEFAULT 0,
    finish_reason  TEXT,
    error_kind     TEXT,
    latency_ms     INTEGER,
    retry_count    INTEGER NOT NULL DEFAULT 0,
    created_at     TEXT NOT NULL
);
CREATE INDEX idx_model_requests_session ON model_requests(session_id);
