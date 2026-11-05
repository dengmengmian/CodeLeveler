-- Calendar trigger fields. `once_at`, `anchor_at` and `every_secs` remain for
-- the two original trigger kinds; these columns carry the local-clock rules.
-- `timezone` is an IANA name, never a UTC offset, so a rule keeps its wall
-- clock across DST and system timezone changes.

ALTER TABLE schedule_definitions ADD COLUMN timezone TEXT;
ALTER TABLE schedule_definitions ADD COLUMN local_time TEXT;
ALTER TABLE schedule_definitions ADD COLUMN weekdays INTEGER;
ALTER TABLE schedule_definitions ADD COLUMN cron_expr TEXT;

CREATE INDEX schedule_definitions_enabled
    ON schedule_definitions (enabled);
