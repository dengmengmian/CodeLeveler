-- A bounded trace of the reviewer's tool activity, owned by the REVIEW row.
--
-- It is deliberately not written to the reviewed session's event log: a
-- review must not append to the session it is judging. The trace is diagnostic
-- evidence (tool, phase, bounded preview, error, elapsed time), never the
-- report or the finding ledger.
ALTER TABLE reviews ADD COLUMN activity_json TEXT NOT NULL DEFAULT '[]';
