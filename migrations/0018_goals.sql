-- Durable goal identity (long-running goal closure, P1).
--
-- A goal is a long-lived intent; a turn is one execution of it. They have
-- different lifetimes, and until now only the turn had a durable record: a
-- goal existed as `is_goal = true` on a turn and as ledger state reconstructed
-- at load. A process that died mid-goal left nothing that said work was owed.
--
-- One task hosts MANY goals over time. A session stays open, the user runs a
-- second goal, and `sessions.status/outcome` — the single lifecycle
-- projection — is overwritten by it. That is correct for the session and it is
-- why goal history needs its own rows.
--
-- What this table deliberately does NOT hold: success/failure. That is
-- `sessions.outcome`, and it already has exactly one writer. `state` here
-- answers a different question — does this goal still owe work? — and a
-- settled goal points at the session for how it went.
CREATE TABLE goals (
    id          TEXT PRIMARY KEY,
    task_id     TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    -- What the user asked, verbatim. The objective the model works from can be
    -- restated between windows; this is the thing that was actually requested.
    objective   TEXT NOT NULL,
    -- `running` | `settled`. Two states on purpose.
    --
    -- There is no `interrupted`: a goal left `running` by a dead process IS
    -- the interrupted one, and task ownership (`tasks.owner_runtime_id`,
    -- `owner_epoch`) already says whether a live runtime is driving it.
    -- Writing a third state would add a second writer for a fact ownership
    -- already holds.
    state       TEXT NOT NULL,
    opened_at   TEXT NOT NULL,
    settled_at  TEXT,
    -- Work windows this goal has consumed. A goal that keeps opening windows
    -- without converging is the failure mode a lifetime bound will need, and
    -- the count has to be durable to survive the process that ran them.
    windows_run INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX goals_task_idx ON goals(task_id);
-- The query P2 exists to answer: what is still owed?
CREATE INDEX goals_state_idx ON goals(state);
