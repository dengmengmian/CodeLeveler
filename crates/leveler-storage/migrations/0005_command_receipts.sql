-- M5: persistent command receipts for at-least-once delivery idempotency.
--
-- A remote client may deliver the same command more than once (retries over an
-- unreliable link). The issuer stamps each command with a unique `command_id`;
-- admitting it here is the dedup point. Persisting the receipt (rather than an
-- in-memory set) means a duplicate that arrives AFTER a restart still does not
-- start its action a second time.
--
-- `command_id` is the PRIMARY KEY: a second admission of the same id fails the
-- uniqueness constraint (INSERT OR IGNORE affects zero rows), which is how the
-- repository detects a re-delivery.
--
-- `status` tracks the dispatch lifecycle so a re-delivery is NOT blindly treated
-- as done: a command admitted but whose `send` failed (or whose process died
-- mid-dispatch) must be retryable or surfaced, not silently swallowed.
--   'dispatching' — admitted, dispatch in flight (or the process died here)
--   'completed'   — dispatch succeeded; a re-delivery is a true duplicate
--   'failed'      — dispatch returned an error before doing anything; retryable
CREATE TABLE command_receipts (
    command_id  TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL,
    command_fingerprint TEXT NOT NULL,
    issued_at   TEXT NOT NULL,
    admitted_at TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'dispatching',
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);
