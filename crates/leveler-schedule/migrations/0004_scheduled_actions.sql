-- Scheduled actions: typed Native and Agent execution, session/overlap
-- policy, persisted unattended grants, and the facts needed to reconcile an
-- agent run that outlives the dispatcher.
--
-- `action` already exists and now stores 'shell' | 'native' | 'agent'.
-- `command` stays for shell actions; the other kinds store an empty string.

ALTER TABLE schedule_definitions ADD COLUMN agent_goal TEXT;
ALTER TABLE schedule_definitions ADD COLUMN native_action TEXT;
ALTER TABLE schedule_definitions ADD COLUMN native_input TEXT;
ALTER TABLE schedule_definitions ADD COLUMN session_policy TEXT NOT NULL DEFAULT 'continue';
ALTER TABLE schedule_definitions ADD COLUMN overlap_policy TEXT NOT NULL DEFAULT 'skip-if-running';
ALTER TABLE schedule_definitions ADD COLUMN unattended_grant TEXT;

ALTER TABLE schedule_occurrences ADD COLUMN run_now INTEGER NOT NULL DEFAULT 0;

ALTER TABLE schedule_runs ADD COLUMN baseline_sequence INTEGER;
ALTER TABLE schedule_runs ADD COLUMN task_sequence INTEGER;
ALTER TABLE schedule_runs ADD COLUMN task_outcome TEXT;
ALTER TABLE schedule_runs ADD COLUMN provider TEXT;
ALTER TABLE schedule_runs ADD COLUMN native_outcome TEXT;
ALTER TABLE schedule_runs ADD COLUMN capability_state TEXT;
ALTER TABLE schedule_runs ADD COLUMN permission_state TEXT;
-- Usage observed in the agent session immediately before this run's goal was
-- submitted. The delta to the final totals is the run's own usage; a continue
-- session carries earlier turns that must not be attributed here.
ALTER TABLE schedule_runs ADD COLUMN usage_baseline_requests INTEGER;
ALTER TABLE schedule_runs ADD COLUMN usage_baseline_input_tokens INTEGER;
ALTER TABLE schedule_runs ADD COLUMN usage_baseline_cached_input_tokens INTEGER;
ALTER TABLE schedule_runs ADD COLUMN usage_baseline_output_tokens INTEGER;
ALTER TABLE schedule_runs ADD COLUMN usage_baseline_reasoning_tokens INTEGER;
ALTER TABLE schedule_runs ADD COLUMN usage_baseline_cost_usd_micros INTEGER;
ALTER TABLE schedule_runs ADD COLUMN usage_baseline_rows_without_cost INTEGER;

ALTER TABLE schedule_runs ADD COLUMN usage_requests INTEGER;
ALTER TABLE schedule_runs ADD COLUMN input_tokens INTEGER;
ALTER TABLE schedule_runs ADD COLUMN cached_input_tokens INTEGER;
ALTER TABLE schedule_runs ADD COLUMN output_tokens INTEGER;
ALTER TABLE schedule_runs ADD COLUMN reasoning_tokens INTEGER;
ALTER TABLE schedule_runs ADD COLUMN cost_usd_micros INTEGER;
ALTER TABLE schedule_runs ADD COLUMN usage_rows_without_cost INTEGER;
ALTER TABLE schedule_runs ADD COLUMN handed_off_at TEXT;
ALTER TABLE schedule_runs ADD COLUMN deadline_at TEXT;
ALTER TABLE schedule_runs ADD COLUMN cancel_requested_at TEXT;
ALTER TABLE schedule_runs ADD COLUMN cancel_issued_at TEXT;

ALTER TABLE schedule_wakeups ADD COLUMN skipped INTEGER NOT NULL DEFAULT 0;
ALTER TABLE schedule_wakeups ADD COLUMN submitted INTEGER NOT NULL DEFAULT 0;
ALTER TABLE schedule_wakeups ADD COLUMN handed_off INTEGER NOT NULL DEFAULT 0;
ALTER TABLE schedule_wakeups ADD COLUMN cancelled INTEGER NOT NULL DEFAULT 0;
ALTER TABLE schedule_wakeups ADD COLUMN waiting_approval INTEGER NOT NULL DEFAULT 0;
ALTER TABLE schedule_wakeups ADD COLUMN blocked INTEGER NOT NULL DEFAULT 0;

CREATE INDEX schedule_runs_active
    ON schedule_runs (definition_id, state);
