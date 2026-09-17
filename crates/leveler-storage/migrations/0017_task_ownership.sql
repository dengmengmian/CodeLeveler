-- Task ownership + fencing epoch (runtime evolution plan, phase 4).
--
-- owner_epoch is the monotonic fencing token generation: 0 = never owned
-- (every pre-existing task), first acquisition = 1. owner_runtime_id is the
-- current owner's durable RuntimeId, NULL while unowned. Historical /
-- completed tasks keep epoch 0 and no owner - a migration cannot know which
-- runtime had authority, so it must not invent one. Ownership is acquired
-- explicitly (and CAS-guarded) when a task actually runs.
ALTER TABLE tasks ADD COLUMN owner_runtime_id TEXT;
ALTER TABLE tasks ADD COLUMN owner_epoch INTEGER NOT NULL DEFAULT 0;
