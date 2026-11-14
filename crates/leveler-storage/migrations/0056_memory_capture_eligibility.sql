-- Durable automatic-capture eligibility for one settled turn.
--
-- The decision is written on the session database when the turn settles, so a
-- later resume cannot treat a paused or globally disabled turn as a capture
-- gap. The memory store is not opened to record this: a closed pipeline must
-- not create a store just to remember that it was closed.
--
-- Absence means the turn predates this ledger. Those turns are not eligible
-- for automatic backfill. A capture job that was already queued remains the
-- recovery path for work that had started.
CREATE TABLE memory_capture_eligibility (
    turn_id TEXT PRIMARY KEY REFERENCES turns(id) ON DELETE CASCADE,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    disposition TEXT NOT NULL CHECK (disposition IN ('eligible', 'ineligible')),
    decided_at TEXT NOT NULL
);

CREATE INDEX idx_memory_capture_eligibility_session
    ON memory_capture_eligibility(session_id);
