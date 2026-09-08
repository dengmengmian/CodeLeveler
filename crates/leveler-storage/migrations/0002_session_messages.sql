-- Persisted conversation transcript for resumable sessions (Phase 3).
-- Each row is one serialized unified `Message`; ordering is by `ordinal`.

CREATE TABLE session_messages (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    ordinal     INTEGER NOT NULL,
    payload     TEXT NOT NULL,
    created_at  TEXT NOT NULL
);
CREATE INDEX idx_session_messages ON session_messages(session_id, ordinal);
