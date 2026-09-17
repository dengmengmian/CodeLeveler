-- What kind of model call a `model_requests` row is.
--
-- Until now the table only ever held main-loop rounds: the drive loop records
-- one row per streamed round, while the summarization call behind a compaction
-- fold and the bounded advisory calls (contract derivation, the completion
-- reconciliation judge) went straight to the runtime and were never recorded
-- at all. Cost attribution and any ablation that claims "fewer summaries" or
-- "fewer advisories" could not be read from this table, because those calls
-- were invisible in it rather than merely unlabelled.
--
-- `kind` names the lane. `retry_count` on the same row still says how many
-- physical attempts stood behind that one logical call, so physical traffic is
-- `SUM(1 + retry_count)` and logical calls are `COUNT(*)`, per lane.
--
-- Rows written before this migration are all main-loop rounds by construction,
-- which is why the backfill default is 'round' and not NULL: it is a fact about
-- what the old writer could produce, not a guess.
ALTER TABLE model_requests ADD COLUMN kind TEXT NOT NULL DEFAULT 'round';

CREATE INDEX idx_model_requests_session_kind ON model_requests(session_id, kind);
