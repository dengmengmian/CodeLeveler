-- Startup runs migrations on a dedicated FK-disabled connection and checks
-- every child FK before publishing the normal FK-enforced pool. SQLx SQLite
-- wraps this script in one transaction: disabling FKs inside it has no effect.
DROP TRIGGER admit_fresh_turn_to_memory_inbox;
CREATE TABLE sessions_nullable (
 id TEXT PRIMARY KEY,
 repository TEXT,
 goal TEXT NOT NULL,
 status TEXT NOT NULL,
 model TEXT NOT NULL,
 state TEXT NOT NULL,
 created_at TEXT NOT NULL,
 updated_at TEXT NOT NULL,
 mode TEXT NOT NULL DEFAULT 'workspace_write',
 sandbox INTEGER NOT NULL DEFAULT 0,
 kind TEXT NOT NULL DEFAULT 'direct',
 outcome TEXT,
 collaboration TEXT NOT NULL DEFAULT 'goal',
 work_profile TEXT NOT NULL DEFAULT 'balanced',
 archived_at TEXT,
 memory_enabled INTEGER NOT NULL DEFAULT 1
);
INSERT INTO sessions_nullable SELECT id,repository,goal,status,model,state,created_at,updated_at,
 mode,sandbox,kind,outcome,collaboration,work_profile,archived_at,memory_enabled FROM sessions;
DROP TABLE sessions;
ALTER TABLE sessions_nullable RENAME TO sessions;
CREATE TRIGGER admit_fresh_turn_to_memory_inbox
AFTER INSERT ON turns
WHEN NEW.kind IN ('user', 'chat')
 AND NEW.payload IS NOT NULL
 AND json_type(NEW.payload, '$.initiating_message') IS NOT NULL
 AND json_type(NEW.payload, '$.initiating_message') != 'null'
BEGIN
 INSERT INTO memory_inbox (turn_id,model,created_at,updated_at)
 SELECT NEW.id,sessions.model,NEW.created_at,NEW.created_at FROM sessions
 WHERE sessions.id=NEW.session_id AND sessions.memory_enabled=1;
END;
