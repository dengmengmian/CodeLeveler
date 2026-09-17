-- Three-tier permission profiles replace plan/workspace_write/full_access.
-- Do NOT edit already-applied migrations (sqlx checksum); rewrite data here.
-- New DBs still get DEFAULT 'workspace_write' from 0003, then this maps them.

UPDATE sessions SET mode = 'assisted' WHERE mode = 'workspace_write';
UPDATE sessions SET mode = 'request_approval' WHERE mode = 'plan';
-- full_access unchanged
