-- Durable admission for evidence-bound agent-experience extraction.
--
-- This is a separate queue from `memory_inbox` because the two lanes have
-- different facts and different timing:
--
-- * user-turn memory reads the initiating message and may run as soon as the
--   turn is terminal;
-- * experience extraction reads the turn's STRUCTURED EXECUTION FACTS
--   (tool_call_started/finished, mutations, the task terminal) and needs the
--   turn to be finished before they exist.
--
-- Admission is still a trigger on the turn insert, exactly like memory_inbox:
-- after a fresh turn is acknowledged there is no crash window in which the
-- work item is missing. The worker only claims rows whose turn is terminal.
CREATE TABLE experience_inbox (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    turn_id             TEXT NOT NULL UNIQUE REFERENCES turns(id) ON DELETE CASCADE,
    model               TEXT NOT NULL,
    status              TEXT NOT NULL DEFAULT 'pending'
                        CHECK (status IN ('pending', 'processing', 'failed_retryable', 'processed')),
    attempt             INTEGER NOT NULL DEFAULT 0 CHECK (attempt >= 0),
    next_attempt_at     TEXT,
    processing_boot_id  TEXT,
    -- Opaque, runtime-validated extraction result. Once present, retries
    -- replay this exact result instead of asking the model again.
    result_json         TEXT,
    last_error          TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    processed_at        TEXT,
    CHECK ((status = 'processing') = (processing_boot_id IS NOT NULL)),
    CHECK ((status = 'processed') = (processed_at IS NOT NULL))
);

CREATE INDEX idx_experience_inbox_ready
    ON experience_inbox(status, next_attempt_at, id);
CREATE INDEX idx_experience_inbox_processing_boot
    ON experience_inbox(processing_boot_id)
    WHERE status = 'processing';
CREATE INDEX idx_experience_inbox_processed
    ON experience_inbox(processed_at)
    WHERE status = 'processed';

CREATE TRIGGER admit_fresh_turn_to_experience_inbox
AFTER INSERT ON turns
WHEN NEW.kind IN ('user', 'chat')
 AND NEW.payload IS NOT NULL
 AND json_type(NEW.payload, '$.initiating_message') IS NOT NULL
 AND json_type(NEW.payload, '$.initiating_message') != 'null'
BEGIN
    INSERT INTO experience_inbox (turn_id, model, created_at, updated_at)
    SELECT NEW.id, sessions.model, NEW.created_at, NEW.created_at
    FROM sessions
    WHERE sessions.id = NEW.session_id
      AND sessions.work_profile != 'economy'
      AND sessions.memory_enabled = 1;
END;

-- A resumed turn is still work that can produce a lesson: the continuation
-- carries the objective anchor but no new initiating message. Admitting it
-- means a crash before the task terminal does not permanently lose the
-- extraction; the interrupted turn's own row settles as skipped, and the
-- resumed turn's row is the one that can still learn from the final facts.
CREATE TRIGGER admit_continuation_to_experience_inbox
AFTER INSERT ON turns
WHEN NEW.kind IN ('user', 'chat')
 AND NEW.payload IS NOT NULL
 AND (json_type(NEW.payload, '$.initiating_message') IS NULL
      OR json_type(NEW.payload, '$.initiating_message') = 'null')
 AND json_type(NEW.payload, '$.objective') IS NOT NULL
 AND json_type(NEW.payload, '$.objective') != 'null'
BEGIN
    INSERT INTO experience_inbox (turn_id, model, created_at, updated_at)
    SELECT NEW.id, sessions.model, NEW.created_at, NEW.created_at
    FROM sessions
    WHERE sessions.id = NEW.session_id
      AND sessions.work_profile != 'economy'
      AND sessions.memory_enabled = 1;
END;
