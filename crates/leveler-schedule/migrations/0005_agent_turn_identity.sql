-- The exact turn a handed-off agent run started.
--
-- `baseline_sequence` proves a terminal is newer than the pre-submission log,
-- but not that it belongs to this run: another client can start a later turn
-- in the same session and its terminal also satisfies `sequence > baseline`.
-- The ordinal captured from the snapshot immediately after delivery names the
-- one turn this run owns. NULL means the dispatcher could not establish that
-- identity; reconciliation must then never settle from a terminal and never
-- cancel the session's turn.

ALTER TABLE schedule_runs ADD COLUMN turn_ordinal INTEGER;
