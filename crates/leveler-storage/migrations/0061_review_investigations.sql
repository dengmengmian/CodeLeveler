-- The review's independent investigations, as runtime facts.
--
-- Each entry is a child agent this review actually launched (id, title, role,
-- state, objective, findings, elapsed). The list is empty when the reviewer
-- launched none; it is never inferred from the report. It belongs to the
-- REVIEW row, not to the reviewed session.
ALTER TABLE reviews ADD COLUMN investigations_json TEXT NOT NULL DEFAULT '[]';
