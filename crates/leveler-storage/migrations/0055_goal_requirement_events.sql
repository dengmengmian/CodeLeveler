-- Structured goal requirements and their change history.
--
-- The runtime used to re-read the user's prose with a hand-maintained
-- vocabulary every time it needed to know what a goal owed. That made the
-- lexicon the scope authority: every new phrasing risked silently dropping or
-- inventing a requirement. Requirements are now model declarations anchored to
-- a verbatim user span; the runtime validates identity, transition legality and
-- order, and this table is the ordered, replayable history those declarations
-- produced.
--
-- The current set is folded from these events; there is no second mutable copy.
-- Each row keeps the quote it was based on, so a later reader can audit what
-- the user actually said. `supersedes` names the requirement a `replace` event
-- retired; the retired row keeps its identity and is never reused.
CREATE TABLE goal_requirement_events (
    goal_id        TEXT NOT NULL,
    seq            INTEGER NOT NULL,
    turn_id        TEXT NOT NULL DEFAULT '',
    operation      TEXT NOT NULL,
    requirement_id TEXT NOT NULL,
    supersedes     TEXT,
    kind           TEXT,
    text           TEXT,
    target         TEXT,
    source_quote   TEXT NOT NULL,
    check_text     TEXT,
    reason         TEXT,
    created_at     TEXT NOT NULL,
    PRIMARY KEY (goal_id, seq),
    FOREIGN KEY (goal_id) REFERENCES goals(id) ON DELETE CASCADE
);

CREATE INDEX idx_goal_requirement_events_requirement
    ON goal_requirement_events(goal_id, requirement_id);
