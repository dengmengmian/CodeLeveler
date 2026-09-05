-- Make a model call's cost auditable from the row that recorded it.
--
-- Three facts were parsed and then dropped. The provider reports how much of
-- the prompt it served from its own prefix cache, and `TokenUsage` carries it
-- (`cached_input_tokens`) all the way to the writer — which had no column to
-- put it in. Pricing charged every input token at the uncached rate because
-- `ModelPricing` had no cached rate, so no row could say what it cost. And a
-- sub-agent's calls reached the parent only as a progress line on a channel:
-- the child's transcript sink emitted an event and kept counters in memory,
-- so a reviewer that spent half a million tokens left no row at all.
--
-- Together those made a session's cost unanswerable from storage. A Phase C
-- run recorded 6,392,866 input tokens for its parent while its reviewer child
-- spent another 486,369 that appear nowhere, and nothing anywhere said how
-- much of either was a cache hit.
--
-- NULL is load-bearing in all three columns and is not the same as zero:
--
--   cached_input_tokens  NULL = the writer did not record it (every row
--                        written before this migration). 0 = the provider
--                        reported no cache hit on this call.
--   cost_usd_micros      NULL = no pricing was configured for that model, or
--                        the row predates this migration. 0 = priced at zero.
--   agent_id             NULL = the root session's own call. A value names the
--                        sub-agent that made it.
--
-- Backfilling zeros would assert facts nobody measured, so old rows stay NULL
-- and any reconciliation over them reports incomplete rather than clean.
ALTER TABLE model_requests ADD COLUMN cached_input_tokens INTEGER;
ALTER TABLE model_requests ADD COLUMN cost_usd_micros INTEGER;
ALTER TABLE model_requests ADD COLUMN agent_id TEXT;

CREATE INDEX idx_model_requests_session_agent ON model_requests(session_id, agent_id);
