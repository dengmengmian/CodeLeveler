-- Durable admission and work ownership for asynchronous semantic-memory
-- consolidation.  The initiating user message remains authoritative in
-- turns.payload; this table stores only scheduling state and a turn FK.
--
-- Admission is a trigger on the turn insert, not a later application write:
-- after a fresh turn is acknowledged there is no crash window in which the
-- message exists but its memory work does not.  Resumed user/chat turns have
-- no payload, while internal turns have another kind, so neither is admitted.
CREATE TABLE memory_inbox (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    turn_id             TEXT NOT NULL UNIQUE REFERENCES turns(id) ON DELETE CASCADE,
    model               TEXT NOT NULL,
    status              TEXT NOT NULL DEFAULT 'pending'
                        CHECK (status IN ('pending', 'processing', 'failed_retryable', 'processed')),
    attempt             INTEGER NOT NULL DEFAULT 0 CHECK (attempt >= 0),
    next_attempt_at     TEXT,
    processing_boot_id  TEXT,
    -- Opaque, runtime-validated consolidation result. Once present, retries
    -- replay this exact result instead of asking the model again.
    result_json         TEXT,
    last_error          TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    processed_at        TEXT,
    CHECK ((status = 'processing') = (processing_boot_id IS NOT NULL)),
    CHECK ((status = 'processed') = (processed_at IS NOT NULL))
);

CREATE INDEX idx_memory_inbox_ready
    ON memory_inbox(status, next_attempt_at, id);
CREATE INDEX idx_memory_inbox_processing_boot
    ON memory_inbox(processing_boot_id)
    WHERE status = 'processing';
CREATE INDEX idx_memory_inbox_processed
    ON memory_inbox(processed_at)
    WHERE status = 'processed';

CREATE TRIGGER admit_fresh_turn_to_memory_inbox
AFTER INSERT ON turns
WHEN NEW.kind IN ('user', 'chat') AND NEW.payload IS NOT NULL
BEGIN
    INSERT INTO memory_inbox (turn_id, model, created_at, updated_at)
    SELECT NEW.id, sessions.model, NEW.created_at, NEW.created_at
    FROM sessions
    WHERE sessions.id = NEW.session_id
      AND sessions.work_profile != 'economy';
END;
