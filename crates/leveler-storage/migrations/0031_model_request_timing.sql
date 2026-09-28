-- Per-round latency decomposition: where a model call's wall clock went.
--
-- `latency_ms` (0001) is the WHOLE logical call: every provider attempt plus
-- the backoff between them. That single number cannot answer the question a
-- slow run raises — was the wait connection setup, time-to-first-token, or
-- token streaming? — so a stalled provider and a long generation looked the
-- same in the durable record.
--
-- These columns decompose the SUCCESSFUL attempt, the one whose output was
-- used:
--
--   latency_ms  = retry_overhead_ms + attempt_ms
--   attempt_ms  ~= ttft_ms + stream_ms
--   ttft_ms     ~= connect_ms + provider_think_ms
--
-- `attempt_ms`, `connect_ms` and `max_event_gap_ms` are recorded whenever a
-- response was produced. `ttft_ms` is NULL only when the stream ended without
-- a single event. Rows written before this migration keep NULL in all four:
-- an absent measurement, never a measured zero. `stream_ms` is not stored —
-- it is `attempt_ms - ttft_ms`, and storing a derived value would create a
-- second copy of the same fact.
ALTER TABLE model_requests ADD COLUMN attempt_ms INTEGER;
ALTER TABLE model_requests ADD COLUMN connect_ms INTEGER;
ALTER TABLE model_requests ADD COLUMN ttft_ms INTEGER;
ALTER TABLE model_requests ADD COLUMN max_event_gap_ms INTEGER;
