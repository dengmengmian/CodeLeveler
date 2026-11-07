-- Durable journal for user input that has been accepted by the runtime but
-- may not yet be part of the transcript.
--
-- A steer is acknowledged before the next model round can consume it, so the
-- acknowledgement must not be the only record: a crash in that window used to
-- lose the text silently. This table is the single authority for
-- "accepted but not yet delivered". A transcript row is written only when the
-- input is actually placed in the model's context, in the same transaction
-- that marks the journal row `consumed` (see
-- `MessageRepository::append_input_in_turn_owned`).
CREATE TABLE session_inputs (
    id          TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    -- Turn the input was admitted against. Nullable: a client may steer a
    -- session whose turn identity this process does not own.
    turn_id     TEXT,
    content     TEXT NOT NULL,
    -- 'pending' | 'consumed' | 'cancelled'
    state       TEXT NOT NULL,
    -- Why the input was cancelled; empty otherwise.
    reason      TEXT NOT NULL DEFAULT '',
    -- Transcript ordinal once consumed. Together with session_id this is the
    -- stable identity a client uses to deduplicate the live row against the
    -- persisted transcript.
    ordinal     INTEGER,
    created_at  TEXT NOT NULL,
    settled_at  TEXT
);

CREATE INDEX idx_session_inputs_pending
    ON session_inputs(session_id, state, created_at);
CREATE INDEX idx_session_inputs_ordinal
    ON session_inputs(session_id, ordinal);
