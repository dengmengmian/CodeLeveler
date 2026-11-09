-- Structured user intent over the goal lifecycle.
--
-- The runtime used to answer "did the user pause or continue?" by scanning the
-- user's text for a fixed phrase table. That made natural language the control
-- authority and kept the vocabulary forever incomplete. The model now states
-- its reading through a validated declaration; this table is the ordered,
-- replayable record of what was accepted and on which user span it was based.
--
-- Current authorization state stays on `goals` (`user_paused_at` and
-- `pause_boundary`); this log is history, not a second authority. A row is
-- appended in the same transaction that applies its state, so replay can never
-- see an accepted intent whose effect is missing (or vice versa).
ALTER TABLE goals ADD COLUMN pause_boundary TEXT;

CREATE TABLE goal_intents (
    goal_id       TEXT NOT NULL,
    seq           INTEGER NOT NULL,
    source_quote  TEXT NOT NULL,
    operations    TEXT NOT NULL,
    pause_in_force INTEGER NOT NULL,
    pause_boundary TEXT,
    created_at    TEXT NOT NULL,
    PRIMARY KEY (goal_id, seq),
    FOREIGN KEY (goal_id) REFERENCES goals(id) ON DELETE CASCADE
);

CREATE INDEX idx_goal_intents_goal ON goal_intents(goal_id, seq);
