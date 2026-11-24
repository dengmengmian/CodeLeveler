-- Permission Model V2: a schedule definition carries the contract it was
-- created under. Existing rows are V1; their dispatched sessions must keep the
-- old semantics until the user explicitly upgrades the definition and
-- re-issues its unattended grant.
ALTER TABLE schedule_definitions
  ADD COLUMN permission_contract INTEGER NOT NULL DEFAULT 1;
