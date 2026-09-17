-- Separate the row's identity from the provider's.
--
-- `model_requests.id` was the provider's own request id, used as the primary
-- key. That made a duplicate provider id a hard failure: the INSERT violates
-- the key, the writer returns a persistence error, and the turn aborts — over
-- a diagnostics row. It held only while one writer existed and every one of
-- its calls streamed a fresh id. It stopped holding the moment the advisory
-- and compaction lanes started writing too, since those calls are answered by
-- `generate` and a gateway is free to repeat an id there.
--
-- The row now carries its own generated id and keeps the provider's as an
-- attribute. Existing rows already hold the provider id in `id`, so the
-- backfill copies it across rather than inventing one or leaving a gap.
ALTER TABLE model_requests ADD COLUMN provider_request_id TEXT;

UPDATE model_requests SET provider_request_id = id WHERE provider_request_id IS NULL;
