-- Manual reviews: durable, independent review executions and their reports.
--
-- A review is NOT a turn. It has its own id and budget, never opens a turn,
-- never settles a goal and never writes to the source session's terminal. The
-- row is the report's identity: a reopened session can render the same card
-- and the same report without re-running the reviewer.
--
-- `scope_json`, `object_json`, `findings_json` and `usage_json` are typed JSON
-- payloads owned by the review service. `status` and the timestamps are
-- columns because history filtering reads them without parsing a report.
CREATE TABLE reviews (
    id              TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    focus           TEXT NOT NULL,
    scope_json      TEXT NOT NULL,
    model           TEXT NOT NULL,
    status          TEXT NOT NULL,
    -- The reviewed object's owning session. It equals session_id for the
    -- requester's own task, and differs when the scope names another session.
    source_session_id TEXT,
    -- The delivery turn in source_session_id the review is based on.
    source_turn_id  TEXT,
    object_json     TEXT,
    findings_json   TEXT NOT NULL DEFAULT '[]',
    report          TEXT NOT NULL DEFAULT '',
    error           TEXT,
    stop_detail     TEXT,
    usage_json      TEXT NOT NULL DEFAULT '{}',
    object_changed  INTEGER NOT NULL DEFAULT 0,
    modified_source INTEGER NOT NULL DEFAULT 0,
    created_at      TEXT NOT NULL,
    finished_at     TEXT
);

CREATE INDEX idx_reviews_session ON reviews(session_id, created_at);
