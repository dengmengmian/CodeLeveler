-- Task status and verification status are orthogonal facts. `outcome` says how
-- the run ENDED (completed | blocked | budget_limited | failed | interrupted;
-- legacy rows may still hold verified | completed_unverified, which read as
-- completed). `verification` says what the project's own checks reported over
-- the final tree: passed | failed | not_run | unavailable. NULL on rows
-- written before this column existed.
ALTER TABLE sessions ADD COLUMN verification TEXT;
