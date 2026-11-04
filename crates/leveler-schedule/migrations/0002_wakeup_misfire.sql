-- OS wakeup observations, misfire policy, and the delivery/start facts a
-- conservative recovery needs to distinguish states that all used to read as
-- "unknown".
--
-- A wakeup row is one dispatch pass: it records that a dispatcher was actually
-- started (by launchd, Task Scheduler, `serve`, or a human) even when it
-- claimed nothing or was killed before settling. It is an observation of the
-- driver, never a second execution fact: the runs and occurrences remain the
-- only execution truth.
--
-- `schedule_runs.delivered_at` is set only after the runtime acknowledged the
-- command envelope; `running_at` is set when the runtime reported the
-- execution. Their presence lets lease recovery say whether the effect may
-- have started instead of collapsing every lost dispatcher into one message.

ALTER TABLE schedule_definitions
    ADD COLUMN misfire_policy TEXT NOT NULL DEFAULT 'skip';
ALTER TABLE schedule_definitions
    ADD COLUMN misfire_grace_secs INTEGER NOT NULL DEFAULT 300;

ALTER TABLE schedule_runs ADD COLUMN delivered_at TEXT;
ALTER TABLE schedule_runs ADD COLUMN running_at TEXT;

CREATE TABLE schedule_wakeups (
    id          INTEGER PRIMARY KEY,
    source      TEXT NOT NULL,
    started_at  TEXT NOT NULL,
    finished_at TEXT,
    recovered   INTEGER NOT NULL DEFAULT 0,
    missed      INTEGER NOT NULL DEFAULT 0,
    claimed     INTEGER NOT NULL DEFAULT 0,
    succeeded   INTEGER NOT NULL DEFAULT 0,
    failed      INTEGER NOT NULL DEFAULT 0,
    unknown     INTEGER NOT NULL DEFAULT 0,
    error       TEXT
);

CREATE INDEX schedule_wakeups_started ON schedule_wakeups (started_at DESC);
