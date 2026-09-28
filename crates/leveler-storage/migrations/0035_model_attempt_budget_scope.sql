-- Attempts are durable before their progress projection. Scope attribution
-- lets recovery rebuild spend without absorbing a different goal's calls.
ALTER TABLE model_requests ADD COLUMN budget_scope TEXT;
ALTER TABLE model_requests ADD COLUMN estimated_tokens INTEGER;
CREATE INDEX idx_model_requests_budget_scope ON model_requests(session_id, budget_scope);
