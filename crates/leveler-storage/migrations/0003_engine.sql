-- Task-engine persistence (plan 阶段B/B1): activate the turns/events tables
-- and persist per-session execution config so resume stops guessing.
-- ALTER TABLE ADD COLUMN only — safe over live databases; old rows read the
-- defaults.

-- How the session executes. `mode`: plan | workspace_write | full_access.
-- `kind`: direct | orchestrate | parallel. `outcome` is NULL until terminal:
-- verified | completed_unverified | failed | interrupted.
ALTER TABLE sessions ADD COLUMN mode    TEXT NOT NULL DEFAULT 'workspace_write';
ALTER TABLE sessions ADD COLUMN sandbox INTEGER NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN kind    TEXT NOT NULL DEFAULT 'direct';
ALTER TABLE sessions ADD COLUMN outcome TEXT;

-- Turn boundaries. `kind`: user | chat | node | repair; `payload` carries
-- kind-specific JSON (e.g. {"node_id":…,"attempt":…}).
ALTER TABLE turns ADD COLUMN kind        TEXT NOT NULL DEFAULT 'user';
ALTER TABLE turns ADD COLUMN payload     TEXT;
ALTER TABLE turns ADD COLUMN status      TEXT NOT NULL DEFAULT 'running';
ALTER TABLE turns ADD COLUMN finished_at TEXT;

-- Messages gain turn ownership (NULL for legacy rows).
ALTER TABLE session_messages ADD COLUMN turn_id TEXT REFERENCES turns(id);
