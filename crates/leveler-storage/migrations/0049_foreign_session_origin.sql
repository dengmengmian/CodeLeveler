-- A CodeLeveler session may be created by importing another agent's session.
--
-- The imported session remains a normal native CodeLeveler session: its own
-- transcript, model, tools, goal and lifecycle. This row records only where it
-- came from, so the UI can keep the source identity visible and the history
-- tool can address the exact source file. The source file itself is never
-- copied, migrated, or modified.
CREATE TABLE foreign_session_origins (
    session_id  TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    tool        TEXT NOT NULL,
    native_id   TEXT NOT NULL,
    source_path TEXT NOT NULL,
    source_cwd  TEXT,
    imported_at TEXT NOT NULL
);

-- One foreign session may be imported more than once (deliberately: a later
-- import can continue from newer source records), so no unique constraint.
CREATE INDEX idx_foreign_session_origins_source
    ON foreign_session_origins(tool, native_id);
