-- 0003 left DEFAULT mode = 'workspace_write'; 0012 rewrote existing rows to
-- 'assisted' but SQLite does not change the column default. New rows that omit
-- mode still get the obsolete default. Rewrite any stragglers; insert paths must
-- also write an explicit mode (see Application::insert_session).
UPDATE sessions SET mode = 'assisted' WHERE mode = 'workspace_write';
UPDATE sessions SET mode = 'request_approval' WHERE mode = 'plan';
