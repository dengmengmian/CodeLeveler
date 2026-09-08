-- Product session axes (collaboration × work profile). Defaults match balanced/goal.
ALTER TABLE sessions ADD COLUMN collaboration TEXT NOT NULL DEFAULT 'goal';
ALTER TABLE sessions ADD COLUMN work_profile TEXT NOT NULL DEFAULT 'balanced';
