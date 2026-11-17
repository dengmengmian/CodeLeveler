-- A review's formal report is the body the Lead Reviewer submitted through
-- `report_review`, not its last chat message.
--
-- `report_source` distinguishes:
--   'submitted' — the report column holds an explicit formal submission;
--   'none'      — no formal report was submitted;
--   'legacy'    — a row written before this contract, whose report column held
--                 the reviewer's final message. It is labelled, never upgraded
--                 to a formal submission.
--
-- Revision columns bind the report to real evidence. `evidence_revision` is the
-- review ledger's latest settlement count; `report_evidence_revision` is the
-- count the submitted report was based on. `report_revision` counts formal
-- submissions, so a later call is a revision rather than a second truth.
ALTER TABLE reviews ADD COLUMN report_source TEXT NOT NULL DEFAULT 'legacy';
ALTER TABLE reviews ADD COLUMN final_message TEXT NOT NULL DEFAULT '';
ALTER TABLE reviews ADD COLUMN evidence_revision INTEGER NOT NULL DEFAULT 0;
ALTER TABLE reviews ADD COLUMN report_revision INTEGER NOT NULL DEFAULT 0;
ALTER TABLE reviews ADD COLUMN report_evidence_revision INTEGER NOT NULL DEFAULT 0;
